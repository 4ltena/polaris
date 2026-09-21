//! A local stub stands in for the service. No real endpoint is contacted.
use super::*;
use polaris_provider::decision::{Answer, Decision, DecisionError};
use std::path::Path;
use std::sync::Mutex;

fn typed_agent(name: &str, tier: &str) -> AgentType {
    let text = format!(
        "---\nname: {name}\ndescription: 説明。\nallowed-tools: read\nmetadata:\n  polaris-access: read\n  polaris-tier: {tier}\n  polaris-wall-seconds: \"60\"\n  polaris-max-turns: \"4\"\n  polaris-continuation: \"denied\"\n  polaris-output: r.json\n---\n本文。\n"
    );
    polaris_skills::agent_type::parse(&text, name, Path::new("agents").join(name).as_path())
        .expect("the fixture parses")
}

#[derive(Default)]
struct Stub {
    reply: Option<Decision>,
    error: Option<DecisionError>,
    seen: Mutex<Vec<BTreeMap<String, Question>>>,
}

#[async_trait::async_trait]
impl Decider for Stub {
    async fn decide(
        &self,
        _: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        self.seen.lock().unwrap().push(questions.clone());
        if let Some(error) = &self.error {
            return Err(error.clone());
        }
        self.reply.clone().ok_or(DecisionError::Unavailable)
    }
}

/// `None` stands for an answer the service returned without a score.
fn answered(pairs: &[(&str, Option<f64>)]) -> Decision {
    Decision {
        answers: pairs
            .iter()
            .map(|(key, score)| {
                (
                    (*key).to_owned(),
                    Answer {
                        score: *score,
                        ..Default::default()
                    },
                )
            })
            .collect(),
        input_tokens: Some(200),
        output_tokens: Some(0),
    }
}

#[tokio::test]
async fn one_request_carries_every_task_as_its_own_score() {
    let heavy = typed_agent("heavy", "supergiant");
    let stub = Stub {
        reply: Some(answered(&[("t0", Some(0.2)), ("t1", Some(1.4))])),
        ..Default::default()
    };
    let decider = TypedTierDecider::new(stub);
    let answers = decider
        .decide(&[(&heavy, "ファイルを一つ読む"), (&heavy, "依存を更新する")])
        .await;
    assert_eq!(answers, vec![Tier::Dwarf, Tier::Subgiant]);

    let seen = decider.decider.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "a wave costs one round trip, not one each");
    assert_eq!(seen[0].len(), 2);
    // Every weight is on the scale in every question, in the order the
    // thresholds read it. Leaving one out would move every rung above it.
    for question in seen[0].values() {
        let Question::Score { criteria, .. } = question else {
            panic!("the weights are a scale, so the question is a Score");
        };
        assert_eq!(criteria, &levels());
        assert_eq!(criteria.len(), Tier::ALL.len());
    }
}

/// The thresholds are the whole point of scoring rather than choosing, so
/// they are pinned rather than left to the reader of `RUNGS`.
#[test]
fn a_task_buys_a_heavier_model_only_once_it_looks_clearly_heavier() {
    for (score, expected) in [
        (0.0, Tier::Dwarf),
        (0.74, Tier::Dwarf),
        (0.75, Tier::Subgiant),
        (1.74, Tier::Subgiant),
        (1.75, Tier::Giant),
        (2.74, Tier::Giant),
        (2.75, Tier::Supergiant),
        (3.0, Tier::Supergiant),
    ] {
        assert_eq!(from_score(score), Some(expected), "score {score}");
    }
    // Plain rounding would have put each of these a rung higher.
    assert_eq!(from_score(0.6), Some(Tier::Dwarf));
    assert_eq!(from_score(1.6), Some(Tier::Subgiant));
    assert_eq!(from_score(2.6), Some(Tier::Giant));
    // Nothing usable comes back as no answer, not as the lightest rung.
    assert_eq!(from_score(-0.1), None);
    assert_eq!(from_score(f64::NAN), None);
}

#[tokio::test]
async fn a_service_that_cannot_answer_uses_the_default_bounded_by_each_ceiling() {
    let light = typed_agent("light", "dwarf");
    let heavy = typed_agent("heavy", "supergiant");
    for error in [
        DecisionError::Auth,
        DecisionError::Unavailable,
        DecisionError::Malformed,
        DecisionError::InvalidAnswer,
    ] {
        let decider = TypedTierDecider::new(Stub {
            error: Some(error),
            ..Default::default()
        });
        assert_eq!(
            decider.decide(&[(&light, "a"), (&heavy, "b")]).await,
            vec![Tier::Dwarf, Tier::DEFAULT],
            "a failed judge leaves work unrouted instead of running its ceiling"
        );
    }
}

#[tokio::test]
async fn malformed_or_missing_answers_use_the_default_for_that_task_alone() {
    let heavy = typed_agent("heavy", "supergiant");
    let decider = TypedTierDecider::new(Stub {
        // One usable answer, one naming an option nobody offered, and one
        // missing entirely.
        reply: Some(answered(&[("t0", Some(0.1)), ("t1", None)])),
        ..Default::default()
    });
    assert_eq!(
        decider
            .decide(&[(&heavy, "a"), (&heavy, "b"), (&heavy, "c")])
            .await,
        vec![Tier::Dwarf, Tier::DEFAULT, Tier::DEFAULT],
        "one unreadable answer does not cost the others their decision"
    );
}

#[tokio::test]
async fn every_rung_on_the_scale_still_refines_a_high_ceiling() {
    let heavy = typed_agent("heavy", "supergiant");
    let decider = TypedTierDecider::new(Stub {
        reply: Some(answered(&[
            ("t0", Some(0.0)),
            ("t1", Some(1.3)),
            ("t2", Some(2.3)),
            ("t3", Some(3.0)),
        ])),
        ..Default::default()
    });
    assert_eq!(
        decider
            .decide(&[(&heavy, "a"), (&heavy, "b"), (&heavy, "c"), (&heavy, "d"),])
            .await,
        vec![Tier::Dwarf, Tier::Subgiant, Tier::Giant, Tier::Supergiant]
    );
}

#[tokio::test]
async fn an_over_limit_wave_uses_the_default_without_calling_the_judge() {
    let heavy = typed_agent("heavy", "supergiant");
    let decider = TypedTierDecider::new(Stub::default());
    let oversized = "a ".repeat(32_001);
    assert_eq!(
        decider.decide(&[(&heavy, &oversized)]).await,
        vec![Tier::DEFAULT]
    );
    assert!(
        decider.decider.seen.lock().unwrap().is_empty(),
        "a request over the judge limit must be refused before the judge runs"
    );
}

#[tokio::test]
async fn nothing_to_price_asks_nothing() {
    let decider = TypedTierDecider::new(Stub::default());
    assert!(decider.decide(&[]).await.is_empty());
    assert!(
        decider.decider.seen.lock().unwrap().is_empty(),
        "an empty wave must not spend a request"
    );
}
