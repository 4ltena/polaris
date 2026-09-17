//! Real writer publications, then independently re-hashed invalid frozen files.
use super::*;
use crate::desktop_store::{McpDisposition, McpTerminalInput};
use crate::mcp::results::{ProjectedPart, ReplyProjection, Source, SourceOrigin};
use polaris_mcp::{
    OperationBinding, OperationFailure,
    catalog::{ItemId, Kind},
};
use polaris_provider::{Message, Role, ToolCall};
use serde_json::{Value, json};
mod history_tests;
mod ready_tests;

fn selected(stage: u8) -> (PrototypeRoot, Writer, Writer) {
    selected_after_inheritance(stage, 0)
}

fn selected_after_inheritance(stage: u8, inherited: u64) -> (PrototypeRoot, Writer, Writer) {
    let turns = inherited.max(1);
    let (root, mut parent, target) = setup_turns(
        "memory-project",
        "prompt-parent",
        HistoryMode::Strict10,
        turns,
    );
    finish(&mut parent, &target, turns);
    if inherited > 0 {
        parent = parent
            .fork_memory(SessionId::new("inherited-parent").unwrap(), "fixture")
            .unwrap();
    }
    let child = parent
        .fork_memory(SessionId::new("prompt-reader").unwrap(), "fixture")
        .unwrap();
    let saved = parent.snapshot().unwrap();
    let target = RunTarget {
        run_id: RunId::new("prompt-run").unwrap(),
        attempt_id: AttemptId::new("prompt-attempt").unwrap(),
    };
    let choice = McpPromptChoice {
        server: "fixture".into(),
        prompt: "note".into(),
        arguments: Default::default(),
    };
    parent
        .apply(
            &request(
                "prompt-draft",
                RequestBody::DraftUpdate(
                    saved.marker.session_id.clone(),
                    DraftUpdate {
                        expected_draft_revision: saved.state.draft.draft_revision,
                        text: "real user input".into(),
                        attachment_ids: vec![],
                    },
                ),
            ),
            None,
        )
        .unwrap();
    let saved = parent.snapshot().unwrap();
    parent
        .apply(
            &request(
                "prompt-start",
                RequestBody::RunStart(
                    saved.marker.session_id.clone(),
                    RunStart {
                        mcp_prompt: Some(choice),
                        expected_draft_revision: saved.state.draft.draft_revision,
                        expected_configuration_revision: saved
                            .state
                            .configuration
                            .configuration_revision,
                        expected_policy_revision: saved.state.policy_revision,
                    },
                ),
            ),
            Some(target.clone()),
        )
        .unwrap();
    if stage == 0 {
        return (root, parent, child);
    }
    let binding = OperationBinding {
        item: ItemId {
            server_id: "fixture".into(),
            kind: Kind::Prompt,
            key: "note".into(),
        },
        registry_revision: 1,
        destination_hash: "a".repeat(64),
        connection_generation: "c".repeat(32),
        catalog_generation: "d".repeat(64),
        schema_hash: None,
        arguments_hash: polaris_mcp::registration::canonical_hash(&json!({})).unwrap(),
        selection_id: Some("s".repeat(32)),
        resource_uri: None,
    };
    let intent = parent
        .prepare_mcp_prompt_intent(
            &target,
            OperationId::new("prompt-op").unwrap(),
            &binding,
            100,
        )
        .unwrap();
    parent.begin_mcp_prompt(&intent, 1).unwrap();
    if stage == 1 {
        return (root, parent, child);
    }
    let mut reply = ReplyProjection {
        source: Source {
            server: "fixture".into(),
            origin: SourceOrigin::HostSelection {
                selection_id: "s".repeat(32),
            },
            result_generation: "result-1".into(),
        },
        status: "complete",
        parts: vec![ProjectedPart::Text {
            text: "untrusted server supplement".into(),
            uri: None,
            mime: None,
        }],
    };
    if stage == 4 {
        reply.status = "input_required";
        reply.parts = vec![ProjectedPart::InputRequired {
            request: json!({"question":"fixture"}),
        }];
    }
    let terminal = match stage {
        2 | 4 => McpTerminalInput::Reply(&reply),
        3 => McpTerminalInput::Failure(OperationFailure::BeforeSend(polaris_mcp::Error::Transport)),
        _ => McpTerminalInput::Failure(OperationFailure::OutcomeUnknown(
            polaris_mcp::Error::Transport,
        )),
    };
    let publication = parent
        .prepare_mcp_prompt_terminal(&intent, terminal)
        .unwrap();
    let saved = parent.publish_mcp_prompt_terminal(&publication).unwrap();
    assert_eq!(
        saved.disposition,
        match stage {
            2 => McpDisposition::Completed,
            3 => McpDisposition::BeforeSend,
            4 => McpDisposition::InputRequired,
            _ => McpDisposition::OutcomeUnknown,
        }
    );
    (root, parent, child)
}

fn load(child: &Writer, marker: Marker) -> StoreResult<()> {
    frozen::FrozenMemory::load(
        child,
        Default::default(),
        &fork::FrozenAncestor {
            marker,
            generation: DecimalU64::new(0),
            summary_ids: vec![],
            structured_ids: vec![],
        },
        &mut |_| Ok(()),
    )
    .map(|_| ())
}

fn write_raw(path: &Path, mut marker: Marker, events: &[RawEventV2]) -> Marker {
    let data: Vec<u8> = events.iter().flat_map(|e| disk::line(e).unwrap()).collect();
    fs::write(path.join(disk::RAW), &data).unwrap();
    marker.raw_offset = DecimalU64::new(data.len() as u64);
    marker.raw_hash = content_hash(&data);
    fs::write(
        path.join(disk::MARKER),
        serde_json::to_vec(&marker).unwrap(),
    )
    .unwrap();
    marker
}

#[test]
fn frozen_prompt_preserves_selected_intent_and_all_terminal_states_in_both_field_orders() {
    for stage in 0..6 {
        let (_root, parent, child) = selected(stage);
        let saved = parent.snapshot().unwrap();
        let path = parent.test_directory();
        let before = bytes(&path);
        // The regular receipt reader also loads its current running owner.
        load(&parent, saved.marker.clone()).unwrap();
        drop(parent);
        load(&child, saved.marker.clone()).unwrap();
        assert_eq!(bytes(&path), before);
        let marker = replace_last_state(&path, &before[1], saved.marker, |_| {});
        let reordered = bytes(&path);
        load(&child, marker).unwrap();
        assert_eq!(bytes(&path), reordered);
    }
}

#[test]
fn frozen_prompt_rejects_receipt_input_run_and_approval_mismatches() {
    let (_root, parent, child) = selected(2);
    let saved = parent.snapshot().unwrap();
    let path = parent.test_directory();
    let original = bytes(&path);
    drop(parent);
    load(&child, saved.marker.clone()).unwrap();
    for mutation in 0..10 {
        let marker = replace_last_state(&path, &original[1], saved.marker.clone(), |state| {
            let run = &mut state["runs"][1];
            match mutation {
                0 => run["operations"][0]["result_id"] = Value::Null,
                1 => run["operations"][0]["result_id"] = "other-result".into(),
                2 => run["policy_revision"] = "9".into(),
                3 => run["run"]["run_id"] = "other-run".into(),
                4 => run["mcp_prompt"] = Value::Null,
                5 => {
                    run["operations"][0]["prompt"]["intent"]["input"]["hash"] =
                        "0".repeat(64).into()
                }
                6 => run["operations"][0]["prompt"]["intent"]["input"]["sequence"] = 1.into(),
                7 => state["inherited_turn_count"] = "1".into(),
                8 => {
                    state["runs"].as_array_mut().unwrap().pop();
                }
                _ => {
                    state["approval_records"] = json!([{
                        "pending": {"approval_id":"invalidated-prompt", "run_id":"prompt-run",
                            "attempt_id":"prompt-attempt", "operation_id":"prompt-op",
                            "operation":"fixture", "scope":"fixture", "payload_hash":"a".repeat(64),
                            "policy_revision":"0", "expires_at_unix_ms":"100", "state":"pending",
                            "display":{"title":"fixture", "description":"fixture", "choices":["allow","deny"]}},
                        "decision":null, "consumed":false, "invalidated":true
                    }])
                }
            }
        });
        let before = bytes(&path);
        let result = load(&child, marker);
        if mutation == 9 {
            assert!(matches!(
                result,
                Err(StoreError::Corrupt("MCP prompt操作の対応"))
            ));
            // Typed serialization puts approval_records after runs; Value puts
            // it before runs. Both must reach the same receipt check.
            let mut marker: Marker = serde_json::from_slice(&before[0]).unwrap();
            let last = before[1][..before[1].len() - 1]
                .iter()
                .rposition(|b| *b == b'\n')
                .unwrap()
                + 1;
            let state: Sidecar = serde_json::from_slice(&before[1][last..]).unwrap();
            let mut data = before[1][..last].to_vec();
            data.extend(disk::line(&state).unwrap());
            marker.sidecar_hash = content_hash(&data);
            marker.sidecar_offset = DecimalU64::new(data.len() as u64);
            fs::write(path.join(disk::SIDECAR), data).unwrap();
            fs::write(
                path.join(disk::MARKER),
                serde_json::to_vec(&marker).unwrap(),
            )
            .unwrap();
            let reordered = bytes(&path);
            assert!(matches!(
                load(&child, marker),
                Err(StoreError::Corrupt("MCP prompt操作の対応"))
            ));
            assert_eq!(bytes(&path), reordered);
            continue;
        }
        assert!(result.is_err(), "mutation {mutation}");
        assert_eq!(bytes(&path), before);
    }
}

#[test]
fn frozen_prompt_preserves_local_ordinals_after_one_and_ten_inherited_turns() {
    for inherited in [1, 10] {
        let (_root, parent, child) = selected_after_inheritance(2, inherited);
        let saved = parent.snapshot().unwrap();
        assert_eq!(saved.state.inherited_turn_count.get(), inherited);
        assert_eq!(saved.state.runs.len(), 1);
        assert_eq!(saved.raw.last().unwrap().turn_id, inherited + 1);
        assert_eq!(
            child
                .snapshot()
                .unwrap()
                .state
                .conversation_memory
                .unwrap()
                .ancestors
                .len(),
            2
        );
        let path = parent.test_directory();
        let before = bytes(&path);
        drop(parent);
        load(&child, saved.marker.clone()).unwrap();
        assert_eq!(bytes(&path), before);
        let marker = replace_last_state(&path, &before[1], saved.marker, |_| {});
        load(&child, marker).unwrap();
    }
}

#[test]
fn frozen_prompt_suffix_retention_enforces_the_existing_message_count_bound() {
    let (_root, parent, child) = selected(2);
    let saved = parent.snapshot().unwrap();
    let path = parent.test_directory();
    drop(parent);
    let mut events = saved.raw.clone();
    for _ in 0..256 {
        events.push(RawEventV2 {
            schema_version: 2,
            sequence: events.len() as u64 + 1,
            epoch: saved.marker.epoch.get(),
            turn_id: 2,
            starts_turn: false,
            message: Message::assistant("part"),
        });
    }
    let marker = write_raw(&path, saved.marker.clone(), &events);
    load(&child, marker).unwrap();
    events.push(RawEventV2 {
        schema_version: 2,
        sequence: events.len() as u64 + 1,
        epoch: saved.marker.epoch.get(),
        turn_id: 2,
        starts_turn: false,
        message: Message::assistant("too many"),
    });
    let marker = write_raw(&path, saved.marker, &events);
    assert!(matches!(load(&child, marker), Err(StoreError::Overflow)));
}

#[test]
fn frozen_prompt_rejects_missing_or_relabelled_supplement_at_eof_and_before_next_turn() {
    let (_root, parent, child) = selected(2);
    let saved = parent.snapshot().unwrap();
    let path = parent.test_directory();
    drop(parent);
    for mutation in 0..6 {
        let mut events = saved.raw.clone();
        match mutation {
            0 => {
                events.pop();
            }
            1 => events.last_mut().unwrap().message.content.push('x'),
            2 => events.last_mut().unwrap().message = Message::assistant("forged suffix"),
            3 => events[2].message.content.push('x'),
            4 => events.last_mut().unwrap().message.role = Role::Tool,
            _ => {
                let last = events.last_mut().unwrap();
                last.starts_turn = true;
                last.turn_id += 1;
            }
        }
        let marker = write_raw(&path, saved.marker.clone(), &events);
        let before = bytes(&path);
        assert!(load(&child, marker).is_err(), "mutation {mutation}");
        assert_eq!(bytes(&path), before);
    }
    let marker = write_raw(&path, saved.marker, &saved.raw);
    load(&child, marker).unwrap();
}

#[test]
fn frozen_prompt_requires_confirmation_before_suffix_and_validates_tool_groups_together() {
    for stage in 0..6 {
        let (_root, parent, child) = selected(stage);
        let saved = parent.snapshot().unwrap();
        let path = parent.test_directory();
        drop(parent);
        for complete in [false, true] {
            let mut messages = vec![Message::assistant_with_tool_calls(
                "",
                vec![ToolCall {
                    id: "tool-1".into(),
                    name: "fixture".into(),
                    arguments: json!({}),
                }],
            )];
            if complete {
                messages.push(Message::tool_result("tool-1", "result"));
                messages.push(Message::assistant("answer"));
            }
            let mut events = saved.raw.clone();
            for message in messages {
                events.push(RawEventV2 {
                    schema_version: 2,
                    sequence: events.len() as u64 + 1,
                    epoch: saved.marker.epoch.get(),
                    turn_id: 2,
                    starts_turn: false,
                    message,
                });
            }
            let marker = write_raw(&path, saved.marker.clone(), &events);
            let before = bytes(&path);
            assert_eq!(
                load(&child, marker).is_ok(),
                stage == 2,
                "stage {stage}, complete {complete}"
            );
            assert_eq!(bytes(&path), before);
            if stage == 2 && complete {
                events.last_mut().unwrap().message = Message::tool_result("missing-call", "result");
                let marker = write_raw(&path, saved.marker.clone(), &events);
                assert!(load(&child, marker).is_err());
            }
        }
    }
}

#[test]
fn frozen_prompt_suffix_retention_matches_the_exact_four_mib_json_bound() {
    let (_root, parent, child) = selected(2);
    let saved = parent.snapshot().unwrap();
    let path = parent.test_directory();
    drop(parent);
    let mut messages = vec![Message::assistant("x".repeat(65_500)); 63];
    messages.push(Message::assistant(""));
    let padding = 4 * 1024 * 1024 - serde_json::to_vec(&messages).unwrap().len();
    messages.last_mut().unwrap().content = "x".repeat(padding);
    assert_eq!(
        serde_json::to_vec(&messages).unwrap().len(),
        4 * 1024 * 1024
    );
    crate::desktop_store::writer::validate_turn_suffix(&messages, Observation::Cancelled).unwrap();
    let mut events = saved.raw.clone();
    for message in messages {
        events.push(RawEventV2 {
            schema_version: 2,
            sequence: events.len() as u64 + 1,
            epoch: saved.marker.epoch.get(),
            turn_id: 2,
            starts_turn: false,
            message,
        });
    }
    let marker = write_raw(&path, saved.marker.clone(), &events);
    load(&child, marker).unwrap();
    events.last_mut().unwrap().message.content.push('x');
    let marker = write_raw(&path, saved.marker, &events);
    assert!(load(&child, marker).is_err());
}
