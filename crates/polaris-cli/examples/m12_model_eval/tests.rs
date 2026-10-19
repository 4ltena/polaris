//! Offline positive and failure controls at the actual agent/provider boundary.
use super::*;
use polaris_provider::{
    CompletionRequest, CompletionResponse, Message, Token, TokenSource, ToolCall, Usage,
    attempts::{AttemptRecord, AttemptStatus, BudgetReservation, UsageObservation},
    turn_affinity::TurnContext,
};
use std::sync::{
    Mutex,
    atomic::{AtomicUsize, Ordering},
};

fn usage() -> Usage {
    Usage {
        input_tokens: 100,
        output_tokens: 10,
        total_tokens: 110,
        cached_tokens: 0,
    }
}
fn metric(id: &str, usage: Option<Usage>, failed: bool) -> Value {
    json!({"attempt_id":id,"model":ROOT_MODEL,"effort":EFFORT,"transport":"http",
        "outcome":if failed {"decode_error"} else {"completed"},
        "usage":usage.map(|u| json!({"input_tokens":u.input_tokens,"output_tokens":u.output_tokens,
            "cache_read_tokens":u.cached_tokens,"cache_write_tokens":null,"reasoning_tokens":3}))})
}
struct Scripted {
    result: Mutex<Option<Result<CompletionResponse, ProviderError>>>,
    requests: Mutex<Vec<CompletionRequest>>,
    calls: AtomicUsize,
    metrics: PathBuf,
}
#[async_trait::async_trait]
impl Provider for Scripted {
    async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        panic!("unscoped call")
    }
    async fn complete_in_turn(
        &self,
        request: CompletionRequest,
        _: &TurnContext,
    ) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push(request);
        let result = self
            .result
            .lock()
            .unwrap()
            .take()
            .expect("unexpected second request");
        let row = metric("attempt-1", guard::usage_of(&result), result.is_err());
        let mut out = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.metrics)
            .unwrap();
        writeln!(out, "{row}").unwrap();
        result
    }
    fn model(&self) -> Option<String> {
        Some(ROOT_MODEL.into())
    }
}
fn rig(
    dir: &Path,
    expected: &str,
    instructions: &str,
    result: Result<CompletionResponse, ProviderError>,
    fee: u64,
) -> (Arc<Scripted>, Arc<guard::Guard>) {
    let inner = Arc::new(Scripted {
        result: Mutex::new(Some(result)),
        requests: Mutex::new(vec![]),
        calls: AtomicUsize::new(0),
        metrics: dir.join("metrics.jsonl"),
    });
    let guard = Arc::new(guard::Guard::new(
        inner.clone(),
        expected.into(),
        instructions.into(),
        inner.metrics.clone(),
        0,
        fee,
    ));
    (inner, guard)
}
fn request(text: &str) -> CompletionRequest {
    CompletionRequest {
        system: "fixed".into(),
        messages: vec![Message::user(text)],
        tools: vec![],
    }
}
fn response(text: String) -> CompletionResponse {
    CompletionResponse {
        text,
        usage: Some(usage()),
        ..Default::default()
    }
}

#[tokio::test]
async fn actual_agent_sends_off_and_projected_input_once_and_reopens_original() {
    for mode in [InputPreparationMode::Off, InputPreparationMode::RepeatRefs] {
        let dir = tempfile::tempdir().unwrap();
        let args = input::tests::args(dir.path());
        let case = Case::load(&args).unwrap();
        let (expected, _) = projection(&case, mode).unwrap();
        assert_eq!(expected == case.input, mode == InputPreparationMode::Off);
        #[cfg(unix)]
        {
            let answer = input::tests::correct_reply(&case).to_string();
            let (inner, guard) = rig(
                dir.path(),
                &expected,
                &case.instructions(),
                Ok(response(answer)),
                5_000_000,
            );
            let trial = run_agent(&case, mode, guard.clone(), &dir.path().join("trial"))
                .await
                .unwrap();
            assert!(trial.raw_verified);
            let outcome = trial.outcome.unwrap();
            assert_eq!(case.score(&outcome.text)["quality_complete"], true);
            assert_eq!(trial.usage.reported_responses, 1);
            assert_eq!(trial.usage.usage, usage());
            assert_eq!(outcome.usage_report, trial.usage);
            assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
            let sent = inner.requests.lock().unwrap();
            assert!(sent[0].tools.is_empty());
            assert_eq!(sent[0].messages[0].content, expected);
            assert!(
                sent[0].system.contains(&case.instructions()),
                "format must survive normal prompt caps"
            );
            assert!(!sent[0].system.contains("GOLDEN_"));
            let evidence = guard.evidence.lock().unwrap();
            assert!(evidence.error.is_none());
            assert_eq!(
                evidence.request.as_ref().unwrap()["user_sha256"],
                content_hash(expected.as_bytes())
            );
            assert_eq!(
                evidence.diagnostics.as_ref().unwrap()["parts"]["tool_calls"],
                0
            );
        }
    }
}

#[tokio::test]
#[cfg(unix)]
async fn tools_are_rejected_before_agent_can_execute_or_spawn() {
    let dir = tempfile::tempdir().unwrap();
    let case = Case::load(&input::tests::args(dir.path())).unwrap();
    let mut result = response(String::new());
    result.tool_calls.push(ToolCall {
        id: "one".into(),
        name: "bash".into(),
        arguments: json!({"command":"must never execute"}),
    });
    let (inner, guard) = rig(
        dir.path(),
        &case.input,
        &case.instructions(),
        Ok(result),
        5_000_000,
    );
    let trial = run_agent(
        &case,
        InputPreparationMode::Off,
        guard,
        &dir.path().join("trial"),
    )
    .await
    .unwrap();
    assert!(trial.outcome.is_err());
    assert!(trial.raw_verified);
    assert_eq!(trial.usage.usage, usage());
    assert_eq!(trial.usage.reported_responses, 1);
    // UsageMeter classifies known failed usage as a reported response.
    assert_eq!(trial.usage.failed_requests, 0);
    assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_429_keeps_its_bounded_cause_separate_from_missing_usage() {
    let dir = tempfile::tempdir().unwrap();
    let detail = r#"rate limited. retry-after: 60 seconds. {"error":{"type":"usage_limit_reached","code":"usage_limit_reached","resets_at":1800000000,"message":"PRIVATE_SENTINEL","account_id":"PRIVATE_SENTINEL"}}"#;
    let (inner, guard) = rig(
        dir.path(),
        "input",
        "fixed",
        Err(ProviderError::Http(detail.into())),
        5_000_000,
    );
    let meter = UsageMeter::default();
    let wrapped = meter.wrap(guard.clone());
    assert!(
        wrapped
            .complete_in_turn(request("input"), &TurnContext::new())
            .await
            .is_err()
    );
    let evidence = guard.evidence.lock().unwrap();
    assert_eq!(evidence.error, Some("response usage missing"));
    assert_eq!(
        evidence.provider_error.as_ref().unwrap(),
        &json!({
            "kind":"http", "status":429, "retry_after_seconds":60,
            "error_type":"usage_limit_reached", "error_code":"usage_limit_reached",
            "resets_at":1800000000, "body_retained":false
        })
    );
    assert!(
        !evidence
            .provider_error
            .as_ref()
            .unwrap()
            .to_string()
            .contains("PRIVATE_SENTINEL")
    );
    assert!(evidence.known_cost_upper.is_none());
    drop(evidence);
    assert_eq!(meter.snapshot().reported_responses, 0);
    assert_eq!(meter.snapshot().failed_requests, 1);
    assert!(
        wrapped
            .complete_in_turn(request("input"), &TurnContext::new())
            .await
            .is_err()
    );
    assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn missing_usage_known_failure_and_output_or_fee_stops_do_not_retry() {
    let mut high_output = response("too much".into());
    high_output.usage = Some(Usage {
        input_tokens: 100,
        output_tokens: 4097,
        total_tokens: 4197,
        cached_tokens: 0,
    });
    for (result, fee, known) in [
        (
            Ok(CompletionResponse {
                text: "missing".into(),
                ..Default::default()
            }),
            5_000_000,
            false,
        ),
        (
            Err(ProviderError::Http("synthetic 429".into())),
            5_000_000,
            false,
        ),
        (
            Err(ProviderError::ReceivedUsage {
                source: Box::new(ProviderError::Decode("synthetic failure".into())),
                usage: usage(),
            }),
            5_000_000,
            true,
        ),
        (Ok(high_output), 5_000_000, true),
        (Ok(response("cost".into())), 1, true),
    ] {
        let dir = tempfile::tempdir().unwrap();
        let (inner, guard) = rig(dir.path(), "input", "fixed", result, fee);
        let meter = UsageMeter::default();
        let wrapped = meter.wrap(guard.clone());
        assert!(
            wrapped
                .complete_in_turn(request("input"), &TurnContext::new())
                .await
                .is_err()
        );
        assert_eq!(meter.snapshot().reported_responses, u64::from(known));
        assert_eq!(meter.snapshot().failed_requests, u64::from(!known));
        assert!(
            guard
                .complete_in_turn(request("input"), &TurnContext::new())
                .await
                .is_err()
        );
        assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            guard.evidence.lock().unwrap().known_cost_upper.is_some(),
            known
        );
    }
}

#[tokio::test]
async fn wrong_projection_unscoped_call_and_second_call_are_denied() {
    for wrong in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let (inner, guard) = rig(
            dir.path(),
            "expected",
            "fixed",
            Ok(response("answer".into())),
            5_000_000,
        );
        let result = if wrong {
            guard
                .complete_in_turn(request("wrong"), &TurnContext::new())
                .await
        } else {
            guard.complete(request("expected")).await
        };
        assert!(result.is_err());
        assert!(
            guard
                .complete_in_turn(request("expected"), &TurnContext::new())
                .await
                .is_err()
        );
        assert_eq!(inner.calls.load(Ordering::SeqCst), 0);
    }
    let dir = tempfile::tempdir().unwrap();
    let (inner, guard) = rig(
        dir.path(),
        "expected",
        "fixed",
        Ok(response("answer".into())),
        5_000_000,
    );
    assert!(
        guard
            .complete_in_turn(request("expected"), &TurnContext::new())
            .await
            .is_ok()
    );
    assert!(
        guard
            .complete_in_turn(request("expected"), &TurnContext::new())
            .await
            .is_err()
    );
    assert_eq!(inner.calls.load(Ordering::SeqCst), 1);
    assert!(guard::NoChildren.complete(request("child")).await.is_err());
}

#[test]
fn provider_diagnostics_are_bounded_and_do_not_retain_arbitrary_error_text() {
    for message in [
        "rate limited. retry-after: unknown seconds. not-json PRIVATE_SENTINEL".to_owned(),
        r#"status 503 Service Unavailable: {"error":{"type":"PRIVATE_SENTINEL","code":"PRIVATE_SENTINEL","message":"PRIVATE_SENTINEL"}}"#.to_owned(),
        format!("rate limited. retry-after: 12 seconds. {}", "PRIVATE_SENTINEL".repeat(2000)),
    ] {
        let diagnostic = guard::provider_diagnostic(&ProviderError::Http(message));
        let serialized = diagnostic.to_string();
        assert!(serialized.len() < 512);
        assert!(!serialized.contains("PRIVATE_SENTINEL"));
        assert_eq!(diagnostic["body_retained"], false);
    }
    let wrapped = ProviderError::ReceivedUsage {
        source: Box::new(ProviderError::Http(
            "status 503 Service Unavailable: {}".into(),
        )),
        usage: usage(),
    };
    assert_eq!(guard::provider_diagnostic(&wrapped)["status"], 503);
    assert_eq!(
        guard::provider_diagnostic(&ProviderError::Auth("PRIVATE_SENTINEL".into())),
        json!({"kind":"auth","body_retained":false})
    );
}

#[tokio::test]
async fn frozen_tokens_never_refresh_and_expired_tokens_do_not_send() {
    let tokens = eval_auth::FixedTokens {
        token: Token {
            access_token: "synthetic".into(),
            account_id: "synthetic".into(),
            effort: Some(EFFORT.into()),
        },
        expires_at: 1,
    };
    assert!(tokens.token().await.is_err());
    assert!(tokens.refreshed().await.is_err());
}

#[test]
fn physical_identity_and_meter_totals_must_match() {
    let u = usage();
    let attempt = AttemptRecord {
        logical_id: "logical-1".into(),
        attempt_id: "attempt-1".into(),
        parent_id: None,
        kind: AttemptKind::Completion,
        model: ROOT_MODEL.into(),
        started_at_ms: 1,
        finished_at_ms: Some(2),
        status: AttemptStatus::Succeeded,
        usage: UsageObservation::Known {
            input_tokens: u.input_tokens,
            output_tokens: u.output_tokens,
            total_tokens: u.total_tokens,
            cached_tokens: u.cached_tokens,
        },
        retry_reason: None,
        reservation: BudgetReservation::default(),
    };
    let report = polaris_provider::UsageReport {
        usage: u,
        reported_responses: 1,
        ..Default::default()
    };
    let row = metric("independent-metric-id", Some(u), false);
    let allowed = [(AttemptKind::Completion, ROOT_MODEL)];
    assert!(
        accounting::reconcile(
            std::slice::from_ref(&attempt),
            std::slice::from_ref(&row),
            &report,
            EFFORT,
            &allowed
        )
        .is_ok()
    );
    let mut wrong = row.clone();
    wrong["model"] = json!("unexpected-model");
    assert!(
        accounting::reconcile(
            std::slice::from_ref(&attempt),
            &[wrong],
            &report,
            EFFORT,
            &allowed
        )
        .is_err()
    );
    let mut wrong = report;
    wrong.usage.input_tokens += 1;
    assert!(accounting::reconcile(&[attempt], &[row], &wrong, EFFORT, &allowed).is_err());
}
