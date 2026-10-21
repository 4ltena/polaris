//! Synthetic loopback protocol tests; no real service or credential.
use super::*;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{header, method, path},
};
fn limits() -> SystemOneLimits {
    SystemOneLimits {
        request_bytes: 4096,
        response_bytes: 4096,
        questions: 4,
        options: 8,
        timeout_ms: 1000,
    }
}
fn target(endpoint: &str, model: &str) -> SystemOneTarget {
    SystemOneTarget::new(endpoint, model, true).unwrap()
}
fn client(endpoint: &str) -> SystemOneDecider {
    let target = target(endpoint, "synthetic");
    let key = BoundCredential::new(target.binding(), "dummy-key".into()).unwrap();
    SystemOneDecider::new(target, key, limits()).unwrap()
}
fn questions() -> BTreeMap<String, Question> {
    BTreeMap::from([(
        "q".into(),
        Question::Noul {
            instructions: "synthetic test".into(),
        },
    )])
}
fn reply() -> serde_json::Value {
    serde_json::json!({"model":"synthetic", "answers":{"q":{"type":"noul","noul":0.9}},"usage":{"input_tokens":7,"output_tokens":0}})
}
#[test]
fn target_normalizes_origin_and_rejects_unapproved_http_and_url_secrets() {
    let first = target("https://EXAMPLE.com:443/v1/systemone", "synthetic");
    let second = target("https://example.com/other", "synthetic");
    assert!(first.binding() == second.binding());
    for endpoint in [
        "http://example.com/api",
        "http://localhost/api",
        "https://user:secret@example.com/api",
        "https://@example.com/api",
        "https://example.com/api?token=dummy",
        "https://example.com/api#secret",
        "file:///tmp/endpoint",
        "https://example.com/\\api",
        " https://example.com/api",
    ] {
        assert!(
            SystemOneTarget::new(endpoint, "synthetic", true).is_err(),
            "accepted {endpoint}"
        );
    }
    assert!(SystemOneTarget::new("http://127.0.0.1/api", "synthetic", false).is_err());
    assert!(SystemOneTarget::new("http://[::1]:777/api", "synthetic", true).is_ok());
    for key in ["", "dummy\ncredential", "dummy key"] {
        assert!(BoundCredential::new(first.binding(), key.into()).is_err());
    }
}
#[tokio::test]
async fn credential_association_mismatch_is_refused_before_request() {
    let server = MockServer::start().await;
    let approved = target(&format!("{}/approved", server.uri()), "synthetic");
    for requested in [
        target(&format!("{}/requested", server.uri()), "other-model"),
        target("http://127.0.0.1:9/api", "synthetic"),
        target("https://127.0.0.1/api", "synthetic"),
    ] {
        let key = BoundCredential::new(approved.binding(), "dummy-key".into()).unwrap();
        assert!(matches!(
            SystemOneDecider::new(requested, key, limits()),
            Err(DecisionError::Auth)
        ));
    }
    assert!(server.received_requests().await.unwrap().is_empty());
}
#[tokio::test]
async fn one_bounded_request_binds_endpoint_model_credential_and_observation() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/api/systemone"))
        .and(header("authorization", "Bearer dummy-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(reply()))
        .expect(1)
        .mount(&server)
        .await;
    let endpoint = format!("{}/api/systemone", server.uri());
    let meter = super::super::DecisionMeter::default();
    let decider = meter.wrap(std::sync::Arc::new(client(&endpoint)));
    let observed = decider
        .observe(&serde_json::json!({"task":"synthetic"}), &questions())
        .await;
    let decision = observed.result.unwrap();
    assert_eq!(decision.answers["q"].noul, Some(0.9));
    assert_eq!(decision.output_tokens, Some(0));
    assert!(observed.attempts[0].invoked);
    assert_eq!(
        observed.attempts[0].endpoint_origin.as_deref(),
        Some(server.uri().as_str())
    );
    let trace = serde_json::to_string(meter.recent().last().unwrap()).unwrap();
    assert!(trace.contains(&server.uri()));
    assert!(!trace.contains("/api/systemone"));
    assert_eq!(observed.identity.backend, "system-one-compatible");
    assert_eq!(
        observed.capabilities.band_calibration,
        super::super::BandCalibration::Unmeasured
    );
    assert_eq!(
        observed.capabilities.network,
        crate::NetworkBoundary::RemoteAllowed
    );
    assert_eq!(observed.capabilities.max_context_tokens, None);
    let requests = server.received_requests().await.unwrap();
    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["model"], "synthetic");
    assert_eq!(body["state"]["task"], "synthetic");
}
#[tokio::test]
async fn redirects_and_error_statuses_never_retry_or_forward_authorization() {
    for status in [301, 302, 307, 308, 401, 403, 429, 500, 503] {
        let origin = MockServer::start().await;
        let redirect = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(status)
                    .insert_header("Location", format!("{}/other", redirect.uri()))
                    .set_body_string("dummy secret body"),
            )
            .expect(1)
            .mount(&origin)
            .await;
        let error = client(&format!("{}/api", origin.uri()))
            .decide(&serde_json::Value::Null, &questions())
            .await
            .unwrap_err();
        assert_eq!(
            error,
            if status == 401 || status == 403 {
                DecisionError::Auth
            } else {
                DecisionError::Unavailable
            }
        );
        assert!(redirect.received_requests().await.unwrap().is_empty());
    }
}
#[tokio::test]
async fn malformed_model_duplicate_keys_and_inconsistent_answers_are_rejected() {
    for body in [
        r#"{"model":"other","answers":{"q":{"type":"noul","noul":0.9}}}"#,
        r#"{"model":"synthetic","answers":{"q":{"type":"noul","noul":0.9},"q":{"type":"noul","noul":0.2}}}"#,
        r#"{"model":"synthetic","answers":{"q":{"type":"noul","noul":0.9,"noul":0.2}}}"#,
        r#"{"model":"synthetic","answers":{"q":{"type":"choice","noul":0.9}}}"#,
        r#"{"model":"synthetic","answers":{}}"#,
        r#"{"model":"synthetic","answers":{"q":{"type":"noul","noul":1.1}}}"#,
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            client(&server.uri())
                .decide(&serde_json::Value::Null, &questions())
                .await,
            Err(DecisionError::InvalidAnswer)
        );
    }
}
#[tokio::test]
async fn request_and_response_limits_are_enforced_without_truncation() {
    let server = MockServer::start().await;
    let target = target(&server.uri(), "synthetic");
    let mut small = limits();
    small.request_bytes = 64;
    let key = BoundCredential::new(target.binding(), "dummy-key".into()).unwrap();
    let decider = SystemOneDecider::new(target, key, small).unwrap();
    let observed = decider
        .observe(&serde_json::json!("x".repeat(300)), &questions())
        .await;
    assert_eq!(observed.result, Err(DecisionError::TooLarge));
    assert!(!observed.attempts[0].invoked);
    assert!(server.received_requests().await.unwrap().is_empty());
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("x".repeat(5000)))
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        client(&server.uri())
            .decide(&serde_json::Value::Null, &questions())
            .await,
        Err(DecisionError::InvalidAnswer)
    );
}
#[tokio::test]
async fn optional_usage_and_strict_choice_score_contracts_are_preserved() {
    let server = MockServer::start().await;
    let questions = BTreeMap::from([
        (
            "choice".into(),
            Question::Choice {
                instructions: "select".into(),
                criteria: BTreeMap::from([("a".into(), "A".into()), ("b".into(), "B".into())]),
            },
        ),
        (
            "score".into(),
            Question::Score {
                instructions: "rate".into(),
                criteria: vec!["low".into(), "mid".into(), "high".into()],
            },
        ),
    ]);
    Mock::given(method("POST")).respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({"model":"synthetic","answers":{
      "choice":{"type":"choice","choice":"b","probabilities":{"a":0.2,"b":0.8}},
      "score":{"type":"score","score":1.25,"probabilities":{"0":0.25,"1":0.25,"2":0.5},"legend":{"0":"low","1":"mid","2":"high"}} }}))).mount(&server).await;
    let decision = client(&server.uri())
        .decide(&serde_json::Value::Null, &questions)
        .await
        .unwrap();
    assert_eq!(decision.input_tokens, None);
    assert_eq!(decision.output_tokens, None);
    assert_eq!(decision.answers["choice"].choice.as_deref(), Some("b"));
    assert_eq!(decision.answers["score"].score, Some(1.25));
}
#[tokio::test]
async fn timeout_and_cancellation_do_not_dispatch_another_request() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(1))
                .set_body_json(reply()),
        )
        .mount(&server)
        .await;
    let target = target(&server.uri(), "synthetic");
    let key = BoundCredential::new(target.binding(), "dummy-key".into()).unwrap();
    let mut short = limits();
    short.timeout_ms = 50;
    assert_eq!(
        SystemOneDecider::new(target, key, short)
            .unwrap()
            .decide(&serde_json::Value::Null, &questions())
            .await,
        Err(DecisionError::Unavailable)
    );
    assert!(server.received_requests().await.unwrap().len() <= 1);
    let decider = client(&server.uri());
    let questions = questions();
    let mut future = Box::pin(decider.decide(&serde_json::Value::Null, &questions));
    tokio::select! { _=&mut future => panic!("answered before cancellation"), _=tokio::time::sleep(Duration::from_millis(30)) => {} }
    drop(future);
    assert!(server.received_requests().await.unwrap().len() <= 2);
}

#[tokio::test]
async fn rejected_reply_retains_reported_usage_without_inventing_a_verified_model() {
    for mismatch in [false, true] {
        let server = MockServer::start().await;
        let mut body = reply();
        if mismatch {
            body["model"] = serde_json::json!("different-model");
        } else {
            body["answers"]["q"]["type"] = serde_json::json!("choice");
        }
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(1)
            .mount(&server)
            .await;
        let meter = super::super::DecisionMeter::default();
        let provider = meter.wrap(std::sync::Arc::new(client(&server.uri())));
        let observed = provider
            .observe(&serde_json::Value::Null, &questions())
            .await;
        assert_eq!(observed.result, Err(DecisionError::InvalidAnswer));
        assert_eq!(
            observed.identity.model.as_deref(),
            if mismatch { None } else { Some("synthetic") }
        );
        assert_eq!(observed.attempts[0].input_tokens, Some(7));
        assert_eq!(observed.attempts[0].output_tokens, Some(0));
        assert_eq!(
            observed.attempts[0].reason.as_deref(),
            Some("usage_reported_by_rejected_reply")
        );
        let usage = meter.snapshot();
        assert_eq!(usage.input_tokens, 7);
        assert_eq!(usage.output_tokens, 0);
        assert_eq!(usage.failed, 1);
        assert_eq!(usage.reported, 0);
        let trace = meter.recent().pop().unwrap();
        assert_eq!(trace.configured.model.as_deref(), Some("synthetic"));
        assert_eq!(trace.effective.unwrap().model, observed.identity.model);
    }
}
