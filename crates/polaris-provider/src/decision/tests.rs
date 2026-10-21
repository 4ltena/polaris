//! Types and checking only; nothing is sent.
//!
//! The shapes here are the ones the service accepts and returns, confirmed
//! against it on 2026-09-17: questions carry `instructions`, a choice names
//! its options in `criteria`, and an answer comes back keyed by those names
//! rather than by position.
use super::*;
use std::collections::BTreeMap;

fn questions(held: &[(&str, Question)]) -> BTreeMap<String, Question> {
    held.iter()
        .map(|(id, question)| ((*id).to_owned(), question.clone()))
        .collect()
}

fn named(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(name, description)| ((*name).to_owned(), (*description).to_owned()))
        .collect()
}

fn weights(pairs: &[(&str, f64)]) -> BTreeMap<String, f64> {
    pairs
        .iter()
        .map(|(name, value)| ((*name).to_owned(), *value))
        .collect()
}

fn choice() -> Question {
    Question::Choice {
        instructions: "どれが合うか".into(),
        criteria: named(&[("a", "ひとつめ"), ("b", "ふたつめ"), ("c", "みっつめ")]),
    }
}

fn noul() -> Question {
    Question::Noul {
        instructions: "これは当てはまるか".into(),
    }
}

fn score() -> Question {
    Question::Score {
        instructions: "どのくらい急ぐか".into(),
        criteria: vec!["低い".into(), "普通".into(), "高い".into()],
    }
}

fn decision(answers: &[(&str, Answer)]) -> Decision {
    Decision {
        answers: answers
            .iter()
            .map(|(id, answer)| ((*id).to_owned(), answer.clone()))
            .collect(),
        input_tokens: Some(100),
        output_tokens: Some(20),
    }
}

fn picked(name: &str) -> Answer {
    Answer {
        choice: Some(name.into()),
        probabilities: weights(&[("a", 0.2), ("b", 0.7), ("c", 0.1)]),
        confidence: Some(0.7),
        ..Default::default()
    }
}

fn yes(value: f64) -> Answer {
    Answer {
        noul: Some(value),
        ..Default::default()
    }
}

#[test]
fn an_answer_is_read_against_the_question_it_belongs_to() {
    assert!(picked("b").answers(&choice()));
    // A yes/no answer is not a choice, however well formed it looks.
    assert!(!picked("b").answers(&noul()));
    assert!(yes(0.42).answers(&noul()));
    assert!(!yes(0.42).answers(&choice()));
}

#[test]
fn conflicting_answer_types_and_nonfinite_values_are_refused() {
    let rated = Answer {
        score: Some(0.74),
        probabilities: weights(&[("0", 0.26), ("1", 0.74), ("2", 0.0)]),
        ..Default::default()
    };
    assert!(rated.answers(&score()));
    for (question, answer) in [
        (
            choice(),
            Answer {
                noul: Some(0.2),
                ..picked("b")
            },
        ),
        (
            choice(),
            Answer {
                score: Some(0.2),
                ..picked("b")
            },
        ),
        (
            score(),
            Answer {
                noul: Some(0.2),
                ..rated.clone()
            },
        ),
        (
            score(),
            Answer {
                choice: Some("0".into()),
                ..rated.clone()
            },
        ),
        (
            noul(),
            Answer {
                probabilities: weights(&[("yes", 1.0)]),
                ..yes(0.2)
            },
        ),
    ] {
        assert!(!answer.answers(&question), "{answer:?}");
    }
    for value in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        assert!(!yes(value).answers(&noul()));
        assert!(
            !Answer {
                score: Some(value),
                ..rated.clone()
            }
            .answers(&score())
        );
        assert!(
            !Answer {
                confidence: Some(value),
                ..picked("b")
            }
            .answers(&choice())
        );
        assert!(
            !Answer {
                probabilities: weights(&[("a", value), ("b", 0.7), ("c", 0.3)]),
                ..picked("b")
            }
            .answers(&choice())
        );
    }
}

#[test]
fn a_choice_answers_only_with_an_option_that_was_offered() {
    // A name nobody offered. Position-free answers make this the shape the
    // check has to catch: there is no index to fall outside a range.
    assert!(!picked("d").answers(&choice()));
    // Every offered name carries a probability, and no others.
    for broken in [
        Answer {
            probabilities: weights(&[("a", 0.5), ("b", 0.5)]),
            ..picked("a")
        },
        Answer {
            probabilities: weights(&[("a", 0.2), ("b", 0.7), ("d", 0.1)]),
            ..picked("a")
        },
    ] {
        assert!(!broken.answers(&choice()), "{broken:?}");
    }
}

#[test]
fn a_probability_outside_its_range_or_set_is_refused() {
    for broken in [
        // Probabilities that describe more than one outcome.
        Answer {
            probabilities: weights(&[("a", 0.9), ("b", 0.9), ("c", 0.9)]),
            ..picked("a")
        },
        // Outside 0 to 1.
        Answer {
            probabilities: weights(&[("a", 1.4), ("b", -0.3), ("c", -0.1)]),
            ..picked("a")
        },
        // A confidence that is not a probability.
        Answer {
            confidence: Some(1.5),
            ..picked("a")
        },
    ] {
        assert!(!broken.answers(&choice()), "{broken:?}");
    }
    assert!(!yes(1.2).answers(&noul()));
}

#[test]
fn a_score_is_read_on_the_scale_it_was_asked_on() {
    let good = Answer {
        score: Some(0.74),
        probabilities: weights(&[("0", 0.26), ("1", 0.74), ("2", 0.0)]),
        confidence: Some(0.61),
        ..Default::default()
    };
    assert!(good.answers(&score()));

    // Three levels answer between 0 and 2; anything else is not on the
    // scale that was offered.
    for value in [-0.1, 2.1] {
        assert!(
            !Answer {
                score: Some(value),
                ..good.clone()
            }
            .answers(&score()),
            "{value}"
        );
    }
    // Levels come back keyed by their index, not by a label.
    assert!(
        !Answer {
            probabilities: weights(&[("低い", 0.26), ("普通", 0.74), ("高い", 0.0)]),
            ..good
        }
        .answers(&score())
    );
}

#[test]
fn every_question_is_answered_and_nothing_else_is() {
    let asked = questions(&[("pick", choice()), ("fits", noul())]);
    let good = decision(&[("pick", picked("c")), ("fits", yes(0.9))]);
    assert_eq!(checked(&good, &asked), Ok(()));

    // One question left out.
    let short = decision(&[("fits", yes(0.9))]);
    assert_eq!(checked(&short, &asked), Err(DecisionError::InvalidAnswer));

    // Every question answered, plus one more nobody asked for. The reply
    // covers the questions, so only counting them catches this.
    let surplus = decision(&[
        ("pick", picked("c")),
        ("fits", yes(0.9)),
        ("unasked", yes(0.1)),
    ]);
    assert_eq!(checked(&surplus, &asked), Err(DecisionError::InvalidAnswer));

    // An answer under a name that was not a question.
    let extra = decision(&[("pick", picked("c")), ("other", yes(0.9))]);
    assert_eq!(checked(&extra, &asked), Err(DecisionError::InvalidAnswer));
}

#[test]
fn a_request_too_large_to_send_is_refused_before_it_is_sent() {
    let small = questions(&[("pick", choice())]);
    assert_eq!(within_limits(100, &small), Ok(()));
    assert_eq!(
        within_limits(MAX_STATE_TOKENS + 1, &small),
        Err(DecisionError::TooLarge)
    );

    let wide = questions(&[(
        "pick",
        Question::Choice {
            instructions: "x".into(),
            criteria: (0..=MAX_OPTIONS)
                .map(|held| (format!("o{held}"), "説明".to_owned()))
                .collect(),
        },
    )]);
    assert_eq!(within_limits(1, &wide), Err(DecisionError::TooLarge));

    // Exactly at the cap is still sendable.
    let edge = questions(&[(
        "pick",
        Question::Choice {
            instructions: "x".into(),
            criteria: (0..MAX_OPTIONS)
                .map(|held| (format!("o{held}"), "説明".to_owned()))
                .collect(),
        },
    )]);
    assert_eq!(within_limits(MAX_STATE_TOKENS, &edge), Ok(()));

    // A yes/no question has no options, so it never trips the option cap.
    assert_eq!(within_limits(1, &questions(&[("q", noul())])), Ok(()));
}

#[tokio::test]
async fn shared_and_metered_judges_retain_the_backend_identity() {
    struct Named;
    #[async_trait::async_trait]
    impl Decider for Named {
        fn name(&self) -> &str {
            "synthetic-backend"
        }

        fn capabilities(&self) -> DecisionCapabilities {
            DecisionCapabilities {
                location: DecisionLocation::LocalProcess,
                max_context_tokens: Some(1024),
                ..Default::default()
            }
        }

        async fn decide(
            &self,
            _: &serde_json::Value,
            _: &std::collections::BTreeMap<String, Question>,
        ) -> Result<Decision, DecisionError> {
            Ok(Decision {
                answers: Default::default(),
                input_tokens: Some(7),
                output_tokens: Some(0),
            })
        }
    }
    let shared: std::sync::Arc<dyn Decider> = std::sync::Arc::new(Named);
    assert_eq!(shared.name(), "synthetic-backend");
    assert_eq!(shared.capabilities(), Named.capabilities());
    let meter = DecisionMeter::default();
    let metered = meter.wrap(shared);
    assert_eq!(metered.name(), "synthetic-backend");
    assert_eq!(metered.capabilities(), Named.capabilities());
    let nested = std::sync::Arc::new(metered);
    assert_eq!(nested.name(), "synthetic-backend");
    assert_eq!(nested.capabilities(), Named.capabilities());
    nested
        .decide(&serde_json::Value::Null, &Default::default())
        .await
        .unwrap();
    assert_eq!(meter.snapshot().input_tokens, 7);
    assert_eq!(meter.snapshot().reported, 1);
}

/// The judge bills separately from the models and at a different rate, and
/// its consumption was counted nowhere: the reply carries a usage figure,
/// `Decision` carries it forward, and no caller read it. A run with the judge
/// on reported its cost with the judge's part missing.
#[tokio::test]
async fn the_judges_own_consumption_is_counted() {
    struct Fixed(Result<Decision, DecisionError>);
    #[async_trait::async_trait]
    impl Decider for Fixed {
        async fn decide(
            &self,
            _state: &serde_json::Value,
            _questions: &std::collections::BTreeMap<String, Question>,
        ) -> Result<Decision, DecisionError> {
            self.0.clone()
        }
    }
    let answered = |input, output| {
        Ok(Decision {
            answers: Default::default(),
            input_tokens: input,
            output_tokens: output,
        })
    };
    let ask = |reply: Result<Decision, DecisionError>, meter: &DecisionMeter| {
        let held = meter.wrap(std::sync::Arc::new(Fixed(reply)));
        async move {
            let _ = held
                .decide(&serde_json::Value::Null, &Default::default())
                .await;
        }
    };

    let meter = DecisionMeter::default();
    ask(answered(Some(400), Some(20)), &meter).await;
    ask(answered(Some(600), Some(30)), &meter).await;
    // Answered, but the reply said nothing about usage. Unknown, not zero.
    ask(answered(None, None), &meter).await;
    // A partial reply contributes its known subtotal, but cannot make the
    // response complete or turn the absent output into an observed zero.
    ask(answered(Some(40), None), &meter).await;
    ask(answered(None, Some(5)), &meter).await;
    // Did not answer. The service may have worked before refusing.
    ask(Err(DecisionError::Unavailable), &meter).await;

    let held = meter.snapshot();
    assert_eq!(held.input_tokens, 1040);
    assert_eq!(held.output_tokens, 55);
    assert_eq!(held.reported, 2);
    assert_eq!(held.missing, 3, "使用量が全て揃わなかった回答");
    assert_eq!(held.failed, 1, "答えなかった分");
}

#[test]
fn unknown_capabilities_never_claim_locality_limits_or_calibration() {
    let held = DecisionCapabilities::default();
    assert!(!held.location.is_local());
    assert_eq!(held.band_calibration, BandCalibration::Unmeasured);
    assert_eq!(held.shapes, None);
    assert_eq!(held.max_state_tokens, None);
    assert_eq!(held.max_context_tokens, None);
    assert_eq!(held.max_questions, None);
    assert_eq!(held.max_options, None);
    assert_eq!(held.max_frame_bytes, None);
    assert_eq!(held.cancellation, DecisionCancellation::Unknown);
    assert_eq!(held.usage, DecisionUsageReporting::Unknown);
}
