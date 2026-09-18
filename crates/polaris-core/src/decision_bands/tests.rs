//! A local stub stands in for the service. No real endpoint is contacted.
use super::*;
use polaris_provider::decision::{Answer, Decision};

struct Stub(Vec<f64>);

#[async_trait::async_trait]
impl Decider for Stub {
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
