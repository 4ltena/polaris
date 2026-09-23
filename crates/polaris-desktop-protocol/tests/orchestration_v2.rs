//! Wire compatibility tests for desktop orchestration snapshots and requests.

use polaris_desktop_protocol::event::Event;
use polaris_desktop_protocol::{
    ProtocolVersion, codec,
    ids::{ClientId, RequestId},
    request::{Request, RequestBody},
    response::{CorrelationError, Response, SuccessResult},
    snapshot::Snapshot,
};
use serde_json::json;

fn orchestration() -> serde_json::Value {
    json!({
      "workflow": {
        "phase": "implement", "step": "executing",
        "active_task": {"artifact_id": "task-1", "content_hash": "a".repeat(64)},
        "acceptance": [{"criterion_id": "criterion-1", "state": "passed", "evidence_count": "1"}],
        "evidence": {"passed":"1", "failed":"0", "stale":"0", "unavailable":"0"},
        "convergence": {"state":"pending", "required_criteria":"1", "unresolved_gaps":"0"}
      },
      "session_manager": {
        "manager_revision":"2", "session_count":"2", "session_total":"2", "session_omitted":false,
        "sessions":[
          {"session_id":"session-main","kind":"current","state":"running"},
          {"session_id":"session-side","kind":"sidechat","state":"idle","source_session_id":"session-main"}
        ], "sidechat":"active",
        "queued_admissions":"1", "active_runs":"1"
      },
      "subagents": [{
        "root_run_id":"run-1", "root_attempt_id":"root-attempt-1", "control_revision":"3",
        "logical_child_id":"child-1", "attempt_id":"attempt-1", "display_name":"Sirius",
        "role":"file-reader", "state":"running", "activity":"read(CURRENT.md)"
      }],
      "subagent_total":"1", "subagent_omitted":false,
      "mcp_leases": [{
        "lease_id":"lease-1", "server_id":"mcp-1", "holder_run_id":"run-1",
        "holder_attempt_id":"attempt-1", "state":"active", "expires_at_unix_ms":"1"
      }],
      "mcp_lease_total":"1", "mcp_lease_omitted":false,
      "common_runner": {
        "state":"running", "case_id":"case-1", "completed_cases":"1", "failed_cases":"0", "cleanup_pending":false
      },
      "clarification": {
        "state":"pending", "unanswered_count":"1",
        "questions":[{
          "question_id":"question-1", "text":"Which interface?", "options":[],
          "free_text":{"allowed":true,"required":true,"max_bytes":"32768"}, "answered":false
        }]
      }
    })
}

#[test]
fn v1_decode_is_preserved_and_v2_is_default_for_new_requests() {
    let v1 = json!({
      "protocol_version":1,"kind":"request","client_id":"client","request_id":"request",
      "method":"hello","params":{}
    });
    let decoded: Request = codec::from_json(v1.to_string().as_bytes()).unwrap();
    assert_eq!(decoded.protocol_version.get(), 1);
    assert_eq!(serde_json::to_value(decoded).unwrap(), v1);
    assert_eq!(ProtocolVersion.get(), 2);
}

#[test]
fn v2_orchestration_snapshot_round_trips_and_rejects_unknown_or_unbounded_state() {
    let mut snapshot: serde_json::Value =
        serde_json::from_str(include_str!("fixtures/snapshot.json")).unwrap();
    snapshot["orchestration"] = orchestration();
    let typed: Snapshot = codec::from_json(snapshot.to_string().as_bytes()).unwrap();
    assert_eq!(serde_json::to_value(&typed).unwrap(), snapshot);

    let mut unknown = snapshot.clone();
    unknown["orchestration"]["mcp_leases"][0]["secret"] = json!("must-not-cross-wire");
    assert!(codec::from_json::<Snapshot>(unknown.to_string().as_bytes()).is_err());

    let mut too_many = snapshot;
    too_many["orchestration"]["session_manager"]["session_count"] = json!("3");
    assert!(codec::from_json::<Snapshot>(too_many.to_string().as_bytes()).is_err());
}

#[test]
fn v2_admission_operations_are_closed_and_bound_text() {
    let submit = json!({
      "protocol_version":2,"kind":"request","client_id":"client","request_id":"request",
      "method":"input.submit","session_id":"session",
      "params":{"expected_manager_revision":"0","expected_session_revision":"3","expected_goal_revision":"4",
      "route":"auto","kind":"text","attachment_ids":["attachment-1"],"admission_id":"adm-1","text":"continue"}
    });
    let decoded: Request = codec::from_json(submit.to_string().as_bytes()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), submit);
    for (field, value) in [("extra", json!(true)), ("text", json!("x".repeat(32_769)))] {
        let mut invalid = submit.clone();
        invalid["params"][field] = value;
        assert!(codec::from_json::<Request>(invalid.to_string().as_bytes()).is_err());
    }
    let mut too_many_attachments = submit.clone();
    too_many_attachments["params"]["attachment_ids"] = json!(
        (0..17)
            .map(|id| format!("attachment-{id}"))
            .collect::<Vec<_>>()
    );
    assert!(codec::from_json::<Request>(too_many_attachments.to_string().as_bytes()).is_err());
    let listed = json!({
      "protocol_version":2,"kind":"request","client_id":"client","request_id":"request",
      "method":"session.list","params":{}
    });
    let decoded: Request = codec::from_json(listed.to_string().as_bytes()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), listed);

    let selected = json!({
      "protocol_version":2,"kind":"request","client_id":"client","request_id":"request",
      "method":"session.select","session_id":"session","params":{}
    });
    let decoded: Request = codec::from_json(selected.to_string().as_bytes()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), selected);

    let cancelled = json!({
      "protocol_version":2,"kind":"request","client_id":"client","request_id":"request",
      "method":"input.cancel","session_id":"session",
      "params":{"expected_manager_revision":"7","admission_id":"adm-1"}
    });
    let decoded: Request = codec::from_json(cancelled.to_string().as_bytes()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), cancelled);
}

#[test]
fn v2_methods_fail_closed_on_v1_frames() {
    for method in [
        "session.list",
        "session.select",
        "input.submit",
        "input.status",
        "input.cancel",
        "subagent.wait",
        "subagent.cancel",
    ] {
        let params = match method {
            "session.list" | "session.select" => json!({}),
            "input.submit" => json!({
                "expected_manager_revision":"0","expected_session_revision":"0",
                "expected_goal_revision":"0","route":"current","kind":"text","text":"x"
            }),
            "input.status" => json!({"admission_id":"adm-1"}),
            "input.cancel" => json!({"expected_manager_revision":"0","admission_id":"adm-1"}),
            "subagent.wait" => json!({
                "root_run_id":"run-1","root_attempt_id":"root-attempt-1",
                "expected_revision":"0","after_cursor":"0"
            }),
            "subagent.cancel" => json!({
                "root_run_id":"run-1","root_attempt_id":"root-attempt-1",
                "logical_child_id":"child-1","child_attempt_id":"child-attempt-1",
                "expected_revision":"0"
            }),
            _ => unreachable!(),
        };
        let mut wire = json!({
            "protocol_version":1,"kind":"request","client_id":"client","request_id":"request",
            "method":method,"params":params
        });
        if method != "session.list" {
            wire["session_id"] = json!("session");
        }
        assert!(codec::from_json::<Request>(wire.to_string().as_bytes()).is_err());
    }
}

#[test]
fn v2_subagent_control_is_exact_closed_and_correlated() {
    let wait = json!({
      "protocol_version":2,"kind":"request","client_id":"client","request_id":"wait",
      "method":"subagent.wait","session_id":"session-main",
      "params":{"root_run_id":"run-1","root_attempt_id":"root-attempt-1",
      "expected_revision":"4","after_cursor":"8"}
    });
    let wait_request: Request = codec::from_json(wait.to_string().as_bytes()).unwrap();
    assert_eq!(serde_json::to_value(&wait_request).unwrap(), wait);
    let wait_result = json!({
      "type":"subagent.wait","payload":{
        "root_run_id":"run-1","root_attempt_id":"root-attempt-1",
        "revision":"4","cursor":"8","state":"unchanged",
        "child_total":"0","child_omitted":false,"children":[]
      }
    });
    let wait_result: SuccessResult = codec::from_json(wait_result.to_string().as_bytes()).unwrap();
    Response::for_request(&wait_request, Ok(wait_result)).unwrap();

    let mixed_child = json!({
      "type":"subagent.wait","payload":{
        "root_run_id":"run-1","root_attempt_id":"root-attempt-1",
        "revision":"4","cursor":"9","state":"updated",
        "child_total":"1","child_omitted":false,"children":[{
          "root_run_id":"foreign-run","root_attempt_id":"root-attempt-1",
          "control_revision":"4","logical_child_id":"child-1",
          "attempt_id":"child-attempt-1","display_name":"Sirius",
          "role":"file-reader","state":"running"
        }]
      }
    });
    let mixed_child: SuccessResult = codec::from_json(mixed_child.to_string().as_bytes()).unwrap();
    assert_eq!(
        Response::for_request(&wait_request, Ok(mixed_child)),
        Err(CorrelationError::Method)
    );

    let cancel = json!({
      "protocol_version":2,"kind":"request","client_id":"client","request_id":"cancel",
      "method":"subagent.cancel","session_id":"session-main",
      "params":{"root_run_id":"run-1","root_attempt_id":"root-attempt-1",
      "logical_child_id":"child-1","child_attempt_id":"child-attempt-1",
      "expected_revision":"4"}
    });
    let cancel_request: Request = codec::from_json(cancel.to_string().as_bytes()).unwrap();
    assert_eq!(serde_json::to_value(&cancel_request).unwrap(), cancel);
    let cancel_result = json!({
      "type":"subagent.cancel","payload":{
        "root_run_id":"run-1","root_attempt_id":"root-attempt-1",
        "logical_child_id":"child-1","child_attempt_id":"child-attempt-1",
        "revision":"5","cursor":"9","disposition":"requested","state":"cancelling",
        "runtime_effect":"unavailable"
      }
    });
    let cancel_result: SuccessResult =
        codec::from_json(cancel_result.to_string().as_bytes()).unwrap();
    Response::for_request(&cancel_request, Ok(cancel_result.clone())).unwrap();

    let mut foreign = cancel_request.clone();
    foreign.body = match foreign.body {
        RequestBody::SubagentCancel(session, mut params) => {
            params.child_attempt_id =
                polaris_desktop_protocol::ids::ChildAttemptId::new("other-attempt").unwrap();
            RequestBody::SubagentCancel(session, params)
        }
        _ => unreachable!(),
    };
    assert_eq!(
        Response::for_request(&foreign, Ok(cancel_result)),
        Err(CorrelationError::Method)
    );

    let mut unknown = cancel;
    unknown["params"]["runtime_handle"] = json!("must-not-cross-wire");
    assert!(codec::from_json::<Request>(unknown.to_string().as_bytes()).is_err());
}

#[test]
fn responses_and_events_fail_closed_across_protocol_versions() {
    let request = Request {
        protocol_version: ProtocolVersion::V1,
        client_id: ClientId::new("client").unwrap(),
        request_id: RequestId::new("request").unwrap(),
        body: RequestBody::Hello,
    };
    let response = Response {
        protocol_version: ProtocolVersion::V2,
        client_id: request.client_id.clone(),
        request_id: request.request_id.clone(),
        outcome: Err(polaris_desktop_protocol::response::ProtocolError {
            code: polaris_desktop_protocol::response::ErrorCode::InvalidRequest,
            message: "mismatch".into(),
        }),
    };
    assert_eq!(
        response.validate_for(&request),
        Err(CorrelationError::ProtocolVersion)
    );

    let event = json!({
      "protocol_version":1,"kind":"event","engine_epoch":"epoch","subscription_id":"subscription",
      "event_seq":"1","session_id":"session","session_revision":"2",
      "type":"orchestration.updated","payload":orchestration()
    });
    assert!(codec::from_json::<Event>(event.to_string().as_bytes()).is_err());
}

#[test]
fn v2_admission_status_preserves_route_destination_and_run_mapping() {
    let status = json!({
      "type":"input.status","payload":{
        "manager_revision":"7", "admission_id":"adm-1", "source_session_id":"session-main",
        "session_revision":"9", "route":"auto", "route_reason":"distinct_context",
        "destination_session_id":"session-side", "state":"running",
        "run":{"run_id":"run-1", "attempt_id":"attempt-1"}
      }
    });
    let typed: SuccessResult = codec::from_json(status.to_string().as_bytes()).unwrap();
    assert_eq!(serde_json::to_value(typed).unwrap(), status);
    let mut unknown = status;
    unknown["payload"]["hidden_text"] = json!("must not cross the durable mapping boundary");
    assert!(codec::from_json::<SuccessResult>(unknown.to_string().as_bytes()).is_err());
}

#[test]
fn v2_orchestration_event_is_a_full_replacement_and_closed() {
    let event = json!({
      "protocol_version":2,"kind":"event","engine_epoch":"epoch","subscription_id":"subscription",
      "event_seq":"1","session_id":"session","session_revision":"2",
      "type":"orchestration.updated","payload":orchestration()
    });
    let decoded: Event = codec::from_json(event.to_string().as_bytes()).unwrap();
    assert_eq!(serde_json::to_value(decoded).unwrap(), event);
    let mut unknown = event;
    unknown["payload"]["common_runner"]["command"] = json!("must-not-cross-wire");
    assert!(codec::from_json::<Event>(unknown.to_string().as_bytes()).is_err());
}
