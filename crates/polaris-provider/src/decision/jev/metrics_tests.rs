//! Physical attempts survive invalid answers and cancellation without payloads.
use super::*;
use serde_json::{Value, json};
use wiremock::matchers::method;
use wiremock::{Mock, MockServer, ResponseTemplate};

fn client(server: &MockServer, path: &std::path::Path) -> JevDecider {
    let mut held =
        JevDecider::new(&server.uri(), "private-key".into(), "jev-latest".into()).expect("client");
    held.metrics_path = Some(path.to_owned());
    held.max_attempts = 1;
    held
}

fn questions() -> BTreeMap<String, Question> {
    BTreeMap::from([(
        "q".into(),
        Question::Noul {
            instructions: "private-question".into(),
        },
    )])
}

fn records(path: &std::path::Path) -> Vec<Value> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

#[test]
fn a_retry_override_requires_the_explicit_benchmark_contract() {
    assert_eq!(benchmark_options(None, None, None).unwrap(), (None, 4));
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metrics.jsonl");
    for (report, attempts) in [
        (None, Some("1")),
        (Some("1"), None),
        (Some("1"), Some("2")),
        (Some("0"), Some("1")),
    ] {
        assert_eq!(
            benchmark_options(
                report.map(Into::into),
                Some(path.clone().into_os_string()),
                attempts.map(Into::into)
            ),
            Err(DecisionError::Malformed)
        );
    }
    assert_eq!(
        benchmark_options(
            Some("1".into()),
            Some("relative.jsonl".into()),
            Some("1".into())
        ),
        Err(DecisionError::Malformed)
    );
    assert_eq!(
        benchmark_options(
            Some("1".into()),
            Some(path.clone().into_os_string()),
            Some("1".into())
        )
        .unwrap(),
        (Some(path.clone()), 1)
    );
    assert!(
        records(&path).is_empty(),
        "zero calls have an existing empty journal"
    );
}

#[tokio::test]
async fn usage_is_preserved_even_when_the_typed_answer_is_invalid() {
    for (answer, outcome) in [
        (json!({"type":"noul", "noul":0.8}), "completed"),
        (json!({"type":"noul", "noul":1.5}), "invalid_answer"),
        (
            json!({"type":"noul", "noul":"private-bad-answer"}),
            "invalid_answer",
        ),
    ] {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "answers":{"q":answer}, "usage":{"input_tokens":120,"output_tokens":0}
            })))
            .expect(1)
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("metrics.jsonl");
        let result = client(&server, &path)
            .decide(&json!("private-state"), &questions())
            .await;
        assert_eq!(result.is_ok(), outcome == "completed");
        let rows = records(&path);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["event"], "started");
        assert_eq!(rows[0]["usage"]["input_tokens"], Value::Null);
        assert_eq!(rows[0]["attempt_id"], rows[1]["attempt_id"]);
        assert_eq!(rows[1]["event"], "finished");
        assert_eq!(rows[1]["outcome"], outcome);
        assert_eq!(rows[1]["http_status"], 200);
        assert_eq!(
            rows[1]["usage"],
            json!({"input_tokens":120,"output_tokens":0})
        );
        assert!(!std::fs::read_to_string(&path).unwrap().contains("private-"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}

#[tokio::test]
async fn benchmark_rate_limiting_is_a_single_unknown_attempt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(429).set_body_string("private-error"))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metrics.jsonl");
    assert_eq!(
        client(&server, &path)
            .decide(&json!({}), &questions())
            .await,
        Err(DecisionError::Unavailable)
    );
    let rows = records(&path);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["http_status"], 429);
    assert_eq!(rows[1]["outcome"], "unavailable");
    assert_eq!(
        rows[1]["usage"],
        json!({"input_tokens":null,"output_tokens":null})
    );
}

#[tokio::test]
async fn cancelling_in_flight_preserves_the_unknown_attempt() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_delay(Duration::from_secs(2)))
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metrics.jsonl");
    let client = client(&server, &path);
    assert!(
        tokio::time::timeout(
            Duration::from_millis(50),
            client.decide(&json!({}), &questions())
        )
        .await
        .is_err()
    );
    let rows = records(&path);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1]["outcome"], "cancelled");
    assert_eq!(rows[1]["usage"]["input_tokens"], Value::Null);
}

#[cfg(unix)]
#[tokio::test]
async fn an_unsafe_journal_prevents_network_dispatch() {
    use std::os::unix::fs::PermissionsExt;
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metrics.jsonl");
    std::fs::write(&path, "").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert_eq!(
        client(&server, &path)
            .decide(&json!({}), &questions())
            .await,
        Err(DecisionError::Malformed)
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn retries_have_distinct_physical_ids_and_one_logical_id() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(503))
        .up_to_n_times(1)
        .expect(1)
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "answers":{"q":{"type":"noul","noul":0.8}}, "usage":{"input_tokens":120}
        })))
        .expect(1)
        .mount(&server)
        .await;
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("metrics.jsonl");
    let mut client = client(&server, &path);
    client.max_attempts = 4;
    assert!(client.decide(&json!({}), &questions()).await.is_ok());
    let rows = records(&path);
    assert_eq!(rows.len(), 4);
    assert_ne!(rows[0]["attempt_id"], rows[2]["attempt_id"]);
    assert_eq!(rows[0]["logical_id"], rows[2]["logical_id"]);
    assert_eq!(rows[1]["outcome"], "unavailable");
    assert_eq!(rows[3]["outcome"], "completed");
    assert_eq!(rows[3]["usage"]["output_tokens"], Value::Null);
}
