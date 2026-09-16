//! Host prompt acceptance, raw provenance and confirm-only recovery.
use super::*;
use crate::mcp::results::{ProjectedPart, ReplyProjection, Source, SourceOrigin};
use polaris_desktop_protocol::run_details::RunDetailKind;
use polaris_mcp::{
    OperationBinding, OperationFailure,
    catalog::{ItemId, Kind},
    registration::canonical_hash,
};
use polaris_provider::{Message, Role};
use serde_json::{Value, json};
mod history_tests;

fn choice() -> McpPromptChoice {
    McpPromptChoice {
        server: "fixture".into(),
        prompt: "確認用prompt".into(),
        arguments: json!({"subject":"合成入力"}).as_object().unwrap().clone(),
    }
}
fn selection() -> Request {
    let mut request = start("start-prompt", 0);
    if let RequestBody::RunStart(_, params) = &mut request.body {
        params.mcp_prompt = Some(choice());
    }
    request
}
fn accepted() -> (PrototypeRoot, Writer) {
    let (root, mut writer) = setup();
    writer.apply(&selection(), Some(target())).unwrap();
    (root, writer)
}
fn binding() -> OperationBinding {
    OperationBinding {
        item: ItemId {
            server_id: "fixture".into(),
            kind: Kind::Prompt,
            key: choice().prompt,
        },
        registry_revision: 1,
        destination_hash: "a".repeat(64),
        connection_generation: "c".repeat(32),
        catalog_generation: "d".repeat(64),
        schema_hash: None,
        arguments_hash: canonical_hash(&json!(choice().arguments)).unwrap(),
        selection_id: Some("n".repeat(32)),
        resource_uri: None,
    }
}
fn intent(writer: &Writer) -> McpPromptIntent {
    writer
        .prepare_mcp_prompt_intent(&target(), op(), &binding(), 100)
        .unwrap()
}
fn begun() -> (PrototypeRoot, Writer, McpPromptIntent) {
    let (root, mut writer) = accepted();
    let intent = intent(&writer);
    assert_eq!(
        writer.begin_mcp_prompt(&intent, 1).unwrap(),
        IntentReceipt::NewlyPublished
    );
    (root, writer, intent)
}
fn reply() -> ReplyProjection {
    ReplyProjection {
        source: Source {
            server: "fixture".into(),
            origin: SourceOrigin::HostSelection {
                selection_id: "n".repeat(32),
            },
            result_generation: "response-1".into(),
        },
        status: "complete",
        parts: vec![ProjectedPart::Text {
            text: "未信頼の合成prompt内容".into(),
            uri: None,
            mime: None,
        }],
    }
}

#[test]
fn prompt_projection_pair_proof_rejects_relocated_or_relabelled_raw() {
    let (_root, mut writer, intent) = begun();
    let publication = writer
        .prepare_mcp_prompt_terminal(&intent, McpTerminalInput::Reply(&reply()))
        .unwrap();
    writer.publish_mcp_prompt_terminal(&publication).unwrap();
    let saved = writer.snapshot().unwrap();
    let record = &saved.state.runs[0];
    let receipt = crate::desktop_store::writer::prompt::validate_record(record)
        .unwrap()
        .unwrap();
    receipt
        .validate_pair(
            &saved.marker,
            &record.input.text,
            &saved.raw[0],
            &saved.raw[1],
        )
        .unwrap();
    for mutation in 0..10 {
        let mut marker = saved.marker.clone();
        let mut input = saved.raw[0].clone();
        let mut event = saved.raw[1].clone();
        let mut input_text = record.input.text.clone();
        match mutation {
            0 => event.sequence += 1,
            1 => event.turn_id += 1,
            2 => event.epoch += 1,
            3 => event.starts_turn = true,
            4 => input.starts_turn = false,
            5 => input.message.content.push('x'),
            6 => input_text.push('x'),
            7 => marker.project_id = ProjectId::new("foreign").unwrap(),
            8 => marker.epoch = DecimalU64::new(marker.epoch.get() + 1),
            _ => event
                .message
                .reasoning
                .push(polaris_provider::ReasoningItem {
                    id: "synthetic".into(),
                    encrypted_content: "opaque".into(),
                }),
        }
        assert!(
            receipt
                .validate_pair(&marker, &input_text, &input, &event)
                .is_err(),
            "mutation {mutation}"
        );
    }
    unchanged(&saved, &writer.snapshot().unwrap());
}

#[test]
fn prompt_projection_record_proof_preserves_receipt_and_run_constraints() {
    let (_root, mut writer, intent) = begun();
    let publication = writer
        .prepare_mcp_prompt_terminal(&intent, McpTerminalInput::Reply(&reply()))
        .unwrap();
    writer.publish_mcp_prompt_terminal(&publication).unwrap();
    let saved = writer.snapshot().unwrap();
    let validate = crate::desktop_store::writer::prompt::validate_record;
    assert_eq!(
        validate(&saved.state.runs[0])
            .unwrap()
            .unwrap()
            .disposition(),
        Some(McpDisposition::Completed)
    );
    for mutation in 0..7 {
        let mut record = saved.state.runs[0].clone();
        match mutation {
            0 => record.operations.push(record.operations[0].clone()),
            1 => record.operations[0].result_id = None,
            2 => record.operations[0].result_id = Some(ResultId::new("foreign").unwrap()),
            3 => record.run.run_id = RunId::new("foreign").unwrap(),
            4 => record.run.attempt_id = AttemptId::new("foreign").unwrap(),
            5 => record.policy_revision = DecimalU64::new(999),
            _ => record.mcp_prompt = None,
        }
        assert!(validate(&record).is_err(), "mutation {mutation}");
    }
    let mut record = saved.state.runs[0].clone();
    record.operations.clear();
    record.run.state = RunState::Interrupted;
    assert!(validate(&record).unwrap().is_none());
    record.run.state = RunState::Succeeded;
    assert!(validate(&record).is_err());
    unchanged(&saved, &writer.snapshot().unwrap());
}

#[test]
fn prompt_ledger_acceptance_keeps_old_wire_and_freezes_the_exact_choice() {
    let legacy = start("legacy", 0);
    let value = serde_json::to_value(&legacy).unwrap();
    assert!(value["params"].get("mcp_prompt").is_none());
    let parsed: Request = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(serde_json::to_value(parsed).unwrap(), value);
    let request = selection();
    let wire = serde_json::to_value(&request).unwrap();
    let parsed: Request = serde_json::from_value(wire.clone()).unwrap();
    assert_eq!(parsed, request);
    for invalid in [
        Value::Null,
        json!([]),
        json!({"server":"fixture","prompt":"name","arguments":null}),
        json!({"server":"fixture","prompt":"name","arguments":{},"grant":true}),
    ] {
        let mut bad = wire.clone();
        bad["params"]["mcp_prompt"] = invalid;
        assert!(serde_json::from_value::<Request>(bad).is_err());
    }
    let (_root, mut writer) = accepted();
    let saved = writer.snapshot().unwrap();
    assert_eq!(saved.state.runs[0].mcp_prompt.as_ref(), Some(&choice()));
    assert_eq!(saved.raw.len(), 1);
    writer.apply(&request, Some(target())).unwrap();
    unchanged(&saved, &writer.snapshot().unwrap());
    let mut modified = saved.clone();
    modified.state.runs[0].mcp_prompt.as_mut().unwrap().prompt = "別のprompt".into();
    assert!(writer.commit(modified, false).is_err());
    unchanged(&saved, &writer.snapshot().unwrap());
}

#[test]
fn prompt_ledger_rejects_invalid_or_unbound_choices_before_publication() {
    for mutation in 0..7 {
        let (_root, mut writer) = setup();
        let before = writer.snapshot().unwrap();
        let mut request = selection();
        let RequestBody::RunStart(_, params) = &mut request.body else {
            unreachable!()
        };
        let selected = params.mcp_prompt.as_mut().unwrap();
        match mutation {
            0 => selected.server.clear(),
            1 => selected.server = "a".repeat(65),
            2 => selected.server = "remote/fixture".into(),
            3 => selected.prompt.clear(),
            4 => selected.prompt = "name\n".into(),
            5 => selected.prompt = "x".repeat(16 * 1024 + 1),
            _ => {
                selected
                    .arguments
                    .insert("huge".into(), json!("x".repeat(256 * 1024)));
            }
        }
        assert!(writer.apply(&request, Some(target())).is_err());
        unchanged(&before, &writer.snapshot().unwrap());
    }
    let (_root, mut writer) = setup();
    writer
        .apply(&start("no-prompt", 0), Some(target()))
        .unwrap();
    let before = writer.snapshot().unwrap();
    assert!(
        writer
            .prepare_mcp_prompt_intent(&target(), op(), &binding(), 100)
            .is_err()
    );
    unchanged(&before, &writer.snapshot().unwrap());
}

#[test]
fn prompt_ledger_binds_arguments_selection_destination_and_actual_input() {
    let (_root, writer) = accepted();
    let before = writer.snapshot().unwrap();
    for mutation in 0..10 {
        let mut value = binding();
        match mutation {
            0 => value.item.kind = Kind::Tool,
            1 => value.item.server_id = "foreign".into(),
            2 => value.item.key = "foreign".into(),
            3 => value.arguments_hash = canonical_hash(&json!({})).unwrap(),
            4 => value.selection_id = None,
            5 => value.selection_id = Some("bad\n".into()),
            6 => value.destination_hash = "wrong".into(),
            7 => value.connection_generation = "wrong".into(),
            8 => value.schema_hash = Some("a".repeat(64)),
            _ => value.resource_uri = Some("fixture://resource".into()),
        }
        assert!(
            writer
                .prepare_mcp_prompt_intent(&target(), op(), &value, 100)
                .is_err(),
            "mutation {mutation}"
        );
        unchanged(&before, &writer.snapshot().unwrap());
    }
    let proof = intent(&writer);
    assert_eq!(proof.binding(), binding());
    assert!(proof.reply_budget().unwrap() < crate::mcp::results::MAX_VISIBLE_BYTES);
}

#[test]
fn prompt_ledger_never_rebinds_or_replays_an_existing_intent() {
    let (root, mut writer, proof) = begun();
    let saved = writer.snapshot().unwrap();
    assert_eq!(
        writer.begin_mcp_prompt(&proof, 2).unwrap(),
        IntentReceipt::AlreadyRecorded
    );
    let mut other = binding();
    other.selection_id = Some("z".repeat(32));
    let other = writer
        .prepare_mcp_prompt_intent(&target(), OperationId::new("second").unwrap(), &other, 100)
        .unwrap();
    assert!(writer.begin_mcp_prompt(&other, 2).is_err());
    unchanged(&saved, &writer.snapshot().unwrap());
    let (_foreign, mut foreign) = accepted();
    assert!(foreign.begin_mcp_prompt(&proof, 2).is_err());
    drop(writer);
    let mut writer = root.open(&project(), &session()).unwrap();
    assert_eq!(
        writer.snapshot().unwrap().state.runs[0].run.state,
        RunState::OutcomeUnknown
    );
    assert_eq!(
        writer.begin_mcp_prompt(&proof, 2).unwrap(),
        IntentReceipt::AlreadyRecorded
    );
    assert!(writer.begin_mcp_prompt(&other, 2).is_err());
}

#[test]
fn prompt_ledger_acceptance_reopens_stopped_and_never_starts_late() {
    let (root, writer) = accepted();
    let proof = intent(&writer);
    drop(writer);
    let mut writer = root.open(&project(), &session()).unwrap();
    assert_eq!(
        writer.snapshot().unwrap().state.runs[0].run.state,
        RunState::Interrupted
    );
    assert!(writer.begin_mcp_prompt(&proof, 1).is_err());
    let (_root, mut writer) = accepted();
    let proof = intent(&writer);
    assert!(writer.begin_mcp_prompt(&proof, 100).is_err());
    writer.set_policy_revision(DecimalU64::new(8)).unwrap();
    assert!(writer.begin_mcp_prompt(&proof, 1).is_err());
}

#[test]
fn prompt_ledger_publishes_supplement_without_rewriting_user_or_worker_suffix() {
    let (root, mut writer) = accepted();
    let main = OperationId::new("main").unwrap();
    // Production records the run tracking intent before worker preparation.
    writer.record_intent(&target(), main.clone()).unwrap();
    let original = writer.snapshot().unwrap();
    let proof = intent(&writer);
    writer.begin_mcp_prompt(&proof, 1).unwrap();
    let publication = writer
        .prepare_mcp_prompt_terminal(&proof, McpTerminalInput::Reply(&reply()))
        .unwrap();
    let received = writer.publish_mcp_prompt_terminal(&publication).unwrap();
    assert!(received.permits_continuation());
    assert_eq!(received.message.role, Role::User);
    let saved = writer.snapshot().unwrap();
    assert_eq!(saved.raw.len(), 2);
    assert_eq!(
        serde_json::to_value(&saved.raw[0]).unwrap(),
        serde_json::to_value(&original.raw[0]).unwrap()
    );
    assert!(!saved.raw[1].starts_turn);
    assert_eq!(saved.raw[1].turn_id, saved.raw[0].turn_id);
    assert_eq!(saved.raw[1].sequence, saved.raw[0].sequence + 1);
    assert!(saved.raw[1].message.tool_calls.is_empty());
    assert!(saved.raw[1].message.tool_call_id.is_none());
    writer.publish_mcp_prompt_terminal(&publication).unwrap();
    writer.confirm_mcp_prompt_terminal(&publication).unwrap();
    unchanged(&saved, &writer.snapshot().unwrap());
    let suffix = vec![Message::assistant("通常の返答")];
    writer
        .finish_with_messages(
            &target(),
            &main,
            Observation::Succeeded,
            result(),
            suffix.clone(),
        )
        .unwrap();
    let complete = writer.snapshot().unwrap();
    assert_eq!(complete.raw.len(), 3);
    assert_eq!(complete.raw[2].message.content, "通常の返答");
    let details = writer.project_run_details(&target()).unwrap();
    let kinds: Vec<_> = details.entries.iter().map(|entry| entry.kind).collect();
    assert_eq!(
        kinds,
        [
            RunDetailKind::UserInput,
            RunDetailKind::McpPrompt,
            RunDetailKind::Assistant
        ]
    );
    assert_eq!(details.entries[1].text, complete.raw[1].message.content);
    writer
        .finish_with_messages(&target(), &main, Observation::Succeeded, result(), suffix)
        .unwrap();
    unchanged(&complete, &writer.snapshot().unwrap());
    drop(writer);
    let mut reopened = root.open(&project(), &session()).unwrap();
    assert!(
        reopened
            .confirm_mcp_prompt_terminal(&publication)
            .unwrap()
            .permits_continuation()
    );
    unchanged(&complete, &reopened.snapshot().unwrap());
}

#[test]
fn prompt_ledger_cannot_complete_without_a_prompt_or_borrow_a_model_result() {
    let (_root, mut writer) = accepted();
    let main = OperationId::new("main").unwrap();
    writer.record_intent(&target(), main.clone()).unwrap();
    let before = writer.snapshot().unwrap();
    assert!(
        writer
            .finish_with_messages(
                &target(),
                &main,
                Observation::Succeeded,
                result(),
                vec![Message::assistant("not sent")]
            )
            .is_err()
    );
    unchanged(&before, &writer.snapshot().unwrap());
    let proof = intent(&writer);
    writer.begin_mcp_prompt(&proof, 1).unwrap();
    let before = writer.snapshot().unwrap();
    for mutation in 0..4 {
        let mut value = reply();
        match mutation {
            0 => {
                value.source.origin = SourceOrigin::ModelCall {
                    model_call: "n".repeat(32),
                }
            }
            1 => value.source.server = "other".into(),
            2 => {
                value.source.origin = SourceOrigin::HostSelection {
                    selection_id: "z".repeat(32),
                }
            }
            _ => {
                value.parts = vec![ProjectedPart::Text {
                    text: "x".repeat(32 * 1024),
                    uri: None,
                    mime: None,
                }]
            }
        }
        assert!(
            writer
                .prepare_mcp_prompt_terminal(&proof, McpTerminalInput::Reply(&value))
                .is_err()
        );
        unchanged(&before, &writer.snapshot().unwrap());
    }
    assert!(
        writer
            .record_operation_result(&target(), &op(), result())
            .is_err()
    );
    unchanged(&before, &writer.snapshot().unwrap());
}

#[test]
fn prompt_ledger_terminal_classifications_stop_before_main_and_survive_restart() {
    for disposition in [
        McpDisposition::BeforeSend,
        McpDisposition::OutcomeUnknown,
        McpDisposition::InputRequired,
    ] {
        let (root, mut writer, proof) = begun();
        let mut response = reply();
        response.status = "input_required";
        response.parts = vec![ProjectedPart::InputRequired {
            request: json!({"question":"合成質問"}),
        }];
        let value = match disposition {
            McpDisposition::BeforeSend => McpTerminalInput::Failure(OperationFailure::BeforeSend(
                polaris_mcp::Error::Cancelled,
            )),
            McpDisposition::OutcomeUnknown => McpTerminalInput::Failure(
                OperationFailure::OutcomeUnknown(polaris_mcp::Error::Transport),
            ),
            _ => McpTerminalInput::Reply(&response),
        };
        let publication = writer.prepare_mcp_prompt_terminal(&proof, value).unwrap();
        let received = writer.publish_mcp_prompt_terminal(&publication).unwrap();
        assert_eq!(received.disposition, disposition);
        assert!(!received.permits_continuation());
        assert!(
            writer
                .finish(&target(), Observation::Succeeded, result())
                .is_err()
        );
        drop(writer);
        let mut writer = root.open(&project(), &session()).unwrap();
        assert_eq!(
            writer.snapshot().unwrap().state.runs[0].run.state,
            if disposition == McpDisposition::OutcomeUnknown {
                RunState::OutcomeUnknown
            } else {
                RunState::Interrupted
            }
        );
        assert_eq!(
            writer
                .confirm_mcp_prompt_terminal(&publication)
                .unwrap()
                .disposition,
            disposition
        );
    }
}

#[test]
fn prompt_ledger_intent_ack_is_confirm_only_and_never_returns_a_fresh_grant() {
    for point in ["sidecar_sync", "marker_rename", "directory_sync"] {
        let (root, mut writer) = accepted();
        let proof = intent(&writer);
        inject(Some(point));
        let failed = writer.begin_mcp_prompt(&proof, 1);
        inject(None);
        assert!(failed.is_err());
        let exists = writer.confirm_mcp_prompt_intent(&proof).unwrap();
        assert_eq!(exists, point == "directory_sync");
        let saved = writer.snapshot().unwrap();
        assert_eq!(saved.raw.len(), 1);
        assert_eq!(saved.state.runs[0].operations.len(), usize::from(exists));
        drop(writer);
        let mut writer = root.open(&project(), &session()).unwrap();
        if exists {
            assert_eq!(
                writer.begin_mcp_prompt(&proof, 2).unwrap(),
                IntentReceipt::AlreadyRecorded
            );
        } else {
            assert!(writer.begin_mcp_prompt(&proof, 2).is_err());
        }
    }
}

#[test]
fn prompt_ledger_terminal_ack_never_republishes_a_missing_supplement() {
    for point in ["raw_sync", "marker_rename", "directory_sync"] {
        let (root, mut writer, proof) = begun();
        let publication = writer
            .prepare_mcp_prompt_terminal(&proof, McpTerminalInput::Reply(&reply()))
            .unwrap();
        inject(Some(point));
        let failed = writer.publish_mcp_prompt_terminal(&publication);
        inject(None);
        assert!(failed.is_err());
        let exists = writer.confirm_mcp_prompt_terminal(&publication).is_ok();
        assert_eq!(exists, point == "directory_sync");
        let saved = writer.snapshot().unwrap();
        assert_eq!(saved.raw.len(), if exists { 2 } else { 1 });
        drop(writer);
        let writer = root.open(&project(), &session()).unwrap();
        assert_eq!(
            writer.snapshot().unwrap().state.runs[0].run.state,
            if exists {
                RunState::Interrupted
            } else {
                RunState::OutcomeUnknown
            }
        );
    }
}

#[test]
fn prompt_ledger_rejects_extra_provider_metadata_on_saved_supplement() {
    use crate::conversation_state::content_hash;
    use polaris_provider::ReasoningItem;

    let (root, mut writer, proof) = begun();
    let publication = writer
        .prepare_mcp_prompt_terminal(&proof, McpTerminalInput::Reply(&reply()))
        .unwrap();
    writer.publish_mcp_prompt_terminal(&publication).unwrap();
    writer
        .finish(&target(), Observation::Succeeded, result())
        .unwrap();
    let original = writer.snapshot().unwrap();
    let directory = dir(&writer);
    drop(writer);
    let writer = root.open(&project(), &session()).unwrap();
    unchanged(&original, &writer.snapshot().unwrap());
    drop(writer);

    let original_raw = fs::read(directory.join(disk::RAW)).unwrap();
    let original_marker = fs::read(directory.join(disk::MARKER)).unwrap();
    let original_sidecar = fs::read(directory.join(disk::SIDECAR)).unwrap();
    let mut raw = original.raw.clone();
    assert_eq!(raw.len(), 2);
    raw[1].message.reasoning.push(ReasoningItem {
        id: "synthetic".into(),
        encrypted_content: "opaque".into(),
    });
    let raw_bytes: Vec<u8> = raw
        .iter()
        .flat_map(|event| disk::line(event).unwrap())
        .collect();
    let mut marker = original.marker.clone();
    marker.raw_offset = DecimalU64::new(raw_bytes.len() as u64);
    marker.raw_hash = content_hash(&raw_bytes);
    let marker_bytes = serde_json::to_vec(&marker).unwrap();
    fs::write(directory.join(disk::RAW), &raw_bytes).unwrap();
    fs::write(directory.join(disk::MARKER), &marker_bytes).unwrap();
    assert!(matches!(
        root.open(&project(), &session()),
        Err(StoreError::Corrupt("MCP prompt補助原文の対応"))
    ));
    assert_eq!(fs::read(directory.join(disk::RAW)).unwrap(), raw_bytes);
    assert_eq!(
        fs::read(directory.join(disk::MARKER)).unwrap(),
        marker_bytes
    );
    assert_eq!(
        fs::read(directory.join(disk::SIDECAR)).unwrap(),
        original_sidecar
    );

    fs::write(directory.join(disk::RAW), original_raw).unwrap();
    fs::write(directory.join(disk::MARKER), original_marker).unwrap();
    let writer = root.open(&project(), &session()).unwrap();
    unchanged(&original, &writer.snapshot().unwrap());
}
