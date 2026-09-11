//! Source application wire contracts, correlation and bounded pagination tests.
use polaris_desktop_protocol::{
    codec,
    ids::*,
    request::{PageLimit, Request, RequestBody},
    response::{ApprovalResolved, Capability, CorrelationError, Resolved, Response, SuccessResult},
    source_apply::*,
    validate::{ConnectionState, GateError, validate_request},
};
use serde_json::{Value, json};

fn request(method: &str, params: Value) -> Value {
    json!({"protocol_version":1,"kind":"request","client_id":"client","request_id":"request","session_id":"session","method":method,"params":params})
}
fn params(method: &str) -> Value {
    match method {
        "source_apply.list" => json!({"expected_session_revision":"7","offset":"0","limit":32}),
        "source_apply.page" => {
            json!({"approval_id":"approval","payload_hash":"a".repeat(64),"expected_session_revision":"7","offset":"0","limit":32})
        }
        _ => {
            json!({"run_id":"run","attempt_id":"attempt","approval_id":"approval","payload_hash":"a".repeat(64),"expected_session_revision":"7","expected_policy_revision":"3","decision":"allow"})
        }
    }
}
fn summary() -> SourceApplySummary {
    serde_json::from_value(json!({"run_id":"run","attempt_id":"attempt","approval_id":"approval","operation_id":"operation","policy_revision":"3","expires_at_unix_ms":"100","payload_hash":"a".repeat(64),"source_path":"/source","recovery_parent_path":"/private/recovery","entry_count":"1","invalidated":false,"intent_committed":true,"result_saved":true})).unwrap()
}
fn entry(path: &str) -> SourceApplyEntry {
    SourceApplyEntry {
        relative_path: path.into(),
        before: None,
        after: Some(SourceApplyVersion {
            hash: "a".repeat(64),
            mode: 0o644,
        }),
    }
}
fn page(
    entries: &[SourceApplyEntry],
    offset: u64,
    limit: u16,
) -> Result<SourceApplyPageResult, SourceApplyPageError> {
    SourceApplyPageResult::from_slice(
        ApprovalId::new("approval").unwrap(),
        "a".repeat(64),
        DecimalU64::new(7),
        entries,
        DecimalU64::new(offset),
        PageLimit::new(limit).unwrap(),
    )
}
fn results() -> Vec<SuccessResult> {
    vec![
        SuccessResult::SourceApplyList(
            SourceApplyListResult::from_slice(
                DecimalU64::new(7),
                &[summary()],
                DecimalU64::new(0),
                PageLimit::new(32).unwrap(),
            )
            .unwrap(),
        ),
        SuccessResult::SourceApplyPage(page(&[entry("file")], 0, 32).unwrap()),
        SuccessResult::SourceApplyResolve(ApprovalResolved {
            approval_id: ApprovalId::new("approval").unwrap(),
            state: Resolved::Resolved,
            decision: polaris_desktop_protocol::snapshot::ApprovalDecision::Allow,
        }),
    ]
}

#[test]
fn source_methods_envelopes_capabilities_and_correlation_round_trip() {
    for (method, result) in [
        "source_apply.list",
        "source_apply.page",
        "source_apply.resolve",
    ]
    .into_iter()
    .zip(results())
    {
        let wire = request(method, params(method));
        let req: Request = codec::from_json(&serde_json::to_vec(&wire).unwrap()).unwrap();
        assert_eq!(serde_json::to_value(&req).unwrap(), wire);
        assert_eq!(req.body.session_id().unwrap().as_str(), "session");
        assert_eq!(
            validate_request(ConnectionState::AwaitingHello, &req).unwrap_err(),
            GateError::HelloRequired
        );
        validate_request(ConnectionState::Ready, &req).unwrap();
        let response = Response::for_request(&req, Ok(result)).unwrap();
        let restored: Response = codec::from_json(&serde_json::to_vec(&response).unwrap()).unwrap();
        assert_eq!(restored, response);
        restored.validate_for(&req).unwrap();
        let mut wrong = req.clone();
        wrong.body = RequestBody::SessionSnapshot(SessionId::new("session").unwrap());
        assert_eq!(response.validate_for(&wrong), Err(CorrelationError::Method));
        wrong = req.clone();
        wrong.client_id = ClientId::new("other").unwrap();
        assert_eq!(
            response.validate_for(&wrong),
            Err(CorrelationError::ClientId)
        );
        wrong = req;
        wrong.request_id = RequestId::new("other").unwrap();
        assert_eq!(
            response.validate_for(&wrong),
            Err(CorrelationError::RequestId)
        );
    }
    for (c, spelling) in [
        (Capability::SourceApplyRead, "source_apply_read"),
        (Capability::SourceApplyResolve, "source_apply_resolve"),
    ] {
        assert_eq!(serde_json::to_value(c).unwrap(), json!(spelling));
        assert_eq!(
            serde_json::from_value::<Capability>(json!(spelling)).unwrap(),
            c
        );
    }
}

#[test]
fn source_requests_reject_extra_null_missing_session_and_wrong_params() {
    for method in [
        "source_apply.list",
        "source_apply.page",
        "source_apply.resolve",
    ] {
        let good = request(method, params(method));
        for field in params(method).as_object().unwrap().keys() {
            let mut bad = good.clone();
            bad["params"][field] = Value::Null;
            assert!(
                serde_json::from_value::<Request>(bad).is_err(),
                "{method}/{field}"
            );
        }
        for variant in 0..7 {
            let mut bad = good.clone();
            match variant {
                0 => {
                    bad.as_object_mut().unwrap().remove("session_id");
                }
                1 => bad["session_id"] = Value::Null,
                2 => bad["extra"] = json!(true),
                3 => bad["params"]["extra"] = json!(true),
                4 => bad["params"] = json!([]),
                5 => bad["method"] = json!("hello"),
                _ => {
                    bad["params"] = params(if method == "source_apply.resolve" {
                        "source_apply.list"
                    } else {
                        "source_apply.resolve"
                    })
                }
            }
            assert!(
                serde_json::from_value::<Request>(bad).is_err(),
                "{method}/{variant}"
            );
        }
        let bytes = serde_json::to_string(&good).unwrap().replacen(
            "\"params\":{",
            "\"params\":{\"expected_session_revision\":\"9\",",
            1,
        );
        assert!(codec::from_json::<Request>(bytes.as_bytes()).is_err());
    }
    // The shared request type still accepts 33; source service must reject it.
    let mut value = params("source_apply.list");
    value["limit"] = json!(33);
    let req: Request = serde_json::from_value(request("source_apply.list", value)).unwrap();
    let RequestBody::SourceApplyList(_, p) = req.body else {
        panic!()
    };
    assert_eq!(
        validate_source_apply_limit(p.limit),
        Err(SourceApplyPageError::Limit)
    );
}

#[test]
fn source_results_omit_optional_fields_and_reject_null_extra_and_wrong_ack() {
    for result in results() {
        let value = serde_json::to_value(&result).unwrap();
        assert!(value["payload"].get("next_offset").is_none());
        let mut bad = value.clone();
        bad["payload"]["extra"] = json!(true);
        assert!(serde_json::from_value::<SuccessResult>(bad).is_err());
        let mut bad = value.clone();
        bad["payload"] = json!([]);
        assert!(serde_json::from_value::<SuccessResult>(bad).is_err());
        let mut bad = value.clone();
        bad["payload"][if value["type"] == "source_apply.resolve" {
            "decision"
        } else {
            "session_revision"
        }] = Value::Null;
        assert!(serde_json::from_value::<SuccessResult>(bad).is_err());
        let mut bad = value.clone();
        bad["payload"][if value["type"] == "source_apply.resolve" {
            "approval_id"
        } else {
            "next_offset"
        }] = Value::Null;
        assert!(serde_json::from_value::<SuccessResult>(bad).is_err());
    }
    let value = serde_json::to_value(summary()).unwrap();
    assert!(value.get("decision").is_none());
    for field in ["decision", "raw_report", "applying"] {
        let mut bad = value.clone();
        bad[field] = Value::Null;
        assert!(serde_json::from_value::<SourceApplySummary>(bad).is_err());
    }
    let value = serde_json::to_value(entry("file")).unwrap();
    assert!(value.get("before").is_none());
    for field in ["before", "after"] {
        let mut bad = value.clone();
        bad[field] = Value::Null;
        assert!(serde_json::from_value::<SourceApplyEntry>(bad).is_err());
    }
    for value in [
        json!({"status":"not_found"}),
        json!({"status":"completed","session_revision":"8","result_id":"result"}),
    ] {
        assert!(
            serde_json::from_value::<SuccessResult>(
                json!({"type":"source_apply.resolve","payload":value})
            )
            .is_err()
        );
    }
}

#[test]
fn source_pages_count_actual_offsets_and_reject_bad_limits_and_offsets() {
    let all: Vec<_> = (0..35).map(|i| entry(&format!("file-{i}"))).collect();
    let first = page(&all, 0, 32).unwrap();
    assert_eq!(first.entries.len(), 32);
    assert_eq!(first.next_offset, Some(DecimalU64::new(32)));
    let last = page(&all, 32, 32).unwrap();
    assert_eq!(last.entries.len(), 3);
    assert!(last.next_offset.is_none());
    assert!(page(&all, 35, 32).unwrap().entries.is_empty());
    assert_eq!(page(&all, 36, 32), Err(SourceApplyPageError::Offset));
    assert_eq!(page(&all, u64::MAX, 32), Err(SourceApplyPageError::Offset));
    assert_eq!(page(&all, 0, 33), Err(SourceApplyPageError::Limit));
    let summaries = vec![summary(); 35];
    let list = SourceApplyListResult::from_slice(
        DecimalU64::new(7),
        &summaries,
        DecimalU64::new(32),
        PageLimit::new(32).unwrap(),
    )
    .unwrap();
    assert_eq!(list.items.len(), 3);
    assert!(list.next_offset.is_none());
}

#[test]
fn source_serialized_escape_budget_continuation_and_singleton_error() {
    let all = vec![entry(&"\u{0001}".repeat(30_000)); 3];
    let first = page(&all, 0, 32).unwrap();
    assert_eq!(first.entries.len(), 1);
    assert_eq!(first.next_offset, Some(DecimalU64::new(1)));
    assert!(
        serde_json::to_vec(&SuccessResult::SourceApplyPage(first))
            .unwrap()
            .len()
            <= MAX_SOURCE_APPLY_RESULT_BYTES
    );
    let last = page(&all, 2, 32).unwrap();
    assert!(last.next_offset.is_none());
    assert_eq!(
        page(&[entry(&"\u{0001}".repeat(50_000))], 0, 32),
        Err(SourceApplyPageError::TooLarge)
    );
    let mut large = summary();
    large.source_path = "\u{0001}".repeat(30_000);
    let list = SourceApplyListResult::from_slice(
        DecimalU64::new(7),
        &[large.clone(), large.clone()],
        DecimalU64::new(0),
        PageLimit::new(32).unwrap(),
    )
    .unwrap();
    assert_eq!(list.items.len(), 1);
    assert_eq!(list.next_offset, Some(DecimalU64::new(1)));
    assert!(
        serde_json::to_vec(&SuccessResult::SourceApplyList(list))
            .unwrap()
            .len()
            <= MAX_SOURCE_APPLY_RESULT_BYTES
    );
    large.source_path = "\u{0001}".repeat(50_000);
    assert_eq!(
        SourceApplyListResult::from_slice(
            DecimalU64::new(7),
            &[large],
            DecimalU64::new(0),
            PageLimit::new(32).unwrap()
        ),
        Err(SourceApplyPageError::TooLarge)
    );
}

#[test]
fn source_direct_results_cannot_bypass_count_or_byte_budgets() {
    let mut result = page(&[], 0, 32).unwrap();
    result.entries = vec![entry("file"); 33];
    assert!(serde_json::to_vec(&result).is_err());
    let value = json!({"approval_id":"approval","payload_hash":"a","session_revision":"7","entries":vec![serde_json::to_value(entry("file")).unwrap();33]});
    assert!(serde_json::from_value::<SourceApplyPageResult>(value).is_err());
    result.entries = vec![entry(&"x".repeat(MAX_SOURCE_APPLY_RESULT_BYTES))];
    assert!(serde_json::to_vec(&result).is_err());
    let value = json!({"approval_id":"approval","payload_hash":"a","session_revision":"7","entries":[{"relative_path":"x".repeat(MAX_SOURCE_APPLY_RESULT_BYTES)}]});
    assert!(serde_json::from_value::<SourceApplyPageResult>(value).is_err());
}

#[test]
fn source_apply_result_projection_is_optional_bounded_and_typed() {
    let mut value = serde_json::to_value(summary()).unwrap();
    assert!(value.get("result").is_none());
    value["result"] = json!({
        "result_id":"saved-result",
        "status":"partial",
        "failure_kind":"conflict",
        "installed_count":"1",
        "deleted_count":"2",
        "restored_count":"3"
    });
    let parsed: SourceApplySummary = serde_json::from_value(value.clone()).unwrap();
    assert_eq!(
        parsed.result.as_ref().unwrap().status,
        SourceApplyResultStatus::Partial
    );
    assert_eq!(
        parsed.result.as_ref().unwrap().failure_kind,
        Some(SourceApplyFailureKind::Conflict)
    );
    value["result"]["failure_kind"] = json!("unbounded_report_text");
    assert!(serde_json::from_value::<SourceApplySummary>(value).is_err());
}
