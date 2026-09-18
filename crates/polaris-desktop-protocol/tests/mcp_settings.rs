//! Host MCP settings: registry objects pass through, secret values stay write-only.

use polaris_desktop_protocol::{
    codec,
    mcp_settings::McpRegistryState,
    request::{Request, RequestBody},
    response::{Capability, Response, SuccessResult},
};
use serde_json::{Value, json};

fn request(method: &str, params: Value) -> Value {
    json!({"protocol_version":1,"kind":"request","client_id":"client","request_id":"request",
        "method":method,"session_id":"session","params":params})
}

fn decode<T: serde::de::DeserializeOwned>(value: &Value) -> Result<T, codec::CodecError> {
    codec::from_json(&serde_json::to_vec(value).unwrap())
}

fn server() -> Value {
    json!({"server_id":"fixture","enabled_projects":["project"],
        "transport":{"kind":"streamable_http","url":"https://example.com/mcp","secret_headers":{"authorization":"credential"}},
        "capabilities":{"catalog_metadata":true,"resource_read":false,"prompt_selection":false,"tool_call":false}})
}

#[test]
fn settings_requests_roundtrip_and_require_the_session_envelope() {
    for (method, params) in [
        ("mcp.registry.read", json!({})),
        (
            "mcp.registry.save",
            json!({"expected_revision":"3","servers":[server()]}),
        ),
        (
            "mcp.secret.set",
            json!({"expected_revision":"0","reference":"credential","value":"synthetic-secret"}),
        ),
        (
            "mcp.secret.delete",
            json!({"expected_revision":"1","reference":"credential"}),
        ),
    ] {
        let wire = request(method, params);
        let typed: Request = decode(&wire).unwrap();
        assert_eq!(serde_json::to_value(&typed).unwrap(), wire);
        assert!(typed.body.session_id().is_some());
        let mut unscoped = wire.clone();
        unscoped.as_object_mut().unwrap().remove("session_id");
        assert!(decode::<Request>(&unscoped).is_err(), "{method}");
        let mut extra = wire;
        extra["params"]["unexpected"] = json!(true);
        assert!(decode::<Request>(&extra).is_err(), "{method}");
    }
}

#[test]
fn catalog_check_and_cancel_are_one_server_and_closed_outcomes() {
    for (method, result) in [
        (
            "mcp.catalog.check",
            json!({"server_id":"fixture","outcome":"partial","tools":"512"}),
        ),
        (
            "mcp.catalog.check",
            json!({"server_id":"fixture","outcome":"failed","tools":"0","failure":"auth"}),
        ),
        (
            "mcp.catalog.cancel",
            json!({"server_id":"fixture","state":"cancelling"}),
        ),
    ] {
        let wire = request(method, json!({"server_id":"fixture"}));
        let typed: Request = decode(&wire).unwrap();
        assert_eq!(serde_json::to_value(&typed).unwrap(), wire);
        let mut bad = wire.clone();
        bad["params"]["server_id"] = json!("bad id");
        assert!(decode::<Request>(&bad).is_err());
        let response = json!({"protocol_version":1,"kind":"response","client_id":"client","request_id":"request",
            "result":{"type":method,"payload":result}});
        let decoded: Response = decode(&response).unwrap();
        decoded.validate_for(&typed).unwrap();
        assert_eq!(serde_json::to_value(&decoded).unwrap(), response);
    }
    let failed_without_reason = json!({"protocol_version":1,"kind":"response","client_id":"client","request_id":"request",
        "result":{"type":"mcp.catalog.check","payload":{"server_id":"fixture","outcome":"failed","tools":"0"}}});
    assert!(decode::<Response>(&failed_without_reason).is_err());
    for payload in [
        json!({"server_id":"fixture","outcome":"checked","tools":"3","failure":"auth"}),
        json!({"server_id":"fixture","outcome":"cancelled","tools":"3"}),
        json!({"server_id":"fixture","outcome":"checked","tools":"513"}),
    ] {
        let invalid = json!({"protocol_version":1,"kind":"response","client_id":"client","request_id":"request",
            "result":{"type":"mcp.catalog.check","payload":payload}});
        assert!(decode::<Response>(&invalid).is_err());
    }
    let check: Request = decode(&request(
        "mcp.catalog.check",
        json!({"server_id":"fixture"}),
    ))
    .unwrap();
    let other: Response = decode(&json!({"protocol_version":1,"kind":"response","client_id":"client","request_id":"request",
        "result":{"type":"mcp.catalog.check","payload":{"server_id":"other","outcome":"checked","tools":"1"}}})).unwrap();
    assert!(other.validate_for(&check).is_err());
}

#[test]
fn secret_values_are_bounded_and_never_debug_printed() {
    let wire = request(
        "mcp.secret.set",
        json!({"expected_revision":"0","reference":"credential","value":"synthetic-secret"}),
    );
    let typed: Request = decode(&wire).unwrap();
    assert!(matches!(typed.body, RequestBody::McpSecretSet(..)));
    assert!(!format!("{typed:?}").contains("synthetic-secret"));
    for bad in ["", "a\nb", "a\rb", "a\u{0}b"] {
        let mut wire = wire.clone();
        wire["params"]["value"] = json!(bad);
        assert!(decode::<Request>(&wire).is_err(), "{bad:?}");
    }
    let mut large = wire.clone();
    large["params"]["value"] = json!("s".repeat(64 * 1024 + 1));
    assert!(decode::<Request>(&large).is_err());
    let mut reference = wire;
    reference["params"]["reference"] = json!("bad id");
    assert!(decode::<Request>(&reference).is_err());
}

#[test]
fn registry_state_has_names_and_revisions_only() {
    let state = json!({"registry_revision":"3","servers":[server()],"secrets_revision":"2",
        "secret_references":["credential"],"active_registry_revision":"2","active_secrets_revision":"2",
        "unchecked_servers":["fixture"]});
    for method in [
        "mcp.registry.read",
        "mcp.registry.save",
        "mcp.secret.set",
        "mcp.secret.delete",
    ] {
        let wire = json!({"protocol_version":1,"kind":"response","client_id":"client","request_id":"request",
            "result":{"type":method,"payload":state}});
        let response: Response = decode(&wire).unwrap();
        assert!(matches!(
            response.outcome,
            Ok(SuccessResult::McpRegistryRead(_)
                | SuccessResult::McpRegistrySave(_)
                | SuccessResult::McpSecretSet(_)
                | SuccessResult::McpSecretDelete(_))
        ));
        assert_eq!(serde_json::to_value(&response).unwrap(), wire);
        let mut leaked = wire.clone();
        leaked["result"]["payload"]["secret_values"] = json!({"credential":"synthetic"});
        assert!(decode::<Response>(&leaked).is_err());
    }
    let mut unchecked = state;
    unchecked["unchecked_servers"] = json!(vec!["fixture"; 33]);
    assert!(serde_json::from_value::<McpRegistryState>(unchecked).is_err());
    let too_many = json!({"expected_revision":"0","servers":vec![server(); 33]});
    assert!(decode::<Request>(&request("mcp.registry.save", too_many)).is_err());
    assert_eq!(
        serde_json::to_value(Capability::McpSettings).unwrap(),
        json!("mcp_settings")
    );
}

#[test]
fn setup_candidates_carry_either_approval_text_or_a_reason() {
    let candidates = json!({"candidates":[
        {"candidate_id":"0123abcd","name":"docs","transport":"streamable_http",
            "status":{"kind":"importable","description":"名前: docs"}},
        {"candidate_id":"4567ef01","name":"context7","transport":"stdio",
            "status":{"kind":"unavailable","reason":"launcher_not_supported"}}]});
    let wire = json!({"protocol_version":1,"kind":"response","client_id":"client","request_id":"request",
        "result":{"type":"mcp.setup.detect","payload":candidates}});
    let response: Response = decode(&wire).unwrap();
    assert_eq!(serde_json::to_value(&response).unwrap(), wire);
    for status in [
        json!({"kind":"importable","description":"x","reason":"invalid"}),
        json!({"kind":"unavailable"}),
        json!({"kind":"importable","description":""}),
    ] {
        let mut bad = wire.clone();
        bad["result"]["payload"]["candidates"][0]["status"] = status;
        assert!(decode::<Response>(&bad).is_err());
    }
    let mut leaked = wire;
    leaked["result"]["payload"]["candidates"][0]["url"] = json!("https://docs.example.com/mcp");
    assert!(decode::<Response>(&leaked).is_err());
    assert!(decode::<Request>(&request("mcp.setup.detect", json!({}))).is_ok());
    assert!(decode::<Request>(&request("mcp.setup.detect", json!({"path":"/tmp"}))).is_err());
    assert!(
        decode::<Request>(&request(
            "mcp.setup.register",
            json!({"candidate_id":"0123abcd"})
        ))
        .is_ok()
    );
    assert!(
        decode::<Request>(&request(
            "mcp.setup.register",
            json!({"candidate_id":"../x"})
        ))
        .is_err()
    );
}
