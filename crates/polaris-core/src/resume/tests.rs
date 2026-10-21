//! Fixed observations only.
use super::*;

fn after(previous: RunState) -> Observed {
    Observed {
        goal_active: true,
        goal_stopped: false,
        previous: Some(previous),
        awaiting_approval: false,
        automatic_used: 0,
        goal_achieved: None,
    }
}

#[test]
fn only_an_unfinished_goal_after_a_settled_failure_continues_by_itself() {
    assert_eq!(decide(&after(RunState::Failed)), Decision::Continue);
    assert_eq!(decide(&after(RunState::Interrupted)), Decision::Continue);
    for (previous, reason) in [
        (RunState::Succeeded, Refusal::Completed),
        (RunState::Cancelled, Refusal::Stopped),
        (RunState::OutcomeUnknown, Refusal::Unsettled),
        (RunState::Running, Refusal::Unsettled),
        (RunState::AwaitingApproval, Refusal::Unsettled),
        (RunState::Cancelling, Refusal::Unsettled),
    ] {
        assert_eq!(
            decide(&after(previous)),
            Decision::Refuse { reason },
            "{previous:?}"
        );
    }
}

#[test]
fn a_stop_a_person_or_a_spent_budget_all_refuse() {
    for (observed, reason) in [
        (
            Observed {
                goal_active: false,
                ..after(RunState::Failed)
            },
            Refusal::NoGoal,
        ),
        (
            Observed {
                goal_stopped: true,
                ..after(RunState::Failed)
            },
            Refusal::Stopped,
        ),
        (
            Observed {
                awaiting_approval: true,
                ..after(RunState::Failed)
            },
            Refusal::AwaitingPerson,
        ),
        (
            Observed {
                automatic_used: MAX_AUTOMATIC,
                ..after(RunState::Failed)
            },
            Refusal::LimitReached,
        ),
        (
            Observed {
                previous: None,
                ..after(RunState::Failed)
            },
            Refusal::Unsettled,
        ),
    ] {
        assert_eq!(decide(&observed), Decision::Refuse { reason });
    }
    // A stop outranks the budget: it is not a counting question.
    assert_eq!(
        decide(&Observed {
            goal_stopped: true,
            automatic_used: 0,
            ..after(RunState::Failed)
        }),
        Decision::Refuse {
            reason: Refusal::Stopped
        }
    );
}

#[test]
fn a_run_that_succeeded_continues_only_when_the_goal_is_judged_unfinished() {
    // A run can finish exactly as asked and still be one step of a larger
    // goal. Without a judgement nothing says so, and the run is left
    // alone — which is what every run did before the judge existed.
    let mut observed = after(RunState::Succeeded);
    assert_eq!(
        decide(&observed),
        Decision::Refuse {
            reason: Refusal::Completed
        }
    );
    observed.goal_achieved = Some(true);
    assert_eq!(
        decide(&observed),
        Decision::Refuse {
            reason: Refusal::Completed
        },
        "judged done is still done"
    );
    observed.goal_achieved = Some(false);
    assert_eq!(decide(&observed), Decision::Continue);

    // The judgement does not buy extra continuations: the budget still
    // caps how many a goal gets without being asked.
    observed.automatic_used = MAX_AUTOMATIC;
    assert_eq!(
        decide(&observed),
        Decision::Refuse {
            reason: Refusal::LimitReached
        }
    );
}

#[test]
fn a_judgement_never_overrides_a_stop_or_an_unsettled_result() {
    // Everything that refuses before the run state is consulted keeps
    // refusing: a judge that says "not finished" must not restart work the
    // person stopped, or work whose result nobody knows.
    for (adjust, reason) in [
        (
            Box::new(|held: &mut Observed| held.goal_stopped = true) as Box<dyn Fn(&mut Observed)>,
            Refusal::Stopped,
        ),
        (
            Box::new(|held: &mut Observed| held.awaiting_approval = true),
            Refusal::AwaitingPerson,
        ),
        (
            Box::new(|held: &mut Observed| held.goal_active = false),
            Refusal::NoGoal,
        ),
        (
            Box::new(|held: &mut Observed| held.previous = Some(RunState::OutcomeUnknown)),
            Refusal::Unsettled,
        ),
    ] {
        let mut observed = after(RunState::Succeeded);
        observed.goal_achieved = Some(false);
        adjust(&mut observed);
        assert_eq!(decide(&observed), Decision::Refuse { reason });
    }
}

mod achieved_tests {
    use super::super::achieved::{MAX_EVIDENCE_BYTES, judge};
    use polaris_provider::decision::{Answer, Decider, Decision as Typed, DecisionError, Question};
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    struct Stub {
        yes: Option<f64>,
        seen: Mutex<Vec<String>>,
    }
    impl Stub {
        fn new(yes: Option<f64>) -> Self {
            Self {
                yes,
                seen: Mutex::new(Vec::new()),
            }
        }
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
        ) -> Result<Typed, DecisionError> {
            for question in questions.values() {
                if let Question::Noul { instructions } = question {
                    self.seen.lock().unwrap().push(instructions.clone());
                }
            }
            let Some(yes) = self.yes else {
                return Err(DecisionError::Unavailable);
            };
            Ok(Typed {
                answers: questions
                    .keys()
                    .map(|key| {
                        (
                            key.clone(),
                            Answer {
                                noul: Some(yes),
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

    #[tokio::test]
    async fn unmeasured_probabilities_never_decide_goal_completion() {
        struct Unknown;
        #[async_trait::async_trait]
        impl Decider for Unknown {
            async fn decide(
                &self,
                _: &serde_json::Value,
                _: &BTreeMap<String, Question>,
            ) -> Result<Typed, DecisionError> {
                panic!("unmeasured backend must not inherit goal-completion thresholds")
            }
        }
        assert_eq!(
            judge(&Unknown, "goal", "evidence", std::future::pending()).await,
            None
        );
    }

    async fn asked(yes: Option<f64>) -> Option<bool> {
        judge(
            &Stub::new(yes),
            "全部終わらせる",
            "終わった",
            std::future::pending(),
        )
        .await
    }

    struct Unknown(std::sync::Arc<Stub>);
    #[async_trait::async_trait]
    impl Decider for Unknown {
        async fn decide(
            &self,
            state: &serde_json::Value,
            questions: &BTreeMap<String, Question>,
        ) -> Result<Typed, DecisionError> {
            self.0.decide(state, questions).await
        }
    }

    #[tokio::test]
    async fn fallback_unmeasured_answers_never_inherit_goal_completion_thresholds() {
        use polaris_provider::decision::fallback::{FallbackDecider, FallbackPolicy};
        for probability in [0.99, 0.01] {
            let primary = std::sync::Arc::new(Stub::new(None));
            let fallback = std::sync::Arc::new(Stub::new(Some(probability)));
            let chain = FallbackDecider::new(
                vec![
                    primary.clone(),
                    std::sync::Arc::new(Unknown(fallback.clone())),
                ],
                FallbackPolicy {
                    max_attempts: 2,
                    attempt_timeout: std::time::Duration::from_secs(1),
                    total_timeout: std::time::Duration::from_secs(2),
                },
            )
            .unwrap();
            assert_eq!(
                judge(&chain, "goal", "evidence", std::future::pending()).await,
                None
            );
            assert_eq!(primary.seen.lock().unwrap().len(), 1);
            assert_eq!(fallback.seen.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn fallback_calibrated_answers_remain_reachable_from_unmeasured_primary() {
        use polaris_provider::decision::fallback::{FallbackDecider, FallbackPolicy};
        for (probability, expected) in [(0.99, true), (0.01, false)] {
            let primary = std::sync::Arc::new(Stub::new(None));
            let fallback = std::sync::Arc::new(Stub::new(Some(probability)));
            let chain = FallbackDecider::new(
                vec![
                    std::sync::Arc::new(Unknown(primary.clone())),
                    fallback.clone(),
                ],
                FallbackPolicy {
                    max_attempts: 2,
                    attempt_timeout: std::time::Duration::from_secs(1),
                    total_timeout: std::time::Duration::from_secs(2),
                },
            )
            .unwrap();
            assert_eq!(
                judge(&chain, "goal", "evidence", std::future::pending()).await,
                Some(expected)
            );
            assert_eq!(primary.seen.lock().unwrap().len(), 1);
            assert_eq!(fallback.seen.lock().unwrap().len(), 1);
        }
    }

    #[tokio::test]
    async fn only_a_confident_answer_is_a_judgement() {
        assert_eq!(asked(Some(0.95)).await, Some(true));
        assert_eq!(asked(Some(0.1)).await, Some(false));
        // Between the edges the answer is not a decision. Continuing or
        // stopping on a coin flip is worse than leaving the run alone.
        assert_eq!(asked(Some(0.6)).await, None);
        // And a service that could not answer is not a verdict either.
        assert_eq!(asked(None).await, None);
    }

    #[tokio::test]
    async fn nothing_is_asked_without_a_goal_to_ask_about() {
        let stub = Stub::new(Some(0.95));
        assert_eq!(
            judge(&stub, "   ", "終わった", std::future::pending()).await,
            None
        );
        assert!(
            stub.seen.lock().unwrap().is_empty(),
            "an empty goal must not spend a request"
        );
    }

    #[tokio::test]
    async fn long_evidence_cannot_turn_one_question_into_a_large_request() {
        let stub = Stub::new(Some(0.95));
        let evidence = "あ".repeat(MAX_EVIDENCE_BYTES);
        judge(&stub, "目標", &evidence, std::future::pending()).await;
        let seen = stub.seen.lock().unwrap();
        assert!(
            seen[0].len() < evidence.len(),
            "the evidence is bounded here, not at the call site"
        );
        // Cut on a character boundary: a half-written character would make
        // the question unreadable rather than merely shorter.
        assert!(seen[0].ends_with('あ'));
    }
}
