//! Factory failover uses synthetic owners and loopback HTTP, never real models or credentials.
use super::*;
use crate::config::DecisionFallback;
use polaris_provider::decision::{Answer, BandCalibration, DecisionMeter};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Local {
    fail: bool,
    calibrated: bool,
    calls: AtomicUsize,
    identity: Option<DecisionIdentity>,
}
#[async_trait::async_trait]
impl Decider for Local {
    fn name(&self) -> &str {
        "clef"
    }
    fn identity(&self) -> DecisionIdentity {
        self.identity.clone().unwrap_or(DecisionIdentity {
            backend: "clef".into(),
            model: None,
            revision: None,
            quantization: None,
        })
    }
    fn capabilities(&self) -> DecisionCapabilities {
        // Synthetic primary enables the historical policy only to test that
        // its calibration is not transferred to the fallback's answer.
        DecisionCapabilities {
            network: polaris_provider::NetworkBoundary::LocalOnly,
            band_calibration: if self.calibrated {
                BandCalibration::LegacyJev
            } else {
                BandCalibration::Unmeasured
            },
            ..Default::default()
        }
    }
    async fn decide(
        &self,
        _: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.fail {
            return Err(DecisionError::Unavailable);
        }
        Ok(Decision {
            answers: questions
                .keys()
                .map(|key| {
                    (
                        key.clone(),
                        Answer {
                            noul: Some(0.0),
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
fn config() -> DecisionConfig {
    DecisionConfig {
        enabled: true,
        local_only: true,
        backend: DecisionBackend::Clef,
        model: Some("first".into()),
        fallback: Some(DecisionFallback {
            targets: vec![DecisionConfig {
                enabled: true,
                backend: DecisionBackend::Clef,
                model: Some("second".into()),
                ..Default::default()
            }],
            max_attempts: 2,
            attempt_timeout_ms: 1000,
            total_timeout_ms: 2000,
        }),
        ..Default::default()
    }
}
fn local(fail: bool, calibrated: bool) -> Arc<Local> {
    Arc::new(Local {
        fail,
        calibrated,
        calls: AtomicUsize::new(0),
        identity: None,
    })
}
fn build(config: &DecisionConfig, first: Arc<Local>, second: Arc<Local>) -> Arc<dyn Decider> {
    super::super::build_with_sources(
        config,
        DecisionUse::Routing,
        || panic!("local chain read a remote key"),
        |id| {
            Ok(if id == "first" {
                first.clone() as Arc<dyn Decider>
            } else {
                second.clone() as Arc<dyn Decider>
            })
        },
    )
    .unwrap()
    .unwrap()
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
async fn fallback_factory_and_meter_preserve_actual_profile_and_each_attempt() {
    let identified = |fail, calibrated, model: &str| {
        Arc::new(Local {
            fail,
            calibrated,
            calls: AtomicUsize::new(0),
            identity: Some(DecisionIdentity {
                backend: "clef".into(),
                model: Some(model.into()),
                revision: Some("a".repeat(40)),
                quantization: Some("synthetic-q4".into()),
            }),
        })
    };
    let first = identified(true, true, "first-weights");
    let second = identified(false, false, "second-weights");
    let meter = DecisionMeter::default();
    let chain = meter.wrap(build(&config(), first.clone(), second.clone()));
    let result = chain.observe(&serde_json::Value::Null, &questions()).await;
    assert!(result.result.is_ok());
    assert_eq!(result.identity.model.as_deref(), Some("second-weights"));
    assert_eq!(result.attempts.len(), 2);
    let usage = meter.snapshot();
    assert_eq!(
        (usage.failed, usage.reported, usage.input_tokens),
        (1, 1, 3)
    );
    let traces = meter.recent();
    assert_eq!(traces[0].configured.model.as_deref(), Some("first-weights"));
    assert_eq!(
        traces[0].effective.as_ref().unwrap().model.as_deref(),
        Some("second-weights")
    );
    assert_eq!(first.calls.load(Ordering::SeqCst), 1);
    assert_eq!(second.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn configured_profile_does_not_replace_unknown_observed_model() {
    let chain = build(&config(), local(true, true), local(false, false));
    assert_eq!(chain.identity().model.as_deref(), Some("first"));
    let observed = chain.observe(&serde_json::Value::Null, &questions()).await;
    assert!(observed.result.is_ok());
    assert_eq!(observed.identity.model, None);
    assert!(
        observed
            .attempts
            .iter()
            .all(|attempt| attempt.identity.model.is_none())
    );
}

#[tokio::test]
async fn fallback_answers_do_not_borrow_the_primary_band_calibration() {
    let chain = build(&config(), local(true, true), local(false, false));
    let bands = super::super::judge_with(
        chain.as_ref(),
        &serde_json::Value::Null,
        vec![Question::Noul {
            instructions: "synthetic".into(),
        }],
        std::future::pending(),
    )
    .await
    .unwrap();
    assert_eq!(bands.calibration, BandCalibration::Unmeasured);
    assert_eq!(bands.middle, [0]);
    assert!(bands.adopted.is_empty());
    assert_eq!(bands.excluded, 0);
}

#[tokio::test]
async fn fallback_can_reach_a_calibrated_target_after_an_uninstalled_primary() {
    let second = local(false, true);
    let chain = super::super::build_with_sources(
        &config(),
        DecisionUse::Routing,
        || panic!("local chain read a remote key"),
        |id| {
            if id == "first" {
                Err(ManagedDecisionError::NotInstalled)
            } else {
                Ok(second.clone() as Arc<dyn Decider>)
            }
        },
    )
    .unwrap()
    .unwrap();
    let meter = DecisionMeter::default();
    let chain = meter.wrap(chain);
    assert_eq!(
        chain.capabilities().band_calibration,
        BandCalibration::Unmeasured
    );
    assert!(chain.may_return_calibrated_bands());
    let bands = super::super::judge_with(
        chain.as_ref(),
        &serde_json::Value::Null,
        vec![Question::Noul {
            instructions: "synthetic".into(),
        }],
        std::future::pending(),
    )
    .await
    .unwrap();
    assert_eq!(bands.calibration, BandCalibration::LegacyJev);
    assert_eq!(bands.excluded, 1);
    assert_eq!(second.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn fallback_disabled_finalizer_and_local_only_do_not_acquire_other_resources() {
    let mut config = config();
    let unused = || panic!("unexpected credential read");
    assert!(
        super::super::build_with_sources(&config, DecisionUse::Finalization, unused, |_| panic!(
            "finalizer loaded a Clef generation"
        ))
        .unwrap()
        .is_none()
    );
    config.enabled = false;
    assert!(
        super::super::build_with_sources(&config, DecisionUse::Routing, unused, |_| panic!(
            "disabled chain loaded a generation"
        ))
        .unwrap()
        .is_none()
    );
    config.enabled = true;
    config.fallback.as_mut().unwrap().targets[0] = DecisionConfig {
        enabled: true,
        ..Default::default()
    };
    assert!(matches!(
        super::super::build_with_sources(&config, DecisionUse::Routing, unused, |_| panic!(
            "local-only unsafe chain loaded resources"
        )),
        Err(DecisionBuildError::LocalOnlyUnavailable)
    ));
}

#[tokio::test]
async fn fallback_budget_does_not_initialize_unreachable_credentials() {
    let mut config = config();
    config.local_only = false;
    let fallback = config.fallback.as_mut().unwrap();
    fallback.max_attempts = 1;
    fallback.targets[0] = DecisionConfig {
        enabled: true,
        ..Default::default()
    };
    let first = local(false, true);
    let chain = super::super::build_with_sources(
        &config,
        DecisionUse::Routing,
        || panic!("unreachable remote target read a credential"),
        |_| Ok(first.clone() as Arc<dyn Decider>),
    )
    .unwrap()
    .unwrap();
    assert!(
        chain
            .observe(&serde_json::Value::Null, &questions())
            .await
            .result
            .is_ok()
    );
    assert_eq!(first.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn fallback_uses_one_http_attempt_after_an_explicit_uninstalled_primary() {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    for succeeds in [true, false] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let requests = Arc::new(AtomicUsize::new(0));
        let counted = requests.clone();
        let server = tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let mut buffer = [0; 8192];
                let _ = stream.read(&mut buffer).await.unwrap();
                counted.fetch_add(1, Ordering::SeqCst);
                let body = r#"{"answers":{"q":{"noul":0.9}},"usage":{"input_tokens":4,"output_tokens":1}}"#;
                let response = if succeeds {
                    format!(
                        "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                } else {
                    "HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".into()
                };
                stream.write_all(response.as_bytes()).await.unwrap();
            }
        });
        let mut config = config();
        config.local_only = false;
        config.fallback.as_mut().unwrap().targets[0] = DecisionConfig {
            enabled: true,
            base_url: Some(format!("http://{address}")),
            ..Default::default()
        };
        let chain = super::super::build_with_sources(
            &config,
            DecisionUse::Routing,
            || Some("synthetic-only".into()),
            |_| Err(ManagedDecisionError::NotInstalled),
        )
        .unwrap()
        .unwrap();
        let result = chain.observe(&serde_json::Value::Null, &questions()).await;
        server.abort();
        let _ = server.await;
        assert_eq!(result.result.is_ok(), succeeds);
        assert_eq!(result.identity.backend, "jev");
        assert_eq!(result.attempts.len(), 2);
        assert_eq!(result.attempts[0].reason.as_deref(), Some("not_installed"));
        assert!(!result.attempts[0].invoked);
        assert_eq!(requests.load(Ordering::SeqCst), 1);
    }
}
