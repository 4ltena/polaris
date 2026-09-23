//! A local stub stands in for the service. No real endpoint is contacted
//! and no key is used.
use super::*;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn decider(server: &MockServer) -> JevDecider {
    JevDecider::new(&server.uri(), "test-key".into(), "jev-latest".into())
        .expect("the client builds")
}

fn one_question() -> BTreeMap<String, Question> {
    let mut held = BTreeMap::new();
    held.insert(
        "q".to_owned(),
        Question::Noul {
            instructions: "これは求められていることを行うか".into(),
        },
    );
    held
}

fn state() -> serde_json::Value {
    serde_json::json!({ "task": "何かしたい" })
}

fn answered(yes: f64) -> serde_json::Value {
    serde_json::json!({
        "model": "jev-latest",
        "answers": { "q": { "type": "noul", "noul": yes } },
        "usage": { "input_tokens": 120, "output_tokens": 0 },
    })
}

#[tokio::test]
async fn one_request_carries_the_state_and_every_question() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/systemone"))
        .and(header("authorization", "Bearer test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(answered(0.82)))
        .expect(1)
        .mount(&server)
        .await;

    let decision = decider(&server)
        .decide(&state(), &one_question())
        .await
        .expect("an answer");
    assert_eq!(decision.answers["q"].noul, Some(0.82));
    // Output is free on this model, so zero is a real figure rather than a
    // missing one.
    assert_eq!(decision.input_tokens, Some(120));
    assert_eq!(decision.output_tokens, Some(0));
}

#[tokio::test]
async fn an_answer_that_does_not_match_its_question_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "answers": { "q": { "type": "noul", "noul": 1.5 } }
        })))
        .mount(&server)
        .await;
    assert_eq!(
        decider(&server).decide(&state(), &one_question()).await,
        Err(DecisionError::InvalidAnswer),
        "a probability outside 0–1 is not an answer"
    );
}

#[tokio::test]
async fn a_reply_that_leaves_a_question_unanswered_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "answers": {}
        })))
        .mount(&server)
        .await;
    assert_eq!(
        decider(&server).decide(&state(), &one_question()).await,
        Err(DecisionError::InvalidAnswer)
    );
}

#[tokio::test]
async fn an_answer_to_something_that_was_never_asked_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "answers": { "q": { "type": "noul", "noul": 0.5 }, "unasked": { "type": "noul", "noul": 0.9 } }
        })))
        .mount(&server)
        .await;
    assert_eq!(
        decider(&server).decide(&state(), &one_question()).await,
        Err(DecisionError::InvalidAnswer)
    );
}

#[tokio::test]
async fn the_codes_that_will_not_change_are_not_retried() {
    for (code, expected) in [
        (401u16, DecisionError::Auth),
        (403, DecisionError::Auth),
        // The service answers 400 and 422 for a request it could not read.
        // Observed on 2026-09-17: a question missing `instructions` came
        // back 400, and a mistyped `criteria` came back 422.
        (400, DecisionError::Malformed),
        (422, DecisionError::Malformed),
        (413, DecisionError::TooLarge),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(code))
            // Asking again cannot change a rejected key or an oversized
            // request, and each retry costs the run time it does not have.
            .expect(1)
            .mount(&server)
            .await;
        assert_eq!(
            decider(&server).decide(&state(), &one_question()).await,
            Err(expected),
            "{code}"
        );
    }
}

#[tokio::test]
async fn a_service_that_could_not_answer_now_is_asked_again() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429))
        // The first attempt plus every backoff step.
        .expect(1 + BACKOFF.len() as u64)
        .mount(&server)
        .await;
    assert_eq!(
        decider(&server).decide(&state(), &one_question()).await,
        Err(DecisionError::Unavailable)
    );
}

#[tokio::test]
async fn finalizer_client_never_retries_an_unavailable_response() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429))
        .expect(1)
        .mount(&server)
        .await;
    let client = JevDecider::new_finalizer(&server.uri(), "test-key".into(), "jev-latest".into())
        .expect("one-shot client");
    assert_eq!(
        client.decide(&state(), &one_question()).await,
        Err(DecisionError::Unavailable)
    );
}

#[test]
fn finalizer_client_has_a_fixed_single_attempt_policy() {
    let client = JevDecider::new_finalizer(
        "https://api.example.invalid/v1",
        "test-key".into(),
        "jev-latest".into(),
    )
    .expect("one-shot client");
    assert_eq!(client.max_attempts, 1);
}

#[tokio::test]
async fn a_retry_that_succeeds_returns_the_answer() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(529))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(answered(0.4)))
        .expect(1)
        .mount(&server)
        .await;
    let decision = decider(&server)
        .decide(&state(), &one_question())
        .await
        .expect("the second attempt answers");
    assert_eq!(decision.answers["q"].noul, Some(0.4));
}

#[tokio::test]
async fn a_body_that_is_not_the_expected_shape_is_refused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_string("not json"))
        // Malformed is not "later"; asking again would give the same thing.
        .expect(1)
        .mount(&server)
        .await;
    assert_eq!(
        decider(&server).decide(&state(), &one_question()).await,
        Err(DecisionError::InvalidAnswer)
    );
}

#[test]
fn a_client_without_a_key_is_not_built() {
    assert_eq!(
        JevDecider::new("https://example.invalid/v1", String::new(), "m".into())
            .err()
            .expect("refused"),
        DecisionError::Auth
    );
    assert_eq!(
        JevDecider::new("https://example.invalid/v1", "   ".into(), "m".into())
            .err()
            .expect("refused"),
        DecisionError::Auth,
        "whitespace is not a key"
    );
}

#[test]
fn the_endpoint_is_built_from_the_versioned_root() {
    let held = JevDecider::new("https://api.example.invalid/v1/", "k".into(), "m".into())
        .expect("the client builds");
    assert_eq!(held.endpoint, "https://api.example.invalid/v1/systemone");
}
