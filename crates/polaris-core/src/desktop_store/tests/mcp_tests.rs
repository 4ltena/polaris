//! Durable MCP evidence, raw-call binding and uncertain-ACK recovery.
use super::*;
use crate::conversation_state::RawEventV2;
use crate::mcp::{
    operation::{HostOperationIdentity, McpOperation},
    results::{ProjectedPart, ReplyProjection, Source, SourceOrigin},
};
use polaris_mcp::{
    OperationBinding, OperationFailure,
    catalog::{ItemId, Kind, tool_alias},
    registration::canonical_hash,
};
use polaris_provider::{Message, ToolCall};
use serde_json::json;

fn evidence() -> (McpOperation, Vec<Message>) {
    let item = ItemId {
        server_id: "fixture".into(),
        kind: Kind::Tool,
        key: "apply".into(),
    };
    let arguments = serde_json::json!({"text":"fixture"});
    let suffix = vec![Message::assistant_with_tool_calls(
        "",
        vec![ToolCall {
            id: "call".into(),
            name: tool_alias(&item).unwrap(),
            arguments: arguments.clone(),
        }],
    )];
    let operation = McpOperation::from_binding(
        HostOperationIdentity {
            run_id: target().run_id.as_str().into(),
            attempt_id: target().attempt_id.as_str().into(),
            model_call_id: "call".into(),
            operation_id: op().as_str().into(),
            policy_revision: 7,
            expires_at_ms: 100,
        },
        "fixture-target",
        OperationBinding {
            item,
            registry_revision: 1,
            destination_hash: "a".repeat(64),
            connection_generation: "c".repeat(32),
            catalog_generation: "d".repeat(64),
            schema_hash: Some("e".repeat(64)),
            arguments_hash: canonical_hash(&arguments).unwrap(),
            selection_id: None,
            resource_uri: None,
        },
    )
    .unwrap();
    (operation, suffix)
}

fn approved() -> (PrototypeRoot, Writer, McpOperation, Vec<Message>) {
    let (root, mut writer) = setup();
    writer.apply(&start("start", 0), Some(target())).unwrap();
    let (operation, suffix) = evidence();
    let mut approval = pending();
    approval.operation = MCP_TOOL_OPERATION.into();
    approval.scope = operation.target().into();
    approval.payload_hash = operation.payload_hash().into();
    writer.publish_approval(approval, 1).unwrap();
    writer.resolve_approval(&answer(), 2).unwrap();
    (root, writer, operation, suffix)
}

fn begun() -> (PrototypeRoot, Writer) {
    let (root, mut writer, operation, suffix) = approved();
    assert_eq!(
        writer
            .consume_mcp_approval_intent(&target(), &pending().approval_id, &operation, suffix, 3)
            .unwrap(),
        IntentReceipt::NewlyPublished
    );
    (root, writer)
}

fn reply() -> ReplyProjection {
    ReplyProjection {
        source: Source {
            server: "fixture".into(),
            origin: SourceOrigin::ModelCall {
                model_call: "call".into(),
            },
            result_generation: "response-1".into(),
        },
        status: "complete",
        parts: vec![ProjectedPart::Text {
            text: "applied".into(),
            uri: None,
            mime: None,
        }],
    }
}

#[test]
fn mcp_intent_and_result_publish_raw_once_and_survive_reopen() {
    let (root, mut writer, operation, suffix) = approved();
    writer
        .consume_mcp_approval_intent(
            &target(),
            &pending().approval_id,
            &operation,
            suffix.clone(),
            3,
        )
        .unwrap();
    let intent = writer.snapshot().unwrap();
    assert_eq!(intent.raw.len(), 2);
    assert!(intent.state.approval_records[0].consumed);
    assert!(intent.state.runs[0].operations[0].result_id.is_none());
    assert_eq!(
        writer
            .consume_mcp_approval_intent(&target(), &pending().approval_id, &operation, suffix, 4)
            .unwrap(),
        IntentReceipt::AlreadyRecorded
    );
    unchanged(&intent, &writer.snapshot().unwrap());
    let projection = reply();
    let publication = writer
        .prepare_mcp_terminal(&target(), &op(), McpTerminalInput::Reply(&projection))
        .unwrap();
    unchanged(&intent, &writer.snapshot().unwrap());
    let received = writer.publish_mcp_terminal(&publication).unwrap();
    assert!(received.permits_continuation());
    assert!(received.succeeded());
    assert_eq!(received.message.tool_call_id.as_deref(), Some("call"));
    let saved = writer.snapshot().unwrap();
    assert_eq!(saved.raw.len(), 3);
    writer.publish_mcp_terminal(&publication).unwrap();
    writer.confirm_mcp_terminal(&publication).unwrap();
    unchanged(&saved, &writer.snapshot().unwrap());
    writer
        .finish(&target(), Observation::Succeeded, result())
        .unwrap();
    drop(writer);
    let mut writer = root.open(&project(), &session()).unwrap();
    assert!(
        writer
            .confirm_mcp_terminal(&publication)
            .unwrap()
            .permits_continuation()
    );
}

#[test]
fn mcp_rejects_wrong_raw_call_or_permission_before_any_publication() {
    for mutation in 0..6 {
        let (_root, mut writer, operation, mut suffix) = approved();
        let before = writer.snapshot().unwrap();
        let mut approval = pending().approval_id;
        let mut now = 3;
        match mutation {
            0 => suffix[0].tool_calls[0].name = "write".into(),
            1 => suffix[0].tool_calls[0].arguments = serde_json::json!({"text":"different"}),
            2 => suffix[0].tool_calls[0].id = "other-call".into(),
            3 => suffix.push(Message::tool_result("call", "premature")),
            4 => approval = ApprovalId::new("wrong").unwrap(),
            _ => now = 100,
        }
        assert!(
            writer
                .consume_mcp_approval_intent(&target(), &approval, &operation, suffix, now)
                .is_err()
        );
        unchanged(&before, &writer.snapshot().unwrap());
    }
}

#[test]
fn mcp_intent_marker_failures_never_split_approval_and_raw() {
    for point in ["raw_sync", "marker_rename", "directory_sync"] {
        let (_root, mut writer, operation, suffix) = approved();
        inject(Some(point));
        let failed = writer.consume_mcp_approval_intent(
            &target(),
            &pending().approval_id,
            &operation,
            suffix.clone(),
            3,
        );
        inject(None);
        assert!(failed.is_err());
        let published = writer
            .confirm_mcp_intent(&target(), &pending().approval_id, &operation, &suffix)
            .unwrap();
        assert_eq!(published, point == "directory_sync");
        let state = writer.snapshot().unwrap();
        assert_eq!(state.state.approval_records[0].consumed, published);
        assert_eq!(state.state.runs[0].operations.len(), usize::from(published));
        assert_eq!(state.raw.len(), if published { 2 } else { 1 });
    }
}

#[test]
fn mcp_intent_confirmation_checks_exact_proof_approval_and_raw_without_granting() {
    let (root, mut writer, operation, suffix) = approved();
    let before = writer.snapshot().unwrap();
    assert!(
        !writer
            .confirm_mcp_intent(&target(), &pending().approval_id, &operation, &suffix)
            .unwrap()
    );
    assert_eq!(
        serde_json::to_value(&writer.snapshot().unwrap().marker).unwrap(),
        serde_json::to_value(&before.marker).unwrap()
    );
    writer
        .consume_mcp_approval_intent(
            &target(),
            &pending().approval_id,
            &operation,
            suffix.clone(),
            3,
        )
        .unwrap();
    let committed = writer.snapshot().unwrap();
    assert!(
        writer
            .confirm_mcp_intent(&target(), &pending().approval_id, &operation, &suffix)
            .unwrap()
    );

    let mut changed = suffix.clone();
    changed[0].content = "changed original assistant text".into();
    assert!(
        writer
            .confirm_mcp_intent(&target(), &pending().approval_id, &operation, &changed)
            .is_err()
    );
    assert!(
        writer
            .confirm_mcp_intent(
                &target(),
                &ApprovalId::new("other").unwrap(),
                &operation,
                &suffix
            )
            .is_err()
    );
    let different = McpOperation::from_binding(
        operation.identity().clone(),
        "other-target",
        operation.binding().clone(),
    )
    .unwrap();
    assert!(
        writer
            .confirm_mcp_intent(&target(), &pending().approval_id, &different, &suffix)
            .is_err()
    );
    assert_eq!(
        serde_json::to_value(&writer.snapshot().unwrap().marker).unwrap(),
        serde_json::to_value(&committed.marker).unwrap()
    );
    assert_eq!(writer.snapshot().unwrap().raw.len(), 2);

    drop(writer);
    let mut writer = root.open(&project(), &session()).unwrap();
    assert_eq!(
        writer.snapshot().unwrap().state.runs[0].run.state,
        RunState::OutcomeUnknown
    );
    assert!(
        writer
            .confirm_mcp_intent(&target(), &pending().approval_id, &operation, &suffix)
            .unwrap()
    );
    assert!(
        writer
            .consume_mcp_approval_intent(
                &target(),
                &pending().approval_id,
                &operation,
                suffix.clone(),
                4
            )
            .is_err()
    );
    let projection = reply();
    let publication = writer
        .prepare_mcp_terminal(&target(), &op(), McpTerminalInput::Reply(&projection))
        .unwrap();
    assert!(writer.publish_mcp_terminal(&publication).is_err());
    assert_eq!(writer.snapshot().unwrap().raw.len(), 2);

    // A separate active run can publish a result; its completed intent is not
    // eligible for the intent-only confirmation path either.
    let (_active_root, mut writer) = begun();
    let publication = writer
        .prepare_mcp_terminal(&target(), &op(), McpTerminalInput::Reply(&projection))
        .unwrap();
    writer.publish_mcp_terminal(&publication).unwrap();
    assert!(
        writer
            .confirm_mcp_intent(&target(), &pending().approval_id, &operation, &suffix)
            .is_err()
    );
    assert_eq!(writer.snapshot().unwrap().raw.len(), 3);
}

#[test]
fn mcp_terminal_ack_readback_never_republishes_a_missing_result() {
    for point in ["raw_sync", "marker_rename", "directory_sync"] {
        let (root, mut writer) = begun();
        let projection = reply();
        let publication = writer
            .prepare_mcp_terminal(&target(), &op(), McpTerminalInput::Reply(&projection))
            .unwrap();
        inject(Some(point));
        let failed = writer.publish_mcp_terminal(&publication);
        inject(None);
        assert!(failed.is_err());
        let confirmed = writer.confirm_mcp_terminal(&publication);
        assert_eq!(confirmed.is_ok(), point == "directory_sync");
        let state = writer.snapshot().unwrap();
        assert_eq!(state.raw.len(), if confirmed.is_ok() { 3 } else { 2 });
        assert_eq!(
            state.state.runs[0].operations[0].result_id.is_some(),
            confirmed.is_ok()
        );
        drop(writer);
        let writer = root.open(&project(), &session()).unwrap();
        assert_eq!(
            writer.snapshot().unwrap().state.runs[0].run.state,
            if confirmed.is_ok() {
                RunState::Interrupted
            } else {
                RunState::OutcomeUnknown
            }
        );
    }
}

#[test]
fn mcp_confirmed_tool_error_or_unsupported_result_is_not_successful() {
    for status in ["tool_error", "unsupported_content"] {
        let (root, mut writer) = begun();
        let mut projection = reply();
        projection.status = status;
        let publication = writer
            .prepare_mcp_terminal(&target(), &op(), McpTerminalInput::Reply(&projection))
            .unwrap();
        let received = writer.publish_mcp_terminal(&publication).unwrap();
        assert!(received.permits_continuation());
        assert!(!received.succeeded());
        drop(writer);
        let mut writer = root.open(&project(), &session()).unwrap();
        assert!(
            !writer
                .confirm_mcp_terminal(&publication)
                .unwrap()
                .succeeded()
        );
    }
}

#[test]
fn mcp_known_unsent_unknown_and_input_required_remain_distinct() {
    for disposition in [
        McpDisposition::BeforeSend,
        McpDisposition::OutcomeUnknown,
        McpDisposition::InputRequired,
    ] {
        let (root, mut writer) = begun();
        let mut projection = reply();
        projection.status = "input_required";
        projection.parts = vec![ProjectedPart::InputRequired {
            request: serde_json::json!({"question":"choose"}),
        }];
        let input = match disposition {
            McpDisposition::BeforeSend => McpTerminalInput::Failure(OperationFailure::BeforeSend(
                polaris_mcp::Error::Cancelled,
            )),
            McpDisposition::OutcomeUnknown => McpTerminalInput::Failure(
                OperationFailure::OutcomeUnknown(polaris_mcp::Error::Transport),
            ),
            _ => McpTerminalInput::Reply(&projection),
        };
        let publication = writer
            .prepare_mcp_terminal(&target(), &op(), input)
            .unwrap();
        let published = writer.publish_mcp_terminal(&publication).unwrap();
        assert_eq!(published.disposition, disposition);
        assert!(!published.succeeded());
        assert_eq!(
            published.permits_continuation(),
            disposition == McpDisposition::BeforeSend
        );
        if disposition != McpDisposition::BeforeSend {
            assert!(
                writer
                    .finish(&target(), Observation::Succeeded, result())
                    .is_err()
            );
        }
        drop(writer);
        let writer = root.open(&project(), &session()).unwrap();
        let state = writer.snapshot().unwrap();
        assert_eq!(
            state.state.runs[0].operations[0]
                .mcp
                .as_ref()
                .unwrap()
                .disposition(),
            Some(disposition)
        );
        assert_eq!(
            state.state.runs[0].run.state,
            if disposition == McpDisposition::OutcomeUnknown {
                RunState::OutcomeUnknown
            } else {
                RunState::Interrupted
            }
        );
    }
}

#[test]
fn mcp_rejects_foreign_projection_oversized_result_and_other_store() {
    let (_root, mut writer) = begun();
    let before = writer.snapshot().unwrap();
    for mutation in 0..4 {
        let mut projection = reply();
        match mutation {
            0 => {
                projection.source.origin = SourceOrigin::ModelCall {
                    model_call: "other".into(),
                }
            }
            1 => projection.source.server = "other".into(),
            2 => {
                projection.parts = vec![ProjectedPart::Text {
                    text: "x".repeat(32 * 1024),
                    uri: None,
                    mime: None,
                }]
            }
            _ => {
                projection.parts = vec![ProjectedPart::InputRequired {
                    request: serde_json::json!({}),
                }]
            }
        }
        assert!(
            writer
                .prepare_mcp_terminal(&target(), &op(), McpTerminalInput::Reply(&projection))
                .is_err()
        );
        unchanged(&before, &writer.snapshot().unwrap());
    }
    let projection = reply();
    let publication = writer
        .prepare_mcp_terminal(&target(), &op(), McpTerminalInput::Reply(&projection))
        .unwrap();
    let (_other_root, mut other) = begun();
    assert!(other.publish_mcp_terminal(&publication).is_err());
    assert!(
        writer
            .record_operation_result(&target(), &op(), result())
            .is_err()
    );
    unchanged(&before, &writer.snapshot().unwrap());
}

#[test]
fn mcp_proof_and_terminal_cannot_be_rewritten_in_later_state() {
    let (_root, mut writer) = begun();
    let projection = reply();
    let publication = writer
        .prepare_mcp_terminal(&target(), &op(), McpTerminalInput::Reply(&projection))
        .unwrap();
    writer.publish_mcp_terminal(&publication).unwrap();
    let before = writer.snapshot().unwrap();
    for mutation in 0..4 {
        let mut value = serde_json::to_value(&before.state).unwrap();
        let operation = &mut value["runs"][0]["operations"][0];
        match mutation {
            0 => operation["mcp"]["intent"]["target"] = "changed".into(),
            1 => operation["mcp"]["terminal"]["disposition"] = "before_send".into(),
            2 => operation["mcp"] = serde_json::Value::Null,
            _ => operation["result_id"] = "forged".into(),
        }
        let mut next = before.clone();
        next.state = serde_json::from_value(value).unwrap();
        assert!(writer.commit(next, false).is_err());
        unchanged(&before, &writer.snapshot().unwrap());
    }
    let old: Operation =
        serde_json::from_value(serde_json::json!({"operation_id":"legacy","result_id":null}))
            .unwrap();
    assert!(old.mcp.is_none());
    assert!(serde_json::to_value(old).unwrap().get("mcp").is_none());
}

#[test]
fn mcp_rejects_malformed_intermediate_terminal_even_when_latest_is_valid() {
    let (root, mut writer) = begun();
    let projection = reply();
    let publication = writer
        .prepare_mcp_terminal(&target(), &op(), McpTerminalInput::Reply(&projection))
        .unwrap();
    writer.publish_mcp_terminal(&publication).unwrap();
    let intermediate = writer.snapshot().unwrap().marker.session_revision;
    writer
        .finish(&target(), Observation::Succeeded, result())
        .unwrap();
    let mut marker = writer.snapshot().unwrap().marker;
    let directory = dir(&writer);
    drop(writer);
    let bytes = fs::read(directory.join(disk::SIDECAR)).unwrap();
    let mut rewritten = Vec::new();
    let mut changed = 0;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        let mut value: serde_json::Value = serde_json::from_slice(line).unwrap();
        if value["session_revision"] == serde_json::to_value(intermediate).unwrap() {
            assert!(value["runs"][0]["operations"][0]["mcp"]["terminal"].is_object());
            assert!(value["runs"][0]["operations"][0]["result_id"].is_string());
            value["runs"][0]["operations"][0]["result_id"] = serde_json::Value::Null;
            changed += 1;
        }
        rewritten.extend(serde_json::to_vec(&value).unwrap());
        rewritten.push(b'\n');
    }
    assert_eq!(changed, 1);
    marker.sidecar_offset = DecimalU64::new(rewritten.len() as u64);
    marker.sidecar_hash = crate::conversation_state::content_hash(&rewritten);
    fs::write(directory.join(disk::SIDECAR), rewritten).unwrap();
    fs::write(
        directory.join(disk::MARKER),
        serde_json::to_vec(&marker).unwrap(),
    )
    .unwrap();
    assert!(
        matches!(
            root.open(&project(), &session()),
            Err(StoreError::Corrupt(_))
        ),
        "an invalid MCP result/terminal pair in an intermediate state must be rejected"
    );
}

#[test]
fn mcp_suffix_reconciliation_retains_terminal_result_after_client_drop() {
    for worker_has_call in [false, true] {
        let (root, mut writer, operation, suffix) = approved();
        let main = OperationId::new("main").unwrap();
        writer.record_intent(&target(), main.clone()).unwrap();
        writer
            .consume_mcp_approval_intent(
                &target(),
                &pending().approval_id,
                &operation,
                suffix.clone(),
                3,
            )
            .unwrap();
        let publication = writer
            .prepare_mcp_terminal(
                &target(),
                &op(),
                McpTerminalInput::Failure(OperationFailure::OutcomeUnknown(
                    polaris_mcp::Error::Cancelled,
                )),
            )
            .unwrap();
        let received = writer.publish_mcp_terminal(&publication).unwrap();
        let before = writer.snapshot().unwrap();
        let partial = if worker_has_call {
            suffix.clone()
        } else {
            vec![]
        };
        let full = writer
            .reconcile_mcp_turn_suffix(&target(), partial, None)
            .unwrap();
        assert_eq!(full.len(), 2);
        assert_eq!(
            serde_json::to_value(&full[1]).unwrap(),
            serde_json::to_value(&received.message).unwrap()
        );
        unchanged(&before, &writer.snapshot().unwrap());
        writer
            .finish_with_messages(
                &target(),
                &main,
                Observation::OutcomeUnknown,
                result(),
                full.clone(),
            )
            .unwrap();
        let saved = writer.snapshot().unwrap();
        assert_eq!(saved.raw.len(), 3);
        assert_eq!(saved.state.runs[0].run.state, RunState::OutcomeUnknown);
        assert!(
            writer
                .reconcile_mcp_turn_suffix(&target(), suffix, None)
                .is_err()
        );
        writer
            .finish_with_messages(
                &target(),
                &main,
                Observation::OutcomeUnknown,
                result(),
                full,
            )
            .unwrap();
        unchanged(&saved, &writer.snapshot().unwrap());
        drop(writer);
        let reopened = root.open(&project(), &session()).unwrap();
        unchanged(&saved, &reopened.snapshot().unwrap());
    }
}

#[test]
fn mcp_suffix_reconciliation_keeps_acknowledged_result_and_worker_tail_once() {
    let (_root, mut writer, operation, mut suffix) = approved();
    let main = OperationId::new("main").unwrap();
    writer.record_intent(&target(), main.clone()).unwrap();
    writer
        .consume_mcp_approval_intent(
            &target(),
            &pending().approval_id,
            &operation,
            suffix.clone(),
            3,
        )
        .unwrap();
    let projection = reply();
    let publication = writer
        .prepare_mcp_terminal(&target(), &op(), McpTerminalInput::Reply(&projection))
        .unwrap();
    suffix.push(writer.publish_mcp_terminal(&publication).unwrap().message);
    suffix.push(Message::assistant("finished"));
    let before = writer.snapshot().unwrap();
    let full = writer
        .reconcile_mcp_turn_suffix(&target(), suffix.clone(), None)
        .unwrap();
    assert_eq!(
        serde_json::to_value(&full).unwrap(),
        serde_json::to_value(&suffix).unwrap()
    );
    unchanged(&before, &writer.snapshot().unwrap());
    writer
        .finish_with_messages(&target(), &main, Observation::Succeeded, result(), full)
        .unwrap();
    let saved = writer.snapshot().unwrap();
    assert_eq!(saved.raw.len(), 4);
    assert_eq!(
        saved
            .raw
            .iter()
            .filter(|event| event.message.tool_call_id.as_deref() == Some("call"))
            .count(),
        1
    );
    let replay = writer
        .reconcile_mcp_turn_suffix(&target(), suffix, None)
        .unwrap();
    writer
        .finish_with_messages(&target(), &main, Observation::Succeeded, result(), replay)
        .unwrap();
    unchanged(&saved, &writer.snapshot().unwrap());
}

#[test]
fn mcp_suffix_reconciliation_rejects_conflicts_unconfirmed_and_non_mcp_truncation() {
    let (_root, mut writer, operation, mut suffix) = approved();
    // Another tool result can follow this MCP result, but cannot replace or
    // move before the already published result at the same raw position.
    suffix[0].tool_calls.push(ToolCall {
        id: "other".into(),
        name: "read".into(),
        arguments: serde_json::json!({}),
    });
    writer
        .consume_mcp_approval_intent(
            &target(),
            &pending().approval_id,
            &operation,
            suffix.clone(),
            3,
        )
        .unwrap();
    assert!(
        writer
            .reconcile_mcp_turn_suffix(&target(), vec![], None)
            .is_err()
    );
    let projection = reply();
    let publication = writer
        .prepare_mcp_terminal(&target(), &op(), McpTerminalInput::Reply(&projection))
        .unwrap();
    let result = writer.publish_mcp_terminal(&publication).unwrap().message;
    let before = writer.snapshot().unwrap();
    for mutation in 0..5 {
        let mut worker = suffix.clone();
        match mutation {
            0 => worker[0].content = "changed".into(),
            1 => worker[0].tool_calls[0].arguments = serde_json::json!({"text":"changed"}),
            2 => worker.push(Message::tool_result("call", "changed result")),
            3 => worker.push(Message::tool_result("other", "out of order")),
            _ => {
                worker[0].tool_calls.reverse();
            }
        }
        assert!(
            writer
                .reconcile_mcp_turn_suffix(&target(), worker, None)
                .is_err()
        );
        unchanged(&before, &writer.snapshot().unwrap());
    }
    let mut full = suffix;
    full.push(result);
    full.push(Message::tool_result("other", "later"));
    assert_eq!(
        writer
            .reconcile_mcp_turn_suffix(&target(), full, None)
            .unwrap()
            .len(),
        3
    );
    let (_other_root, mut ordinary) = setup();
    let previous = workflow_fixture();
    ordinary
        .configure_workflow(DecimalU64::new(0), Some(previous.clone()))
        .unwrap();
    ordinary.apply(&start("start", 0), Some(target())).unwrap();
    let mut candidate = previous.clone();
    candidate.state.phase = crate::workflow::Phase::Review;
    candidate.state.revision = 1;
    // Unrelated workflow history still requires the worker's complete suffix.
    ordinary
        .checkpoint_workflow(
            &target(),
            desktop_tool_turn(),
            previous,
            candidate,
            None,
            None,
        )
        .unwrap();
    assert!(
        ordinary
            .reconcile_mcp_turn_suffix(&target(), vec![], None)
            .is_err()
    );
}

#[test]
fn mcp_suffix_reconciliation_does_not_absorb_later_workflow_checkpoints() {
    let (_root, mut writer) = setup();
    let previous = workflow_fixture();
    writer
        .configure_workflow(DecimalU64::new(0), Some(previous.clone()))
        .unwrap();
    writer.apply(&start("start", 0), Some(target())).unwrap();
    let (operation, mut suffix) = evidence();
    let mut approval = pending();
    approval.operation = MCP_TOOL_OPERATION.into();
    approval.scope = operation.target().into();
    approval.payload_hash = operation.payload_hash().into();
    writer.publish_approval(approval, 1).unwrap();
    writer.resolve_approval(&answer(), 2).unwrap();
    writer
        .consume_mcp_approval_intent(
            &target(),
            &pending().approval_id,
            &operation,
            suffix.clone(),
            3,
        )
        .unwrap();
    let projection = reply();
    let publication = writer
        .prepare_mcp_terminal(&target(), &op(), McpTerminalInput::Reply(&projection))
        .unwrap();
    let short = suffix.clone();
    suffix.push(writer.publish_mcp_terminal(&publication).unwrap().message);
    assert_eq!(
        writer
            .reconcile_mcp_turn_suffix(&target(), short.clone(), Some(&previous))
            .unwrap()
            .len(),
        2
    );
    let mut candidate = previous.clone();
    candidate.state.phase = crate::workflow::Phase::Review;
    candidate.state.revision = 1;
    writer
        .checkpoint_workflow(
            &target(),
            suffix.clone(),
            previous,
            candidate.clone(),
            None,
            None,
        )
        .unwrap();
    // Even without new raw messages, an ACK-lost state transition cannot be
    // replaced by the stale worker's workflow in the subsequent finish.
    assert!(
        writer
            .reconcile_mcp_turn_suffix(&target(), short.clone(), Some(&workflow_fixture()))
            .is_err()
    );
    let before = writer.snapshot().unwrap();
    unchanged(&before, &writer.snapshot().unwrap());
    suffix.extend(desktop_tool_turn());
    writer
        .checkpoint_workflow(
            &target(),
            suffix.clone(),
            candidate.clone(),
            candidate.clone(),
            None,
            None,
        )
        .unwrap();
    let saved = writer.snapshot().unwrap();
    // A matching workflow does not permit recovering a non-MCP raw tail.
    assert!(
        writer
            .reconcile_mcp_turn_suffix(&target(), short, Some(&candidate))
            .is_err()
    );
    unchanged(&saved, &writer.snapshot().unwrap());
}

fn resource_evidence() -> (McpOperation, Vec<Message>) {
    let (tool, _) = evidence();
    let mut binding = tool.binding().clone();
    binding.item.kind = Kind::Resource;
    binding.item.key = "memory://note".into();
    binding.resource_uri = Some(binding.item.key.clone());
    binding.schema_hash = None;
    binding.arguments_hash = canonical_hash(&json!({})).unwrap();
    let meta = json!({"op":"read","server":"fixture","kind":"resource","key":"memory://note","generation":binding.catalog_generation,"arguments":{}});
    let operation = McpOperation::from_resource_binding(
        tool.identity().clone(),
        "project",
        binding,
        canonical_hash(&meta).unwrap(),
    )
    .unwrap();
    let suffix = vec![Message::assistant_with_tool_calls(
        "",
        vec![ToolCall {
            id: "call".into(),
            name: "mcp_catalog".into(),
            arguments: meta,
        }],
    )];
    (operation, suffix)
}

#[test]
fn resource_intent_marker_failure_confirms_without_an_approval_or_partial_raw() {
    for point in ["raw_sync", "marker_rename", "directory_sync"] {
        let (_root, mut writer) = setup();
        writer.apply(&start("start", 0), Some(target())).unwrap();
        let (operation, suffix) = resource_evidence();
        assert!(
            !writer
                .confirm_mcp_read_intent(&target(), &operation, &suffix)
                .unwrap()
        );
        inject(Some(point));
        assert!(
            writer
                .record_mcp_read_intent(&target(), &operation, suffix.clone(), 3)
                .is_err()
        );
        inject(None);
        let present = writer
            .confirm_mcp_read_intent(&target(), &operation, &suffix)
            .unwrap();
        assert_eq!(present, point == "directory_sync");
        let saved = writer.snapshot().unwrap();
        assert!(saved.state.approval_records.is_empty());
        assert_eq!(saved.raw.len(), if present { 2 } else { 1 });
        assert_eq!(saved.state.runs[0].operations.len(), usize::from(present));
        if present {
            let publication = writer
                .prepare_mcp_terminal(
                    &target(),
                    &op(),
                    McpTerminalInput::Failure(OperationFailure::BeforeSend(
                        polaris_mcp::Error::Storage,
                    )),
                )
                .unwrap();
            let result = writer.publish_mcp_terminal(&publication).unwrap();
            assert_eq!(result.disposition, McpDisposition::BeforeSend);
        }
    }
}

#[test]
fn resource_original_meta_args_policy_expiry_and_tool_grant_paths_are_not_interchangeable() {
    let (_root, mut writer) = setup();
    writer.apply(&start("start", 0), Some(target())).unwrap();
    let (operation, suffix) = resource_evidence();
    for (key, value) in [
        ("op", json!("select")),
        ("server", json!("other")),
        ("kind", json!("tool")),
        ("key", json!("memory://other")),
        ("generation", json!("x")),
        ("arguments", json!({"unexpected":true})),
        ("q", json!("extra")),
    ] {
        let mut wrong = suffix.clone();
        wrong[0].tool_calls[0].arguments[key] = value;
        assert!(
            writer
                .record_mcp_read_intent(&target(), &operation, wrong, 3)
                .is_err()
        );
    }
    for (wrong_target, wrong_revision) in [("other-project", 7), ("project", 8)] {
        let mut identity = operation.identity().clone();
        identity.policy_revision = wrong_revision;
        let wrong = McpOperation::from_resource_binding(
            identity,
            wrong_target,
            operation.binding().clone(),
            canonical_hash(&suffix[0].tool_calls[0].arguments).unwrap(),
        )
        .unwrap();
        assert!(
            writer
                .record_mcp_read_intent(&target(), &wrong, suffix.clone(), 3)
                .is_err()
        );
    }
    assert!(
        writer
            .record_mcp_read_intent(&target(), &operation, suffix.clone(), 100)
            .is_err()
    );
    assert!(
        writer
            .consume_mcp_approval_intent(
                &target(),
                &pending().approval_id,
                &operation,
                suffix.clone(),
                3
            )
            .is_err()
    );
    assert!(
        crate::approval::Gate::new(crate::approval::ApprovalPolicy::Always)
            .owned_mcp_approval(crate::mcp::operation::ExternalCeiling::Allowed, &operation)
            .is_err()
    );
    let (tool, tool_suffix) = evidence();
    assert!(
        writer
            .record_mcp_read_intent(&target(), &tool, tool_suffix, 3)
            .is_err()
    );
    assert_eq!(
        writer
            .record_mcp_read_intent(&target(), &operation, suffix.clone(), 3)
            .unwrap(),
        IntentReceipt::NewlyPublished
    );
    assert_eq!(
        writer
            .record_mcp_read_intent(&target(), &operation, suffix.clone(), 3)
            .unwrap(),
        IntentReceipt::AlreadyRecorded
    );
    let before = writer.snapshot().unwrap();
    assert!(
        writer
            .confirm_mcp_read_intent(&target(), &operation, &suffix)
            .unwrap()
    );
    assert_eq!(
        serde_json::to_value(&before.marker).unwrap(),
        serde_json::to_value(&writer.snapshot().unwrap().marker).unwrap()
    );
}

#[test]
fn legacy_tool_proof_json_is_unchanged_and_cannot_be_downgraded_to_a_read() {
    use crate::mcp::operation::SavedMcpIntent;
    let (operation, _) = evidence();
    let value = serde_json::to_value(SavedMcpIntent::from_operation(&operation).unwrap()).unwrap();
    assert_eq!(value["schema_hash"], "e".repeat(64));
    assert!(value.get("resource").is_none());
    let saved: SavedMcpIntent = serde_json::from_value(value.clone()).unwrap();
    saved.validate().unwrap();
    assert_eq!(serde_json::to_value(saved).unwrap(), value);
    let (read, _) = resource_evidence();
    let mut bad = serde_json::to_value(SavedMcpIntent::from_operation(&read).unwrap()).unwrap();
    bad.as_object_mut().unwrap().remove("resource");
    assert!(
        serde_json::from_value::<SavedMcpIntent>(bad)
            .unwrap()
            .validate()
            .is_err()
    );
}

#[test]
fn host_prompt_result_cannot_complete_a_model_tool_call_with_the_same_id() {
    let (_root, mut writer) = begun();
    let before = writer.snapshot().unwrap();
    let mut projection = reply();
    projection.source.origin = SourceOrigin::HostSelection {
        selection_id: "call".into(),
    };
    assert!(
        writer
            .prepare_mcp_terminal(&target(), &op(), McpTerminalInput::Reply(&projection))
            .is_err()
    );
    unchanged(&before, &writer.snapshot().unwrap());
    let projection = reply();
    let publication = writer
        .prepare_mcp_terminal(&target(), &op(), McpTerminalInput::Reply(&projection))
        .unwrap();
    let published = writer.publish_mcp_terminal(&publication).unwrap();
    assert_eq!(published.disposition, McpDisposition::Completed);
    assert_eq!(published.message.tool_call_id.as_deref(), Some("call"));
}

#[test]
fn host_prompt_saved_result_rejects_mixed_origins_even_with_matching_hashes() {
    use crate::conversation_state::content_hash;

    let (root, mut writer) = begun();
    let projection = reply();
    let publication = writer
        .prepare_mcp_terminal(&target(), &op(), McpTerminalInput::Reply(&projection))
        .unwrap();
    writer.publish_mcp_terminal(&publication).unwrap();
    writer
        .finish(&target(), Observation::Succeeded, result())
        .unwrap();
    let original = writer.snapshot().unwrap();
    let directory = dir(&writer);
    drop(writer);
    // An ordinary model result still reopens without changing any saved state.
    let writer = root.open(&project(), &session()).unwrap();
    unchanged(&original, &writer.snapshot().unwrap());
    drop(writer);

    let original_raw = fs::read(directory.join(disk::RAW)).unwrap();
    let original_sidecar = fs::read(directory.join(disk::SIDECAR)).unwrap();
    let original_marker = fs::read(directory.join(disk::MARKER)).unwrap();
    for selection in [json!("call"), serde_json::Value::Null] {
        let mut raw = original.raw.clone();
        let message = &mut raw.last_mut().unwrap().message;
        assert_eq!(message.tool_call_id.as_deref(), Some("call"));
        let envelope: serde_json::Value = serde_json::from_str(&message.content).unwrap();
        let old_reply = serde_json::to_string(&envelope["reply"]).unwrap();
        let mut changed_reply = envelope["reply"].clone();
        changed_reply["source"]["selection_id"] = selection;
        let changed_reply = serde_json::to_string(&changed_reply).unwrap();
        assert_eq!(message.content.matches(&old_reply).count(), 1);
        // Keep the Envelope field ordering canonical and alter only the reply.
        message.content = message.content.replace(&old_reply, &changed_reply);
        let envelope_hash = content_hash(message.content.as_bytes());
        let raw_bytes: Vec<u8> = raw
            .iter()
            .flat_map(|event| disk::line(event).unwrap())
            .collect();
        let mut sidecar_bytes = Vec::new();
        let mut changed = 0;
        for line in original_sidecar.split_inclusive(|byte| *byte == b'\n') {
            let mut value: serde_json::Value = serde_json::from_slice(line).unwrap();
            if let Some(operation) = value
                .get_mut("runs")
                .and_then(|runs| runs.get_mut(0))
                .and_then(|run| run.get_mut("operations"))
                .and_then(|operations| operations.get_mut(0))
                && operation["mcp"]["terminal"].is_object()
            {
                operation["mcp"]["terminal"]["envelope_hash"] = json!(envelope_hash);
                operation["result_id"] = json!(format!("mcp-result-{envelope_hash}"));
                changed += 1;
            }
            sidecar_bytes.extend(disk::line(&value).unwrap());
        }
        assert_eq!(
            changed, 2,
            "terminal publication and finished state must agree"
        );
        let mut marker = original.marker.clone();
        marker.raw_offset = DecimalU64::new(raw_bytes.len() as u64);
        marker.raw_hash = content_hash(&raw_bytes);
        marker.sidecar_offset = DecimalU64::new(sidecar_bytes.len() as u64);
        marker.sidecar_hash = content_hash(&sidecar_bytes);
        let marker_bytes = serde_json::to_vec(&marker).unwrap();
        fs::write(directory.join(disk::RAW), &raw_bytes).unwrap();
        fs::write(directory.join(disk::SIDECAR), &sidecar_bytes).unwrap();
        fs::write(directory.join(disk::MARKER), &marker_bytes).unwrap();
        assert!(
            matches!(
                root.open(&project(), &session()),
                Err(StoreError::RunConflict)
            ),
            "saved model results must reject host selection provenance, including null"
        );
        assert_eq!(fs::read(directory.join(disk::RAW)).unwrap(), raw_bytes);
        assert_eq!(
            fs::read(directory.join(disk::SIDECAR)).unwrap(),
            sidecar_bytes
        );
        assert_eq!(
            fs::read(directory.join(disk::MARKER)).unwrap(),
            marker_bytes
        );
    }
    fs::write(directory.join(disk::RAW), original_raw).unwrap();
    fs::write(directory.join(disk::SIDECAR), original_sidecar).unwrap();
    fs::write(directory.join(disk::MARKER), original_marker).unwrap();
    let writer = root.open(&project(), &session()).unwrap();
    unchanged(&original, &writer.snapshot().unwrap());
}

fn migration_turn(messages: Vec<Message>) -> Vec<RawEventV2> {
    std::iter::once(Message::user("fixture input"))
        .chain(messages)
        .enumerate()
        .map(|(i, message)| RawEventV2 {
            schema_version: 2,
            sequence: i as u64 + 1,
            epoch: 1,
            turn_id: 1,
            starts_turn: i == 0,
            message,
        })
        .collect()
}
#[test]
fn migration_local_tool_keeps_terminal_history_and_rejects_unknown() {
    for disposition in [
        McpDisposition::Completed,
        McpDisposition::BeforeSend,
        McpDisposition::InputRequired,
        McpDisposition::OutcomeUnknown,
    ] {
        let (operation, mut messages) = evidence();
        let mut response = reply();
        if disposition == McpDisposition::InputRequired {
            response.status = "input_required";
        }
        let input = match disposition {
            McpDisposition::BeforeSend => McpTerminalInput::Failure(OperationFailure::BeforeSend(
                polaris_mcp::Error::Transport,
            )),
            McpDisposition::OutcomeUnknown => McpTerminalInput::Failure(
                OperationFailure::OutcomeUnknown(polaris_mcp::Error::Transport),
            ),
            _ => McpTerminalInput::Reply(&response),
        };
        messages.push(
            super::super::local_mcp_terminal(&operation, input)
                .unwrap()
                .message,
        );
        let events = migration_turn(messages);
        // A result exists even for Unknown: the generic round-trip validator passes.
        crate::conversation_state::validate_complete_turn_events(events.iter()).unwrap();
        let result =
            super::super::writer::migration::validate_local_turn(&events, "project", "session");
        assert_eq!(
            result.is_ok(),
            disposition != McpDisposition::OutcomeUnknown
        );
    }
}
#[test]
fn migration_local_tool_rejects_malformed_binding_and_wire_drift() {
    for mutation in 0..5 {
        let (operation, mut messages) = evidence();
        let mut result =
            super::super::local_mcp_terminal(&operation, McpTerminalInput::Reply(&reply()))
                .unwrap()
                .message;
        match mutation {
            0 => result.content = "not an envelope".into(),
            1 => result.content.push(' '),
            2 => {
                let mut value: serde_json::Value = serde_json::from_str(&result.content).unwrap();
                value["intent"]["registry_revision"] = json!(99);
                result.content = serde_json::to_string(&value).unwrap();
            }
            3 => messages[0].tool_calls[0].arguments = json!({"changed":true}),
            _ => result.tool_call_id = Some("different-call".into()),
        }
        messages.push(result);
        assert!(
            super::super::writer::migration::validate_local_turn(
                &migration_turn(messages),
                "project",
                "session"
            )
            .is_err()
        );
    }
}
#[test]
fn migration_local_generic_calls_and_catalog_metadata_require_complete_round_trip() {
    for name in ["file_reader", "mcp_catalog"] {
        let call = Message::assistant_with_tool_calls(
            "",
            vec![ToolCall {
                id: "ordinary".into(),
                name: name.into(),
                arguments: json!({"op":"search","q":"fixture"}),
            }],
        );
        assert!(
            super::super::writer::migration::validate_local_turn(
                &migration_turn(vec![call.clone()]),
                "project",
                "session"
            )
            .is_err()
        );
        let events = migration_turn(vec![call, Message::tool_result("ordinary", "metadata")]);
        super::super::writer::migration::validate_local_turn(&events, "project", "session")
            .unwrap();
    }
    let events = migration_turn(vec![Message::assistant("complete")]);
    super::super::writer::migration::validate_local_turn(&events, "project", "session").unwrap();
}
#[test]
fn migration_desktop_rejects_persisted_unknown_and_unfinished_mcp_without_recovery() {
    let (_root, mut writer, operation, suffix) = approved();
    writer
        .consume_mcp_approval_intent(&target(), &pending().approval_id, &operation, suffix, 3)
        .unwrap();
    let snapshot = writer.snapshot().unwrap();
    assert!(super::super::writer::migration::validate_desktop(&snapshot).is_err());
    let publication = writer
        .prepare_mcp_terminal(
            &target(),
            &op(),
            McpTerminalInput::Failure(OperationFailure::OutcomeUnknown(
                polaris_mcp::Error::Transport,
            )),
        )
        .unwrap();
    writer.publish_mcp_terminal(&publication).unwrap();
    let snapshot = writer.snapshot().unwrap();
    assert!(snapshot.state.runs[0].operations[0].result_id.is_some());
    assert!(super::super::writer::migration::validate_desktop(&snapshot).is_err());
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
type MigrationSessionSnapshot = Vec<(String, Vec<u8>, (u64, u64, u32, u64, i64, i64, i64, i64))>;

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn migration_session_files(path: &std::path::Path) -> MigrationSessionSnapshot {
    use std::os::unix::fs::MetadataExt;
    let mut result = Vec::new();
    for entry in std::fs::read_dir(path).unwrap() {
        let entry = entry.unwrap();
        let meta = entry.path().symlink_metadata().unwrap();
        if meta.is_file() {
            result.push((
                entry.file_name().into_string().unwrap(),
                std::fs::read(entry.path()).unwrap(),
                (
                    meta.dev(),
                    meta.ino(),
                    meta.mode(),
                    meta.nlink(),
                    meta.mtime(),
                    meta.mtime_nsec(),
                    meta.ctime(),
                    meta.ctime_nsec(),
                ),
            ));
        }
    }
    result.sort_by(|a, b| a.0.cmp(&b.0));
    result
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn migration_fd_session_inspection_is_read_only_and_keeps_shared_existing_lock() {
    let (_root, writer) = setup();
    let path = dir(&writer);
    drop(writer);
    let parent = disk::Directory::open_owned(path.parent().unwrap()).unwrap();
    let bucket = path.file_name().unwrap().to_str().unwrap();
    let before = migration_session_files(&path);
    let view = super::super::writer::migration::DesktopSessionTransportInspectionV2::inspect(
        &parent, bucket,
    )
    .unwrap();
    assert_eq!(
        view.verified_published().unwrap().marker.session_id,
        session()
    );
    assert_eq!(before, migration_session_files(&path));
    let lock = std::fs::File::options()
        .read(true)
        .write(true)
        .open(path.join("writer.lock"))
        .unwrap();
    assert!(matches!(
        lock.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    drop(view);
    lock.try_lock().unwrap();
    lock.unlock().unwrap();
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn migration_fd_session_inspection_rejects_active_owner_without_recovery() {
    let (_root, writer) = setup();
    let path = dir(&writer);
    let before = migration_session_files(&path);
    let parent = disk::Directory::open_owned(path.parent().unwrap()).unwrap();
    let result = super::super::writer::migration::DesktopSessionTransportInspectionV2::inspect(
        &parent,
        path.file_name().unwrap().to_str().unwrap(),
    );
    assert!(matches!(result, Err(StoreError::Busy)));
    assert_eq!(before, migration_session_files(&path));
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn migration_fd_session_inspection_rejects_unfinished_and_unknown_binding_without_repair() {
    for unknown in [false, true] {
        let (_root, mut writer, operation, suffix) = approved();
        writer
            .consume_mcp_approval_intent(&target(), &pending().approval_id, &operation, suffix, 3)
            .unwrap();
        if unknown {
            let publication = writer
                .prepare_mcp_terminal(
                    &target(),
                    &op(),
                    McpTerminalInput::Failure(OperationFailure::OutcomeUnknown(
                        polaris_mcp::Error::Transport,
                    )),
                )
                .unwrap();
            writer.publish_mcp_terminal(&publication).unwrap();
        }
        let path = dir(&writer);
        drop(writer);
        let before = migration_session_files(&path);
        let parent = disk::Directory::open_owned(path.parent().unwrap()).unwrap();
        assert!(
            super::super::writer::migration::DesktopSessionTransportInspectionV2::inspect(
                &parent,
                path.file_name().unwrap().to_str().unwrap()
            )
            .is_err()
        );
        assert_eq!(before, migration_session_files(&path));
    }
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn migration_fd_session_inspection_rejects_file_root_parent_and_child_set_drift() {
    use std::os::unix::fs::PermissionsExt;
    for mutation in 0..5 {
        let (_root, writer) = setup();
        let path = dir(&writer);
        drop(writer);
        let parent = disk::Directory::open_owned(path.parent().unwrap()).unwrap();
        let view = super::super::writer::migration::DesktopSessionTransportInspectionV2::inspect(
            &parent,
            path.file_name().unwrap().to_str().unwrap(),
        )
        .unwrap();
        match mutation {
            0 => {
                let mut file = std::fs::OpenOptions::new()
                    .append(true)
                    .open(path.join("raw.jsonl"))
                    .unwrap();
                file.write_all(b"changed").unwrap();
            }
            1 => {
                let temporary = path.join("fixture-replacement");
                std::fs::write(&temporary, std::fs::read(path.join("state.json")).unwrap())
                    .unwrap();
                std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o600))
                    .unwrap();
                std::fs::rename(temporary, path.join("state.json")).unwrap();
            }
            2 => {
                std::fs::rename(&path, path.with_file_name("saved-session")).unwrap();
                std::fs::create_dir(&path).unwrap();
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
            }
            3 => {
                let parent_path = path.parent().unwrap();
                std::fs::rename(parent_path, parent_path.with_extension("saved-parent")).unwrap();
                std::fs::create_dir(parent_path).unwrap();
                std::fs::set_permissions(parent_path, std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
            _ => {
                std::fs::write(path.join("state.next"), b"unfinished").unwrap();
                std::fs::set_permissions(
                    path.join("state.next"),
                    std::fs::Permissions::from_mode(0o600),
                )
                .unwrap();
            }
        }
        assert!(view.verified_published().is_err());
        if mutation == 3 {
            drop(view);
            drop(parent);
            let parent_path = path.parent().unwrap();
            std::fs::remove_dir(parent_path).unwrap();
            std::fs::rename(parent_path.with_extension("saved-parent"), parent_path).unwrap();
        }
    }
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn migration_fd_session_inspection_closes_changes_after_payload_decode() {
    use super::super::writer::migration::AFTER_SESSION_DECODE;
    use std::os::unix::fs::PermissionsExt;
    for addition in [false, true] {
        let (_root, writer) = setup();
        let path = dir(&writer);
        drop(writer);
        let parent = disk::Directory::open_owned(path.parent().unwrap()).unwrap();
        let captured = path.clone();
        AFTER_SESSION_DECODE.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                if addition {
                    std::fs::write(captured.join("state.next"), b"unfinished").unwrap();
                    std::fs::set_permissions(
                        captured.join("state.next"),
                        std::fs::Permissions::from_mode(0o600),
                    )
                    .unwrap();
                } else {
                    let mut file = std::fs::OpenOptions::new()
                        .append(true)
                        .open(captured.join("raw.jsonl"))
                        .unwrap();
                    file.write_all(b"changed after decode").unwrap();
                }
            }))
        });
        assert!(
            super::super::writer::migration::DesktopSessionTransportInspectionV2::inspect(
                &parent,
                path.file_name().unwrap().to_str().unwrap()
            )
            .is_err()
        );
        assert!(AFTER_SESSION_DECODE.with(|slot| slot.borrow().is_none()));
    }
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn migration_fd_session_inspection_rejects_unpublished_tails_without_repair() {
    for name in ["raw.jsonl", "desktop.jsonl"] {
        let (_root, writer) = setup();
        let path = dir(&writer);
        drop(writer);
        std::fs::OpenOptions::new()
            .append(true)
            .open(path.join(name))
            .unwrap()
            .write_all(b"unpublished-operation")
            .unwrap();
        let before = migration_session_files(&path);
        let parent = disk::Directory::open_owned(path.parent().unwrap()).unwrap();
        assert!(matches!(
            super::super::writer::migration::DesktopSessionTransportInspectionV2::inspect(
                &parent,
                path.file_name().unwrap().to_str().unwrap()
            ),
            Err(StoreError::RecoveryRequired)
        ));
        assert_eq!(before, migration_session_files(&path));
    }
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn migration_fd_session_inspection_rejects_foreign_bucket_and_unfinished_files() {
    use std::os::unix::fs::PermissionsExt;
    for mutation in 0..3 {
        let (_root, writer) = setup();
        let mut path = dir(&writer);
        drop(writer);
        match mutation {
            0 => {
                let renamed = path.with_file_name("f".repeat(64));
                std::fs::rename(&path, &renamed).unwrap();
                path = renamed;
            }
            1 => std::fs::remove_file(path.join("state.json")).unwrap(),
            _ => {
                std::fs::write(path.join("state.next"), b"unfinished").unwrap();
                std::fs::set_permissions(
                    path.join("state.next"),
                    std::fs::Permissions::from_mode(0o600),
                )
                .unwrap();
            }
        }
        let before = migration_session_files(&path);
        let parent = disk::Directory::open_owned(path.parent().unwrap()).unwrap();
        assert!(
            super::super::writer::migration::DesktopSessionTransportInspectionV2::inspect(
                &parent,
                path.file_name().unwrap().to_str().unwrap()
            )
            .is_err()
        );
        assert_eq!(before, migration_session_files(&path));
    }
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
mod desktop_root_migration_tests {
    use super::super::super::writer::migration::root::{
        AFTER_ROOT_DECODE, AFTER_ROOT_FINAL_NAMES, AFTER_ROOT_NAMES,
        DesktopRootTransportInspectionV2,
    };
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };
    fn empty() -> (tempfile::TempDir, disk::Directory) {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let directory = disk::Directory::open_owned(root.path()).unwrap();
        (root, directory)
    }
    #[test]
    fn migration_desktop_root_empty_does_not_create_children() {
        let (root, directory) = empty();
        let before =
            super::super::super::writer::migration::file_stamp(&root.path().metadata().unwrap());
        let view = DesktopRootTransportInspectionV2::inspect(&directory).unwrap();
        assert!(view.verified_sessions().unwrap().is_empty());
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 0);
        assert_eq!(
            before,
            super::super::super::writer::migration::file_stamp(&root.path().metadata().unwrap())
        );
    }
    #[test]
    fn migration_desktop_root_retains_all_sessions_and_their_real_locks() {
        let (root, writer) = setup();
        let first = dir(&writer);
        drop(writer);
        let other = root
            .create(
                ProjectId::new("second-project").unwrap(),
                SessionId::new("second-session").unwrap(),
                initial(),
            )
            .unwrap();
        let second = dir(&other);
        drop(other);
        let directory = disk::Directory::open_owned(first.parent().unwrap()).unwrap();
        let before = [
            migration_session_files(&first),
            migration_session_files(&second),
        ];
        let view = DesktopRootTransportInspectionV2::inspect(&directory).unwrap();
        let sessions = view.verified_sessions().unwrap();
        assert_eq!(sessions.len(), 2);
        assert!(sessions.values().any(|session| {
            session.verified_published().unwrap().marker.project_id
                == ProjectId::new("second-project").unwrap()
        }));
        let locks = [first.join("writer.lock"), second.join("writer.lock")]
            .map(|path| fs::File::open(path).unwrap());
        for lock in &locks {
            assert!(matches!(lock.try_lock(), Err(fs::TryLockError::WouldBlock)));
        }
        assert_eq!(
            before,
            [
                migration_session_files(&first),
                migration_session_files(&second)
            ]
        );
        drop(view);
        for lock in locks {
            lock.try_lock().unwrap();
            lock.unlock().unwrap();
        }
    }
    #[test]
    fn migration_desktop_root_failed_later_owner_releases_earlier_locks() {
        let (root, writer) = setup();
        let first = dir(&writer);
        drop(writer);
        let second = root
            .create(
                ProjectId::new("second-project").unwrap(),
                SessionId::new("second-session").unwrap(),
                initial(),
            )
            .unwrap();
        let directory = disk::Directory::open_owned(first.parent().unwrap()).unwrap();
        assert!(matches!(
            DesktopRootTransportInspectionV2::inspect(&directory),
            Err(StoreError::Busy)
        ));
        let lock = fs::File::open(first.join("writer.lock")).unwrap();
        lock.try_lock().unwrap();
        lock.unlock().unwrap();
        drop(second);
        DesktopRootTransportInspectionV2::inspect(&directory).unwrap();
    }
    #[test]
    fn migration_desktop_root_rejects_unknown_symlink_and_incomplete_bucket() {
        for kind in 0..3 {
            let (root, directory) = empty();
            let name = if kind == 0 {
                "unknown".to_owned()
            } else {
                "a".repeat(64)
            };
            let path = root.path().join(name);
            if kind == 1 {
                symlink(root.path(), &path).unwrap();
            } else {
                fs::create_dir(&path).unwrap();
                fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
            }
            assert!(DesktopRootTransportInspectionV2::inspect(&directory).is_err());
            assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
        }
    }
    #[test]
    fn migration_desktop_root_closes_insertions_at_each_observation_boundary() {
        for hook in [
            &AFTER_ROOT_NAMES,
            &AFTER_ROOT_DECODE,
            &AFTER_ROOT_FINAL_NAMES,
        ] {
            let (root, directory) = empty();
            let path = root.path().join("unexpected");
            hook.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    fs::write(path, b"x").unwrap();
                }))
            });
            assert!(DesktopRootTransportInspectionV2::inspect(&directory).is_err());
            assert!(hook.with(|slot| slot.borrow().is_none()));
        }
    }
    #[test]
    fn migration_desktop_root_rechecks_in_place_file_and_parent_mode() {
        for mutate_root in [false, true] {
            let (_root, writer) = setup();
            let path = dir(&writer);
            drop(writer);
            let directory = disk::Directory::open_owned(path.parent().unwrap()).unwrap();
            let view = DesktopRootTransportInspectionV2::inspect(&directory).unwrap();
            if mutate_root {
                fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o755)).unwrap();
            } else {
                use std::io::Write;
                fs::OpenOptions::new()
                    .append(true)
                    .open(path.join("raw.jsonl"))
                    .unwrap()
                    .write_all(b"\n")
                    .unwrap();
            }
            assert!(view.verified_sessions().is_err());
        }
    }
    #[test]
    fn migration_desktop_root_charges_session_bytes_before_decode() {
        let (_root, writer) = setup();
        let path = dir(&writer);
        drop(writer);
        let directory = disk::Directory::open_owned(path.parent().unwrap()).unwrap();
        fs::OpenOptions::new()
            .write(true)
            .open(path.join("raw.jsonl"))
            .unwrap()
            .set_len(256 * 1024 * 1024 + 1)
            .unwrap();
        assert!(matches!(
            DesktopRootTransportInspectionV2::inspect(&directory),
            Err(StoreError::Capacity("desktop root inspected bytes"))
        ));
    }
    #[test]
    fn migration_desktop_root_manager_is_observed_and_busy_is_not_repaired() {
        use crate::desktop_store::session_manager::SessionManagerStore;
        let (root, directory) = empty();
        let manager = SessionManagerStore::from_root(std::sync::Arc::new(
            disk::Directory::open_owned(directory.path()).unwrap(),
        ))
        .unwrap();
        assert!(matches!(
            DesktopRootTransportInspectionV2::inspect(&directory),
            Err(StoreError::Busy)
        ));
        drop(manager);
        let before = migration_session_files(&root.path().join("session-manager-v1"));
        let view = DesktopRootTransportInspectionV2::inspect(&directory).unwrap();
        assert!(view.verified_sessions().unwrap().is_empty());
        assert_eq!(
            before,
            migration_session_files(&root.path().join("session-manager-v1"))
        );
    }
    #[test]
    fn migration_desktop_root_manager_budget_precedes_journal_decode() {
        use crate::desktop_store::session_manager::SessionManagerStore;
        let (root, directory) = empty();
        drop(
            SessionManagerStore::from_root(std::sync::Arc::new(
                disk::Directory::open_owned(directory.path()).unwrap(),
            ))
            .unwrap(),
        );
        let path = root.path().join("session-manager-v1").join("commits.jsonl");
        let file = fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&path)
            .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        file.set_len(256 * 1024 * 1024 + 1).unwrap();
        assert!(matches!(
            DesktopRootTransportInspectionV2::inspect(&directory),
            Err(StoreError::Capacity("desktop root inspected bytes"))
        ));
    }
    #[test]
    fn migration_desktop_root_artifact_object_is_validated_and_not_rewritten() {
        use crate::desktop_store::artifacts::WorkflowArtifactStore;
        use crate::workflow::{ChangeContractV1, RoutineEligibilityV1};
        let (root, directory) = empty();
        let store = WorkflowArtifactStore::open_owned(root.path(), &project()).unwrap();
        let artifact = ChangeContractV1::draft(
            "root-contract",
            RoutineEligibilityV1::local_component("desktop_store"),
            "existing body",
            "private storage",
            "read only migration",
            vec!["validate exact bytes".into()],
        )
        .unwrap();
        store.put_change_contract(&artifact).unwrap();
        drop(store);
        DesktopRootTransportInspectionV2::inspect(&directory).unwrap();
        let objects = fs::read_dir(root.path().join("workflow-artifacts-v1"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path()
            .join("objects");
        let object = fs::read_dir(&objects)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        fs::write(&object, b"{}").unwrap();
        assert!(DesktopRootTransportInspectionV2::inspect(&directory).is_err());
        assert_eq!(fs::read(object).unwrap(), b"{}");
    }
    #[test]
    fn migration_desktop_root_control_retains_owner_and_rejects_temporary_state() {
        use crate::desktop_store::subagent_control::{
            SubagentControlBindingV1, SubagentControlOwner,
        };
        use crate::subagent_control::{RootAttemptId, RootRunId, TaskId};
        let (root, directory) = empty();
        let binding = SubagentControlBindingV1 {
            project_id: project(),
            session_id: session(),
            root_run_id: RootRunId::new("root-run").unwrap(),
            root_attempt_id: RootAttemptId::new("root-attempt").unwrap(),
            active_task_id: TaskId::new("active-task").unwrap(),
        };
        let owner = SubagentControlOwner::open_or_create_owned(root.path(), binding).unwrap();
        assert!(matches!(
            DesktopRootTransportInspectionV2::inspect(&directory),
            Err(StoreError::Busy)
        ));
        drop(owner);
        let view = DesktopRootTransportInspectionV2::inspect(&directory).unwrap();
        fn leaf(path: &std::path::Path) -> std::path::PathBuf {
            if path.join("owner.lock").exists() {
                return path.to_owned();
            }
            leaf(&fs::read_dir(path).unwrap().next().unwrap().unwrap().path())
        }
        let control = leaf(&root.path().join("subagent-control-v1"));
        let lock = fs::File::open(control.join("owner.lock")).unwrap();
        assert!(matches!(lock.try_lock(), Err(fs::TryLockError::WouldBlock)));
        drop(view);
        lock.try_lock().unwrap();
        lock.unlock().unwrap();
        fs::write(control.join("state.next"), b"unpublished").unwrap();
        assert!(DesktopRootTransportInspectionV2::inspect(&directory).is_err());
        assert_eq!(
            fs::read(control.join("state.next")).unwrap(),
            b"unpublished"
        );
    }
    #[test]
    fn migration_desktop_root_rejects_actual_persisted_mcp_control_unknown() {
        use crate::desktop_store::subagent_control::{
            SubagentControlBindingV1, SubagentControlOwner,
        };
        use crate::subagent_control::{
            ApprovalCapabilityV1, CapabilitySnapshotV1, ChildAttemptId, ChildBindingV1,
            ChildStateV1, FailureClassV1, LogicalChildId, McpCapabilityV1, ParentBindingV1,
            RegisterChildV1, RootAttemptId, RootRunId, SandboxCapabilityV1, TaskId,
        };
        let (root, directory) = empty();
        let binding = SubagentControlBindingV1 {
            project_id: project(),
            session_id: session(),
            root_run_id: RootRunId::new("root-run").unwrap(),
            root_attempt_id: RootAttemptId::new("root-attempt").unwrap(),
            active_task_id: TaskId::new("active-task").unwrap(),
        };
        let mut owner =
            SubagentControlOwner::open_or_create_owned(root.path(), binding.clone()).unwrap();
        let snapshot = owner.snapshot().unwrap();
        let mut plane = snapshot.control_plane;
        plane
            .register_before_start(RegisterChildV1 {
                logical_child_id: LogicalChildId::new("child").unwrap(),
                attempt_id: ChildAttemptId::new("attempt").unwrap(),
                binding: ChildBindingV1 {
                    root_run_id: binding.root_run_id,
                    root_attempt_id: binding.root_attempt_id,
                    active_task_id: binding.active_task_id,
                    parent: ParentBindingV1::Root,
                },
                role: "reader".into(),
                tier: "dwarf".into(),
                model: "luna".into(),
                effort: "low".into(),
                capabilities: CapabilitySnapshotV1 {
                    sandbox: SandboxCapabilityV1::ReadOnly,
                    write_roots: Vec::new(),
                    tool_names: vec!["read".into()],
                    approval: ApprovalCapabilityV1::Never,
                    mcp: McpCapabilityV1::ReadSnapshotsOnly,
                },
            })
            .unwrap();
        plane.children[0].attempts[0].state = ChildStateV1::Failed {
            class: FailureClassV1::ExternalWriteOutcomeUnknown,
        };
        owner.replace(snapshot.revision, plane).unwrap();
        drop(owner);
        assert!(matches!(
            DesktopRootTransportInspectionV2::inspect(&directory),
            Err(StoreError::RecoveryRequired)
        ));
    }
}
