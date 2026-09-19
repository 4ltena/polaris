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
        self.reply.clone().ok_or(DecisionError::Unavailable)
    }
}

fn answered(pairs: &[(&str, &str)]) -> Decision {
    Decision {
        answers: pairs
            .iter()
            .map(|(key, choice)| {
                (
                    (*key).to_owned(),
                    Answer {
                        choice: Some((*choice).to_owned()),
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
async fn one_request_carries_every_task_as_its_own_choice() {
    let heavy = typed_agent("heavy", "supergiant");
    let stub = Stub {
        reply: Some(answered(&[("t0", "dwarf"), ("t1", "subgiant")])),
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
    // Every weight is on offer in every question. Leaving one out would
    // make it unreachable no matter what the task looks like.
    for question in seen[0].values() {
        let Question::Choice { criteria, .. } = question else {
            panic!("weights are exclusive, so the question is a Choice");
        };
        assert_eq!(criteria.len(), 4);
        for tier in Tier::ALL {
            assert!(criteria.contains_key(tier.name()), "{tier} is not offered");
        }
    }
}

#[tokio::test]
async fn a_service_that_cannot_answer_leaves_every_declaration_standing() {
    let light = typed_agent("light", "dwarf");
    let heavy = typed_agent("heavy", "supergiant");
    let decider = TypedTierDecider::new(Stub::default());
    assert_eq!(
        decider.decide(&[(&light, "a"), (&heavy, "b")]).await,
        vec![Tier::Dwarf, Tier::Supergiant],
        "a delegation is not lost because the service that prices it is down"
    );
}

#[tokio::test]
async fn an_answer_that_names_nothing_on_offer_falls_back_for_that_task_alone() {
    let heavy = typed_agent("heavy", "supergiant");
    let decider = TypedTierDecider::new(Stub {
        // One usable answer, one naming an option nobody offered, and one
        // missing entirely.
        reply: Some(answered(&[("t0", "dwarf"), ("t1", "featherweight")])),
        ..Default::default()
    });
    assert_eq!(
        decider
            .decide(&[(&heavy, "a"), (&heavy, "b"), (&heavy, "c")])
            .await,
        vec![Tier::Dwarf, Tier::Supergiant, Tier::Supergiant],
        "one unreadable answer does not cost the others their decision"
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
