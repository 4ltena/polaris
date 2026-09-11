//! 手書きfixtureと対比し、全method、厳格な構文、snapshotの復元情報と相関を検査する。

use polaris_desktop_protocol::{
    codec::{self, CodecError, Decode},
    event::{Durability, Event, EventBody},
    ids::*,
    request::{HistoryPage, Request, RequestBody},
    response::{CorrelationError, RequestStatusResult, Response, SuccessResult},
    snapshot::{AcceptanceState, BlockerKind, Snapshot},
    validate::{ConnectionState, GateError, validate_request},
};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};
use std::fmt::Debug;

fn values(source: &str) -> Vec<Value> {
    serde_json::from_str(source).unwrap()
}
fn read<T: DeserializeOwned>(value: &Value) -> T {
    codec::from_json(&serde_json::to_vec(value).unwrap()).unwrap()
}
fn assert_wire<T: DeserializeOwned + Serialize + Debug + PartialEq>(value: &Value) -> T {
    let typed: T = read(value);
    assert_eq!(serde_json::to_value(&typed).unwrap(), *value);
    let frame = codec::encode(&typed).unwrap();
    assert_eq!(
        codec::decode::<T>(&frame),
        Decode::Decoded {
            value: read(value),
            consumed: frame.len()
        }
    );
    typed
}
fn requests() -> Vec<Value> {
    values(include_str!("fixtures/requests.json"))
}
fn results() -> Vec<Value> {
    values(include_str!("fixtures/results.json"))
}

#[test]
fn every_method_has_handwritten_request_and_corresponding_result() {
    let requests = requests();
    let results = results();
    assert_eq!(requests.len(), 12);
    assert_eq!(results.len(), requests.len());
    for (request, result) in requests.iter().zip(&results) {
        let request = assert_wire::<Request>(request);
        let result = assert_wire::<SuccessResult>(result);
        assert_eq!(result.method(), request.body.method());
        let response = Response::for_request(&request, Ok(result)).unwrap();
        let expected = json!({"protocol_version":1,"kind":"response","client_id":request.client_id,"request_id":request.request_id,"result":result_wire(&response)});
        assert_wire::<Response>(&expected)
            .validate_for(&request)
            .unwrap();
    }
}

fn result_wire(response: &Response) -> Value {
    serde_json::to_value(response.outcome.as_ref().unwrap()).unwrap()
}

#[test]
fn m1_draft_example_accepts_omitted_attachments_but_not_null() {
    let mut wire = requests()[5].clone();
    let request: Request = read(&wire);
    let RequestBody::DraftUpdate(session, draft) = request.body else {
        panic!("draft request")
    };
    assert_eq!(session.as_str(), "session-example");
    assert_eq!(draft.expected_draft_revision.get(), 4);
    assert_eq!(draft.text, "境界条件を確認する");
    assert!(draft.attachment_ids.is_empty());
    wire["params"]["attachment_ids"] = Value::Null;
    assert_eq!(
        codec::from_json::<Request>(&serde_json::to_vec(&wire).unwrap()),
        Err(CodecError::Null)
    );
    wire["params"]["attachment_ids"] = json!(["attachment"]);
    assert_wire::<Request>(&wire);
}

#[test]
fn response_success_and_error_are_exclusive_and_correlated() {
    let requests = requests();
    for (wire, index) in values(include_str!("fixtures/responses.json"))
        .iter()
        .zip([8, 7])
    {
        let response = assert_wire::<Response>(wire);
        response.validate_for(&read(&requests[index])).unwrap();
        let mut both = wire.clone();
        both["error"] = json!({"code":"invalid_request","message":"合成"});
        both["result"] = results()[index].clone();
        assert!(codec::from_json::<Response>(&serde_json::to_vec(&both).unwrap()).is_err());
        let mut neither = wire.clone();
        neither.as_object_mut().unwrap().remove("result");
        neither.as_object_mut().unwrap().remove("error");
        assert!(codec::from_json::<Response>(&serde_json::to_vec(&neither).unwrap()).is_err());
    }
    let request: Request = read(&requests[8]);
    let mut response = Response::for_request(&request, Ok(read(&results()[8]))).unwrap();
    response.outcome = Ok(read(&results()[7]));
    assert_eq!(
        response.validate_for(&request),
        Err(CorrelationError::Method)
    );
    response.request_id = RequestId::new("different").unwrap();
    assert_eq!(
        response.validate_for(&request),
        Err(CorrelationError::RequestId)
    );
    response.client_id = ClientId::new("different").unwrap();
    assert_eq!(
        response.validate_for(&request),
        Err(CorrelationError::ClientId)
    );
    assert_eq!(
        Response::for_request(&request, Ok(read(&results()[0]))),
        Err(CorrelationError::Method)
    );
}

#[test]
fn snapshot_preserves_approval_acceptance_blockers_and_history_start() {
    let wire: Value = serde_json::from_str(include_str!("fixtures/snapshot.json")).unwrap();
    let snapshot = assert_wire::<Snapshot>(&wire);
    assert_eq!(snapshot.session_revision.get(), u64::MAX);
    assert_eq!(snapshot.content_revision.get(), 9_007_199_254_740_993);
    assert_eq!(snapshot.unresolved_approvals.len(), 1);
    let approval = &snapshot.unresolved_approvals[0];
    assert_eq!(approval.run_id, snapshot.runs[0].run_id);
    assert_eq!(approval.attempt_id, snapshot.runs[0].attempt_id);
    assert_eq!(approval.payload_hash, "sha256:synthetic");
    assert_eq!(approval.policy_revision.get(), 2);
    assert_eq!(approval.expires_at_unix_ms.get(), 1_893_456_000_000);
    let task = &snapshot.tasks[0];
    assert_eq!(task.acceptance[0].state, AcceptanceState::Passed);
    assert_eq!(task.acceptance[0].evidence, "合成の確認記録");
    assert_eq!(task.blockers[0].kind, BlockerKind::OutcomeUnknown);
    assert_eq!(task.blockers[0].run_id, snapshot.children[0].run_id);
    assert_eq!(snapshot.runs[0].task_ids, vec![task.task_id.clone()]);
    assert_eq!(snapshot.children[0].task_ids, vec![task.task_id.clone()]);
    let page = HistoryPage {
        snapshot_id: snapshot.snapshot_id,
        cursor: snapshot.history_start_cursor,
        limit: polaris_desktop_protocol::request::PageLimit::new(32).unwrap(),
    };
    assert_eq!(
        serde_json::to_value(page).unwrap(),
        json!({"snapshot_id":"snap","cursor":"history-start","limit":32})
    );
}

#[test]
fn strict10_configuration_and_memory_status_roundtrip_without_nulls() {
    use polaris_desktop_protocol::snapshot::HistoryMode;
    let mut wire: Value = serde_json::from_str(include_str!("fixtures/snapshot.json")).unwrap();
    let old: Snapshot = read(&wire);
    assert_eq!(old.configuration.history_mode, HistoryMode::Legacy);
    assert!(old.memory.is_none());
    wire["configuration"]["history_mode"] = json!("strict10");
    wire["memory"] = json!({"run_id":"run-memory", "phase":"ready", "detail":"準備完了",
        "recent_raw_turns":"10", "retrieval_sources":["conversation://source-1-abcd?start=1&end=1"], "reference_tokens":"256",
        "summary_usage":{"input_tokens":"100", "output_tokens":"10", "cached_tokens":"20",
            "reported_responses":"1", "missing_responses":"0", "failed_requests":"0"}});
    let updated = assert_wire::<Snapshot>(&wire);
    assert_eq!(updated.configuration.history_mode, HistoryMode::Strict10);
    assert!(updated.memory.unwrap().embedding_usage.is_none());
    let mut event = values(include_str!("fixtures/events.json"))[0].clone();
    event["type"] = json!("memory.updated");
    event["payload"] = wire["memory"].clone();
    assert!(matches!(
        assert_wire::<Event>(&event).body,
        EventBody::MemoryUpdated(_)
    ));
    let mut request = requests()
        .into_iter()
        .find(|v| v["method"] == "session.configure")
        .unwrap();
    request["params"]["history_mode"] = json!("strict10");
    assert_wire::<Request>(&request);
    request["params"]["history_mode"] = json!("invented");
    assert!(codec::from_json::<Request>(&serde_json::to_vec(&request).unwrap()).is_err());
    wire["memory"]["embedding_usage"] = Value::Null;
    assert_eq!(
        codec::from_json::<Snapshot>(&serde_json::to_vec(&wire).unwrap()),
        Err(CodecError::Null)
    );
}

#[test]
fn status_not_found_accepted_completed_unknown_remain_distinct() {
    let wire = values(include_str!("fixtures/status.json"));
    let typed: Vec<_> = wire
        .iter()
        .map(assert_wire::<RequestStatusResult>)
        .collect();
    assert!(matches!(typed[0], RequestStatusResult::NotFound {}));
    assert!(matches!(typed[1], RequestStatusResult::Accepted { .. }));
    assert!(matches!(typed[2], RequestStatusResult::Completed { .. }));
    assert!(matches!(
        typed[3],
        RequestStatusResult::OutcomeUnknown { .. }
    ));
    assert_ne!(wire[0], wire[3]);
    let status: Request = read(&requests()[10]);
    let RequestBody::RequestStatus(_, query) = status.body else {
        panic!("status")
    };
    assert_ne!(query.client_id, status.client_id);
    assert_ne!(query.request_id, status.request_id);
}

#[test]
fn event_fixtures_preserve_byte_offsets_epoch_and_tentative_content() {
    let wire = values(include_str!("fixtures/events.json"));
    for value in &wire {
        assert_wire::<Event>(value);
    }
    let event: Event = read(&wire[0]);
    assert_eq!(event.engine_epoch.as_str(), "e");
    assert_eq!(event.subscription_id.as_str(), "sub");
    assert_eq!(event.event_seq.get(), 9_007_199_254_740_993);
    let EventBody::MessageDelta(mut delta) = event.body else {
        panic!("delta")
    };
    assert_eq!(delta.durability, Durability::Tentative);
    assert_eq!(delta.text.chars().count(), 4);
    assert_eq!(delta.end_offset().unwrap().get(), 22);
    delta.byte_offset = DecimalU64::new(u64::MAX);
    assert_eq!(delta.end_offset(), Err(ValueError::Overflow));
    let mut unknown = wire[1].clone();
    unknown["type"] = json!("run.future_state");
    assert_eq!(
        codec::from_json::<Event>(&serde_json::to_vec(&unknown).unwrap()),
        Err(CodecError::UnknownEvent)
    );
    unknown = wire[1].clone();
    unknown["payload"]["state"] = json!("future_state");
    assert_eq!(
        codec::from_json::<Event>(&serde_json::to_vec(&unknown).unwrap()),
        Err(CodecError::Schema)
    );
}

#[test]
fn handwritten_rejection_reasons_are_stable() {
    for fixture in values(include_str!("fixtures/rejected.json")) {
        let expected = match fixture["reason"].as_str().unwrap() {
            "schema" => CodecError::Schema,
            "duplicate_key" => CodecError::DuplicateKey,
            "null" => CodecError::Null,
            "unsupported_version" => CodecError::UnsupportedVersion,
            _ => panic!("unknown fixture reason"),
        };
        assert_eq!(
            codec::from_json::<Request>(fixture["wire"].as_str().unwrap().as_bytes()),
            Err(expected),
            "{fixture}"
        );
    }
}

// 手書き正常値の各object/fieldを個別に壊し、未知field、null、必須欠落を検査する。
fn paths(value: &Value, path: String, objects: &mut Vec<String>, fields: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            objects.push(path.clone());
            for (key, value) in map {
                let next = format!("{path}/{key}");
                fields.push(next.clone());
                paths(value, next, objects, fields);
            }
        }
        Value::Array(array) => {
            for (index, value) in array.iter().enumerate() {
                paths(value, format!("{path}/{index}"), objects, fields);
            }
        }
        _ => {}
    }
}

fn rejects_mutations<T: DeserializeOwned>(wire: &Value) {
    let mut objects = Vec::new();
    let mut fields = Vec::new();
    paths(wire, String::new(), &mut objects, &mut fields);
    for path in objects {
        let mut positional = wire.clone();
        let object = positional.pointer_mut(&path).unwrap();
        *object = Value::Array(object.as_object().unwrap().values().cloned().collect());
        assert!(
            codec::from_json::<T>(&serde_json::to_vec(&positional).unwrap()).is_err(),
            "positional object accepted: {path}: {positional}"
        );
        let mut bad = wire.clone();
        bad.pointer_mut(&path)
            .unwrap()
            .as_object_mut()
            .unwrap()
            .insert("unexpected".into(), json!(true));
        assert!(
            codec::from_json::<T>(&serde_json::to_vec(&bad).unwrap()).is_err(),
            "unknown field accepted: {path}: {bad}"
        );
    }
    for path in fields {
        let mut bad = wire.clone();
        *bad.pointer_mut(&path).unwrap() = Value::Null;
        assert!(
            codec::from_json::<T>(&serde_json::to_vec(&bad).unwrap()).is_err(),
            "null accepted: {path}"
        );
        let optional = path.ends_with("/resume")
            || path.ends_with("/next_cursor")
            || path.ends_with("/run")
            || (wire["method"] == "draft.update" && path == "/params/attachment_ids");
        if !optional {
            let mut bad = wire.clone();
            let (parent, key) = path.rsplit_once('/').unwrap();
            bad.pointer_mut(parent)
                .unwrap()
                .as_object_mut()
                .unwrap()
                .remove(key);
            assert!(
                codec::from_json::<T>(&serde_json::to_vec(&bad).unwrap()).is_err(),
                "missing field accepted: {path}: {bad}"
            );
        }
    }
}

#[test]
fn correctly_ordered_positional_structs_are_rejected_at_nested_boundaries() {
    let mut request = requests()[7].clone();
    request["params"] = json!(["4", "3", "2"]);
    assert_eq!(
        codec::from_json::<Request>(&serde_json::to_vec(&request).unwrap()),
        Err(CodecError::Schema)
    );
    let mut request = requests()[3].clone();
    request["params"]["resume"] = json!(["e", "sub", "1"]);
    assert_eq!(
        codec::from_json::<Request>(&serde_json::to_vec(&request).unwrap()),
        Err(CodecError::Schema)
    );
    let mut event = values(include_str!("fixtures/events.json"))[0].clone();
    event["payload"] = json!(["message", "9", "日本語🙂", "tentative"]);
    assert_eq!(
        codec::from_json::<Event>(&serde_json::to_vec(&event).unwrap()),
        Err(CodecError::Schema)
    );
    let mut result = results()[5].clone();
    result["payload"] = json!(["5", "5"]);
    assert_eq!(
        codec::from_json::<SuccessResult>(&serde_json::to_vec(&result).unwrap()),
        Err(CodecError::Schema)
    );
    let mut response = values(include_str!("fixtures/responses.json"))[0].clone();
    response["result"] = json!(["run.cancel", {"status":"cancel_requested"}]);
    assert_eq!(
        codec::from_json::<Response>(&serde_json::to_vec(&response).unwrap()),
        Err(CodecError::Schema)
    );
    let mut snapshot: Value = serde_json::from_str(include_str!("fixtures/snapshot.json")).unwrap();
    snapshot["draft"] = json!(["4", "日本語", ["attachment"]]);
    assert_eq!(
        codec::from_json::<Snapshot>(&serde_json::to_vec(&snapshot).unwrap()),
        Err(CodecError::Schema)
    );
    // 正常fixture中のtasks/children/IDs/choices等は前の往復試験で実際に受理する。
}

#[test]
fn every_fixture_object_rejects_unknown_null_and_missing_required_fields() {
    for wire in requests() {
        rejects_mutations::<Request>(&wire);
    }
    for wire in results() {
        rejects_mutations::<SuccessResult>(&wire);
    }
    for wire in values(include_str!("fixtures/responses.json")) {
        rejects_mutations::<Response>(&wire);
    }
    for wire in values(include_str!("fixtures/events.json")) {
        rejects_mutations::<Event>(&wire);
    }
    for wire in values(include_str!("fixtures/status.json")) {
        rejects_mutations::<RequestStatusResult>(&wire);
    }
    rejects_mutations::<Snapshot>(
        &serde_json::from_str(include_str!("fixtures/snapshot.json")).unwrap(),
    );
}

#[test]
fn all_requests_enforce_session_scope_and_hello_gate() {
    for (index, wire) in requests().iter().enumerate() {
        let request: Request = read(wire);
        let before = ConnectionState::AwaitingHello;
        if index == 0 {
            assert_eq!(
                validate_request(before, &request).unwrap().next_state,
                ConnectionState::Ready
            );
            assert_eq!(
                validate_request(ConnectionState::Ready, &request),
                Err(GateError::AlreadyReady)
            );
        } else {
            assert_eq!(
                validate_request(before, &request),
                Err(GateError::HelloRequired)
            );
            assert_eq!(before, ConnectionState::AwaitingHello);
            assert_eq!(
                validate_request(ConnectionState::Ready, &request)
                    .unwrap()
                    .next_state,
                ConnectionState::Ready
            );
        }
        let mut wrong_scope = wire.clone();
        if index == 0 || index == 11 {
            wrong_scope["session_id"] = json!("s");
        } else {
            wrong_scope.as_object_mut().unwrap().remove("session_id");
        }
        assert!(codec::from_json::<Request>(&serde_json::to_vec(&wrong_scope).unwrap()).is_err());
        let mut version = wire.clone();
        version["protocol_version"] = json!(2);
        assert_eq!(
            codec::from_json::<Request>(&serde_json::to_vec(&version).unwrap()),
            Err(CodecError::UnsupportedVersion)
        );
    }
}

#[test]
fn duplicate_keys_are_rejected_before_value_conversion_at_any_depth() {
    for wire in [
        r#"{"x":1,"x":2}"#,
        r#"{"a":[{"x":1,"x":2}]}"#,
        r#"{"x":1,"\u0078":2}"#,
        r#"{"a":{"b":{"x":1,"x":2}}}"#,
    ] {
        assert_eq!(
            codec::from_json::<Value>(wire.as_bytes()),
            Err(CodecError::DuplicateKey)
        );
    }
    let request = r#"{"protocol_version":1,"kind":"request","client_id":"c","request_id":"r","method":"hello","params":{},"params":{}}"#;
    assert!(serde_json::from_str::<Request>(request).is_err());
    let event = include_str!("fixtures/events.json").replace(
        "\"byte_offset\":\"9\"",
        "\"byte_offset\":\"9\",\"byte_offset\":\"9\"",
    );
    assert_eq!(
        codec::from_json::<Vec<Event>>(event.as_bytes()),
        Err(CodecError::DuplicateKey)
    );
}

#[test]
fn optional_resume_is_atomic_and_page_limit_is_bounded() {
    let mut wire = requests()[3].clone();
    wire["params"] = json!({});
    assert_wire::<Request>(&wire);
    wire["params"] = json!({"resume":{"engine_epoch":"e","event_seq":"1"}});
    assert!(codec::from_json::<Request>(&serde_json::to_vec(&wire).unwrap()).is_err());
    for limit in [0, 257, 65536] {
        let mut wire = requests()[4].clone();
        wire["params"]["limit"] = json!(limit);
        assert!(codec::from_json::<Request>(&serde_json::to_vec(&wire).unwrap()).is_err());
    }
    for limit in [1, 256] {
        let mut wire = requests()[4].clone();
        wire["params"]["limit"] = json!(limit);
        assert_wire::<Request>(&wire);
    }
}
