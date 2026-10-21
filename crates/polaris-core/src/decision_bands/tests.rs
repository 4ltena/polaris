//! A local stub stands in for the service. No real endpoint is contacted.
use super::*;
use polaris_provider::decision::{Answer, Decision};

struct Stub(Vec<f64>);

#[test]
fn local_only_refuses_remote_or_unconfined_judges_before_credentials_or_spawn() {
    use crate::config::{DecisionBackend, DecisionConfig};
    let unused = || panic!("local-only looked up a remote key");
    let config = DecisionConfig {
        enabled: true,
        local_only: true,
        ..Default::default()
    };
    assert!(matches!(
        build_with_key(&config, DecisionUse::Routing, unused),
        Err(DecisionBuildError::LocalOnlyUnavailable)
    ));
    let dir = tempfile::tempdir().unwrap();
    let helper = dir.path().join("helper");
    std::fs::write(&helper, b"synthetic helper never executed").unwrap();
    let local = DecisionConfig {
        backend: DecisionBackend::SemIf,
        helper: Some(helper),
        ..config
    };
    assert!(matches!(
        build_with_key(&local, DecisionUse::Routing, unused),
        Err(DecisionBuildError::LocalOnlyUnavailable)
    ));
    let disabled = DecisionConfig {
        enabled: false,
        ..local
    };
    assert!(
        build_with_key(&disabled, DecisionUse::Routing, unused)
            .unwrap()
            .is_none()
    );
}

#[test]
fn the_shared_factory_preserves_an_explicit_semif_selection() {
    let dir = tempfile::tempdir().unwrap();
    let helper = dir.path().join("semif-helper");
    std::fs::write(&helper, "#!/bin/sh\nexit 0\n").unwrap();
    let config = crate::config::DecisionConfig {
        enabled: true,
        backend: crate::config::DecisionBackend::SemIf,
        helper: Some(helper),
        ..Default::default()
    };
    assert_eq!(
        from_configuration(&config).map(|decider| decider.name().to_owned()),
        Some("semif".to_owned()),
        "a configured local judge must not be replaced by Jev or silently disabled"
    );
}

#[test]
fn disabled_local_and_unsupported_finalizer_paths_do_not_read_a_remote_key() {
    use crate::config::{DecisionBackend, DecisionConfig};
    let unused = || panic!("a disabled or local judge looked up a remote credential");
    assert!(
        build_with_key(&DecisionConfig::default(), DecisionUse::Routing, unused)
            .unwrap()
            .is_none()
    );
    let dir = tempfile::tempdir().unwrap();
    let helper = dir.path().join("helper");
    std::fs::write(&helper, b"synthetic, never executed").unwrap();
    let config = DecisionConfig {
        enabled: true,
        backend: DecisionBackend::SemIf,
        helper: Some(helper),
        ..Default::default()
    };
    assert_eq!(
        build_with_key(&config, DecisionUse::Routing, unused)
            .unwrap()
            .unwrap()
            .name(),
        "semif"
    );
    assert!(
        build_with_key(&config, DecisionUse::Finalization, unused)
            .unwrap()
            .is_none()
    );
    let missing = DecisionConfig {
        helper: None,
        ..config
    };
    assert!(matches!(
        build_with_key(&missing, DecisionUse::Routing, unused),
        Err(DecisionBuildError::MissingSemIfHelper)
    ));
}

#[test]
fn the_shared_factory_preserves_jev_and_reports_missing_or_empty_credentials() {
    let config = crate::config::DecisionConfig {
        enabled: true,
        ..Default::default()
    };
    for purpose in [DecisionUse::Routing, DecisionUse::Finalization] {
        assert!(matches!(
            build_with_key(&config, purpose, || None),
            Err(DecisionBuildError::MissingJevKey)
        ));
        assert!(matches!(
            build_with_key(&config, purpose, || Some(String::new())),
            Err(DecisionBuildError::InvalidSettings(DecisionError::Auth))
        ));
        assert_eq!(
            build_with_key(&config, purpose, || Some("synthetic-only".into()))
                .unwrap()
                .unwrap()
                .name(),
            "jev"
        );
    }
}

#[tokio::test]
async fn the_shared_finalizer_factory_does_not_retry_a_failed_observation() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let requests = Arc::new(AtomicUsize::new(0));
    let observed = requests.clone();
    let server = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 8192];
            let _ = stream.read(&mut buffer).await.unwrap();
            observed.fetch_add(1, Ordering::SeqCst);
            stream.write_all(
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            ).await.unwrap();
        }
    });
    let config = crate::config::DecisionConfig {
        enabled: true,
        base_url: Some(format!("http://{address}")),
        ..Default::default()
    };
    let decider = build_with_key(&config, DecisionUse::Finalization, || {
        Some("synthetic-only".into())
    })
    .unwrap()
    .unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        decider.decide(&serde_json::json!({}), &BTreeMap::new()),
    )
    .await;
    server.abort();
    let _ = server.await;
    assert!(matches!(result, Ok(Err(DecisionError::Unavailable))));
    assert_eq!(requests.load(Ordering::SeqCst), 1);
}

#[async_trait::async_trait]
impl Decider for Stub {
    fn capabilities(&self) -> polaris_provider::decision::DecisionCapabilities {
        // This synthetic fixture exercises the historical numerical policy.
        polaris_provider::decision::DecisionCapabilities {
            band_calibration: polaris_provider::decision::BandCalibration::LegacyJev,
            ..Default::default()
        }
    }

    async fn decide(
        &self,
        _: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        Ok(Decision {
            answers: questions
                .keys()
                .enumerate()
                .map(|(index, key)| {
                    (
                        key.clone(),
                        Answer {
                            noul: self.0.get(index).copied(),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            input_tokens: Some(1),
            output_tokens: Some(0),
        })
    }
}

fn candidates() -> Vec<(&'static str, &'static str)> {
    vec![("a", "ひとつめ"), ("b", "ふたつめ"), ("c", "みっつめ")]
}

async fn bands(answers: &[f64]) -> Result<Bands, Refused> {
    judge(
        &Stub(answers.to_vec()),
        &serde_json::json!({ "task": "何か" }),
        "何か",
        "skill",
        &candidates(),
        std::future::pending(),
    )
    .await
}

#[tokio::test]
async fn the_two_edges_are_closed_so_nothing_falls_between_them() {
    // Exactly on an edge belongs to the decided side of it.
    let held = bands(&[ADOPT, EXCLUDE, EXCLUDE + 0.01]).await.unwrap();
    assert_eq!(held.adopted, vec![0]);
    assert_eq!(held.middle, vec![2]);
    assert_eq!(held.excluded, 1);
    assert_eq!(
        held.adopted.len() + held.middle.len() + held.excluded,
        3,
        "every candidate lands in exactly one band"
    );
}

#[tokio::test]
async fn each_band_is_ordered_most_confident_first_and_ties_keep_their_order() {
    let held = bands(&[0.85, 0.95, 0.9]).await.unwrap();
    assert_eq!(held.adopted, vec![1, 2, 0]);
    let held = bands(&[0.9, 0.9, 0.9]).await.unwrap();
    assert_eq!(
        held.adopted,
        vec![0, 1, 2],
        "the result must not depend on sort details"
    );
}

#[tokio::test]
async fn a_reply_that_leaves_a_candidate_unanswered_is_refused() {
    // A narrower reply is not a narrower answer; it answers a different set.
    assert_eq!(bands(&[0.9, 0.9]).await, Err(Refused::InvalidOutput));
}

#[tokio::test]
async fn the_question_asks_what_is_useful_rather_than_what_finishes_the_job() {
    // The wording is part of the calibration: asking whether a candidate
    // accomplishes the request scores a means to the end like a failure.
    let Question::Noul { instructions } = question("依頼", "skill", "名前", "説明") else {
        panic!("candidates are asked about with a Noul");
    };
    assert!(instructions.contains("役に立つか"), "{instructions}");
    assert!(!instructions.contains("達成するか"), "{instructions}");
    assert!(
        instructions.contains("skill"),
        "the kind names what is judged"
    );
}

#[tokio::test]
async fn not_getting_an_answer_is_never_read_as_nothing_fits() {
    struct Failing(DecisionError);
    #[async_trait::async_trait]
    impl Decider for Failing {
        fn capabilities(&self) -> polaris_provider::decision::DecisionCapabilities {
            // This synthetic fixture exercises the historical numerical policy.
            polaris_provider::decision::DecisionCapabilities {
                band_calibration: polaris_provider::decision::BandCalibration::LegacyJev,
                ..Default::default()
            }
        }

        async fn decide(
            &self,
            _: &serde_json::Value,
            _: &BTreeMap<String, Question>,
        ) -> Result<Decision, DecisionError> {
            Err(self.0.clone())
        }
    }
    for (error, expected) in [
        (DecisionError::Unavailable, Refused::Unavailable),
        (DecisionError::Auth, Refused::Unavailable),
        (DecisionError::TooLarge, Refused::Unavailable),
        (DecisionError::Cancelled, Refused::Cancelled),
        (DecisionError::InvalidAnswer, Refused::InvalidOutput),
        (DecisionError::Malformed, Refused::InvalidOutput),
    ] {
        let held = judge(
            &Failing(error.clone()),
            &serde_json::json!({}),
            "何か",
            "skill",
            &candidates(),
            std::future::pending(),
        )
        .await;
        assert_eq!(held, Err(expected), "{error:?}");
    }
}

struct Unmeasured;
#[async_trait::async_trait]
impl Decider for Unmeasured {
    async fn decide(
        &self,
        _: &serde_json::Value,
        _: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        panic!("unmeasured probabilities must not be requested for numerical bands")
    }
}

#[tokio::test]
async fn an_unmeasured_backend_retains_all_candidates_without_inference() {
    let bands = judge(
        &Unmeasured,
        &serde_json::json!({}),
        "query",
        "skill",
        &candidates(),
        std::future::pending(),
    )
    .await
    .unwrap();
    assert_eq!(
        bands.calibration,
        polaris_provider::decision::BandCalibration::Unmeasured
    );
    assert!(bands.adopted.is_empty());
    assert_eq!(bands.middle, vec![0, 1, 2]);
    assert_eq!(bands.excluded, 0);
    assert_eq!(bands.context_candidates(), vec![0, 1, 2]);
    assert_eq!(
        judge(
            &Unmeasured,
            &serde_json::json!({}),
            "query",
            "skill",
            &candidates(),
            std::future::ready(())
        )
        .await,
        Err(Refused::Cancelled)
    );
}

#[test]
fn historical_context_selection_does_not_adopt_the_middle_band() {
    let bands = Bands {
        calibration: polaris_provider::decision::BandCalibration::LegacyJev,
        adopted: vec![2],
        middle: vec![0],
        excluded: 1,
    };
    assert_eq!(bands.context_candidates(), vec![2]);
}

#[tokio::test]
async fn a_legacy_reply_with_unrequested_answers_is_refused() {
    struct Extra;
    #[async_trait::async_trait]
    impl Decider for Extra {
        fn capabilities(&self) -> polaris_provider::decision::DecisionCapabilities {
            Stub(vec![]).capabilities()
        }
        async fn decide(
            &self,
            state: &serde_json::Value,
            asked: &BTreeMap<String, Question>,
        ) -> Result<Decision, DecisionError> {
            let mut held = Stub(vec![0.9; asked.len()]).decide(state, asked).await?;
            held.answers.insert(
                "unrequested".into(),
                Answer {
                    noul: Some(0.9),
                    ..Default::default()
                },
            );
            Ok(held)
        }
    }
    assert_eq!(
        judge(
            &Extra,
            &serde_json::json!({}),
            "query",
            "skill",
            &candidates(),
            std::future::pending()
        )
        .await,
        Err(Refused::InvalidOutput)
    );
}

#[test]
fn clef_factory_is_local_lazy_and_never_substitutes_a_remote_backend() {
    use crate::{
        config::{DecisionBackend, DecisionConfig},
        managed_decision::ManagedDecisionError,
    };
    struct Local;
    #[async_trait::async_trait]
    impl Decider for Local {
        fn name(&self) -> &str {
            "clef"
        }
        async fn decide(
            &self,
            _: &serde_json::Value,
            _: &BTreeMap<String, Question>,
        ) -> Result<Decision, DecisionError> {
            panic!("factory construction must not run inference")
        }
    }
    let configured = DecisionConfig {
        enabled: true,
        backend: DecisionBackend::Clef,
        model: Some("flash-cpu-v1".into()),
        ..Default::default()
    };
    let unused_key = || panic!("a local selection read a remote key");
    let local = build_with_sources(&configured, DecisionUse::Routing, unused_key, |id| {
        assert_eq!(id, "flash-cpu-v1");
        Ok(std::sync::Arc::new(Local))
    })
    .unwrap()
    .unwrap();
    assert_eq!(local.name(), "clef");
    assert!(matches!(
        build_with_sources(&configured, DecisionUse::Routing, unused_key, |_| Err(
            ManagedDecisionError::UnverifiedProfile
        )),
        Err(DecisionBuildError::Clef(
            ManagedDecisionError::UnverifiedProfile
        ))
    ));
    assert!(
        build_with_sources(
            &configured,
            DecisionUse::Finalization,
            unused_key,
            |_| panic!("unsupported finalizer read a generation")
        )
        .unwrap()
        .is_none()
    );
    let disabled = DecisionConfig {
        enabled: false,
        ..configured.clone()
    };
    assert!(
        build_with_sources(&disabled, DecisionUse::Routing, unused_key, |_| panic!(
            "disabled decider read a generation"
        ))
        .unwrap()
        .is_none()
    );
    let invalid = DecisionConfig {
        helper: Some("/tmp/arbitrary".into()),
        ..configured.clone()
    };
    assert!(matches!(
        build_with_sources(&invalid, DecisionUse::Routing, unused_key, |_| panic!(
            "invalid settings read a generation"
        )),
        Err(DecisionBuildError::InvalidClefSettings)
    ));
    let unverified = build_with_key(&configured, DecisionUse::Routing, unused_key);
    #[cfg(target_os = "macos")]
    assert!(matches!(
        unverified,
        Err(DecisionBuildError::Clef(
            ManagedDecisionError::UnverifiedProfile
        ))
    ));
    #[cfg(not(target_os = "macos"))]
    assert!(matches!(
        unverified,
        Err(DecisionBuildError::Clef(
            ManagedDecisionError::UnsupportedPlatform
        ))
    ));
}
