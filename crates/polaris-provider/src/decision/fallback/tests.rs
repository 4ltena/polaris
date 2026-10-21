//! Synthetic failover, cancellation and per-request provenance; no model or endpoint.
use super::*;
use crate::decision::{Answer, BandCalibration};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fake {
    name: &'static str,
    error: Option<DecisionError>,
    calls: AtomicUsize,
    pending: bool,
    invalid: bool,
    started: tokio::sync::Notify,
}
impl Fake {
    fn new(name: &'static str, error: Option<DecisionError>) -> Arc<Self> {
        Arc::new(Self {
            name,
            error,
            calls: AtomicUsize::new(0),
            pending: false,
            invalid: false,
            started: tokio::sync::Notify::new(),
        })
    }
}
#[async_trait::async_trait]
impl Decider for Fake {
    fn name(&self) -> &str {
        self.name
    }
    fn capabilities(&self) -> DecisionCapabilities {
        DecisionCapabilities {
            band_calibration: if self.name == "primary" {
                BandCalibration::LegacyJev
            } else {
                BandCalibration::Unmeasured
            },
            ..Default::default()
        }
    }
    async fn decide(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.started.notify_one();
        if self.pending {
            std::future::pending::<()>().await;
        }
        if state["allow_primary"] != true
            && let Some(error) = &self.error
        {
            return Err(error.clone());
        }
        if self.invalid {
            return Ok(Decision {
                answers: BTreeMap::new(),
                input_tokens: Some(3),
                output_tokens: Some(0),
            });
        }
        Ok(Decision {
            answers: questions
                .keys()
                .map(|key| {
                    (
                        key.clone(),
                        Answer {
                            noul: Some(0.7),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            input_tokens: Some(3),
            output_tokens: Some(1),
        })
    }
}
fn policy(n: usize) -> FallbackPolicy {
    FallbackPolicy {
        max_attempts: n,
        attempt_timeout: Duration::from_secs(1),
        total_timeout: Duration::from_secs(2),
    }
}
fn questions() -> BTreeMap<String, Question> {
    BTreeMap::from([(
        "q".into(),
        Question::Noul {
            instructions: "synthetic".into(),
        },
    )])
}

#[tokio::test]
async fn an_explicit_chain_keeps_order_and_attributes_the_actual_answer() {
    let first = Fake::new("primary", Some(DecisionError::Unavailable));
    let second = Fake::new("secondary", None);
    let chain = FallbackDecider::new(vec![first.clone(), second.clone()], policy(2)).unwrap();
    let reply = chain.observe(&serde_json::Value::Null, &questions()).await;
    assert!(reply.result.is_ok());
    assert_eq!(reply.identity.backend, "secondary");
    assert_eq!(
        reply.capabilities.band_calibration,
        BandCalibration::Unmeasured
    );
    assert_eq!(
        reply
            .attempts
            .iter()
            .map(|a| a.identity.backend.as_str())
            .collect::<Vec<_>>(),
        ["primary", "secondary"]
    );
    assert_eq!(reply.attempts[0].error, Some(DecisionError::Unavailable));
    assert_eq!(reply.attempts[1].input_tokens, Some(3));
    assert_eq!(first.calls.load(Ordering::SeqCst), 1);
    assert_eq!(second.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_input_limit_can_switch_to_the_next_explicit_backend() {
    let first = Fake::new("primary", Some(DecisionError::TooLarge));
    let second = Fake::new("secondary", None);
    let chain = FallbackDecider::new(vec![first.clone(), second.clone()], policy(2)).unwrap();
    let result = chain.observe(&serde_json::Value::Null, &questions()).await;
    assert!(result.result.is_ok());
    assert_eq!(result.attempts[0].error, Some(DecisionError::TooLarge));
    assert_eq!(result.identity.backend, "secondary");
    assert_eq!(first.calls.load(Ordering::SeqCst), 1);
    assert_eq!(second.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn auth_malformed_invalid_and_cancelled_never_start_another_backend() {
    for error in [
        DecisionError::Auth,
        DecisionError::Malformed,
        DecisionError::InvalidAnswer,
        DecisionError::Cancelled,
    ] {
        let first = Fake::new("primary", Some(error.clone()));
        let second = Fake::new("secondary", None);
        let chain = FallbackDecider::new(vec![first.clone(), second.clone()], policy(2)).unwrap();
        let result = chain.observe(&serde_json::Value::Null, &questions()).await;
        assert_eq!(result.result, Err(error));
        assert_eq!(first.calls.load(Ordering::SeqCst), 1);
        assert_eq!(second.calls.load(Ordering::SeqCst), 0);
    }
}

#[tokio::test]
async fn attempt_limit_and_timeout_are_request_local() {
    let first = Fake::new("primary", Some(DecisionError::TooLarge));
    let second = Fake::new("secondary", None);
    let chain = FallbackDecider::new(vec![first, second.clone()], policy(1)).unwrap();
    assert_eq!(
        chain.decide(&serde_json::Value::Null, &questions()).await,
        Err(DecisionError::TooLarge)
    );
    assert_eq!(second.calls.load(Ordering::SeqCst), 0);
    let mut pending = Fake::new("primary", None);
    Arc::get_mut(&mut pending).unwrap().pending = true;
    let mut limit = policy(2);
    limit.attempt_timeout = Duration::from_millis(20);
    let chain = FallbackDecider::new(vec![pending, second.clone()], limit).unwrap();
    let result = chain.observe(&serde_json::Value::Null, &questions()).await;
    assert!(result.result.is_ok());
    assert!(result.attempts[0].timed_out);
    assert_eq!(second.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn dropping_an_active_chain_does_not_dispatch_the_next_backend() {
    let mut first = Fake::new("primary", None);
    Arc::get_mut(&mut first).unwrap().pending = true;
    let second = Fake::new("secondary", None);
    let chain = FallbackDecider::new(vec![first.clone(), second.clone()], policy(2)).unwrap();
    let task =
        tokio::spawn(async move { chain.observe(&serde_json::Value::Null, &questions()).await });
    first.started.notified().await;
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    assert_eq!(first.calls.load(Ordering::SeqCst), 1);
    assert_eq!(second.calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn concurrent_replies_keep_their_own_provenance() {
    let first = Fake::new("primary", Some(DecisionError::Unavailable));
    let second = Fake::new("secondary", None);
    let chain = FallbackDecider::new(vec![first, second], policy(2)).unwrap();
    let yes = serde_json::json!({"allow_primary":true});
    let q = questions();
    let (one, two) = tokio::join!(
        chain.observe(&yes, &q),
        chain.observe(&serde_json::Value::Null, &q)
    );
    assert_eq!(one.identity.backend, "primary");
    assert_eq!(one.attempts.len(), 1);
    assert_eq!(two.identity.backend, "secondary");
    assert_eq!(two.attempts.len(), 2);
}

#[tokio::test]
async fn invalid_answers_and_expired_total_budget_never_fall_through() {
    let mut first = Fake::new("primary", None);
    Arc::get_mut(&mut first).unwrap().invalid = true;
    let second = Fake::new("secondary", None);
    let chain = FallbackDecider::new(vec![first, second.clone()], policy(2)).unwrap();
    assert_eq!(
        chain.decide(&serde_json::Value::Null, &questions()).await,
        Err(DecisionError::InvalidAnswer)
    );
    assert_eq!(second.calls.load(Ordering::SeqCst), 0);
    let mut first = Fake::new("primary", None);
    Arc::get_mut(&mut first).unwrap().pending = true;
    let limits = FallbackPolicy {
        max_attempts: 2,
        attempt_timeout: Duration::from_millis(20),
        total_timeout: Duration::from_millis(20),
    };
    let chain = FallbackDecider::new(vec![first.clone(), second.clone()], limits).unwrap();
    let result = chain.observe(&serde_json::Value::Null, &questions()).await;
    assert_eq!(result.result, Err(DecisionError::Unavailable));
    assert_eq!(first.calls.load(Ordering::SeqCst), 1);
    assert_eq!(second.calls.load(Ordering::SeqCst), 0);
    assert!(result.attempts[0].timed_out);
}

#[tokio::test]
async fn metered_traces_are_bounded_and_never_store_request_content() {
    let first = Fake::new("primary", Some(DecisionError::Unavailable));
    let second = Fake::new("secondary", None);
    let chain = Arc::new(FallbackDecider::new(vec![first, second], policy(2)).unwrap());
    let meter = crate::decision::DecisionMeter::default();
    let wrapped = meter.wrap(chain);
    for _ in 0..40 {
        wrapped
            .decide(
                &serde_json::json!({"private":"never-store-this-state"}),
                &questions(),
            )
            .await
            .unwrap();
    }
    let traces = meter.recent();
    assert_eq!(traces.len(), 32);
    assert_eq!(traces[0].configured.backend, "primary");
    assert_eq!(traces[0].effective.backend, "secondary");
    assert_eq!(meter.snapshot().failed, 40);
    assert_eq!(meter.snapshot().reported, 40);
    assert!(
        !serde_json::to_string(&traces)
            .unwrap()
            .contains("never-store-this-state")
    );
}
