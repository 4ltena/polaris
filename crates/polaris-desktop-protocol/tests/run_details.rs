//! run.details pages: bounded fragments and run correlation.
use polaris_desktop_protocol::{
    codec,
    request::Request,
    response::{Response, SuccessResult},
};
use serde_json::{Value, json};

fn decode<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, codec::CodecError> {
    codec::from_json(&serde_json::to_vec(value).unwrap())
}

fn page(fragments: Value) -> Value {
    json!({"protocol_version":1,"kind":"response","client_id":"client","request_id":"request",
        "result":{"type":"run.details","payload":{
            "snapshot_id":"details-1","run_id":"run","attempt_id":"attempt",
            "session_revision":"5","accepted_revision":"3","run_state":"succeeded",
            "history_gap":false,"fragments":fragments,"next_cursor":"entry-2:0"}}})
}

fn fragment(text: &str, offset: u64, total: u64) -> Value {
    json!({"entry_id":"12-0","kind":"tool_call","offset":offset.to_string(),"total_bytes":total.to_string(),"text":text})
}

#[test]
fn pages_round_trip_and_reject_oversize_or_inconsistent_fragments() {
    let wire = page(json!([fragment("{\"id\":\"call\"}", 0, 13)]));
    let response: Response = decode(&wire).unwrap();
    assert!(matches!(response.outcome, Ok(SuccessResult::RunDetails(_))));
    assert_eq!(serde_json::to_value(&response).unwrap(), wire);
    for bad in [
        Value::Array(vec![fragment("a", 0, 1); 5]),
        json!([fragment(&"a".repeat(32 * 1024 + 1), 0, 40_000)]),
        json!([fragment("abc", 2, 4)]),
        json!([fragment("a", u64::MAX, 1)]),
        json!([
            fragment(&"a".repeat(32 * 1024), 0, 80_000),
            fragment(&"a".repeat(32 * 1024), 32 * 1024, 80_000),
            fragment("a", 64 * 1024, 80_000)
        ]),
    ] {
        assert!(decode::<Response>(&page(bad)).is_err());
    }
    let mut unknown = wire;
    unknown["result"]["payload"]["fragments"][0]["kind"] = json!("reasoning");
    assert!(decode::<Response>(&unknown).is_err());
}

#[test]
fn requests_carry_run_identity_and_responses_must_match_it() {
    let request: Request = decode(
        &json!({"protocol_version":1,"kind":"request","client_id":"client",
        "request_id":"request","method":"run.details","session_id":"session",
        "params":{"run_id":"run","attempt_id":"attempt"}}),
    )
    .unwrap();
    let response: Response = decode(&page(json!([]))).unwrap();
    assert!(response.validate_for(&request).is_ok());
    let other: Request = decode(&json!({"protocol_version":1,"kind":"request","client_id":"client",
        "request_id":"request","method":"run.details","session_id":"session",
        "params":{"run_id":"run","attempt_id":"other","snapshot_id":"details-1","cursor":"entry-2:0"}}))
    .unwrap();
    assert!(response.validate_for(&other).is_err());
    for params in [
        json!({"run_id":"run","attempt_id":"attempt","cursor":null}),
        json!({"run_id":"run","attempt_id":"attempt","cursor":"entry-2:0"}),
        json!({"run_id":"run","attempt_id":"attempt","snapshot_id":"details-1"}),
    ] {
        let wire = json!({"protocol_version":1,"kind":"request","client_id":"client",
            "request_id":"request","method":"run.details","session_id":"session","params":params});
        assert!(decode::<Request>(&wire).is_err(), "{wire}");
    }
}
