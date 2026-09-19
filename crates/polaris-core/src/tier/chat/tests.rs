//! A scripted provider stands in for the model. Nothing is sent.
use super::*;
use polaris_provider::{CompletionResponse, ProviderError};
use std::path::Path;
use std::sync::Mutex;

fn typed_agent(name: &str, tier: &str) -> AgentType {
    let text = format!(
        "---\nname: {name}\ndescription: 説明。\nallowed-tools: read\nmetadata:\n  polaris-access: read\n  polaris-tier: {tier}\n  polaris-wall-seconds: \"60\"\n  polaris-max-turns: \"4\"\n  polaris-continuation: \"denied\"\n  polaris-output: r.json\n---\n本文。\n"
    );
    polaris_skills::agent_type::parse(&text, name, Path::new("agents").join(name).as_path())
        .expect("the fixture parses")
}

struct Scripted {
    reply: Option<&'static str>,
    seen: Mutex<Vec<CompletionRequest>>,
}

#[async_trait::async_trait]
impl Provider for Scripted {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.seen.lock().unwrap().push(request);
        match self.reply {
            Some(text) => Ok(CompletionResponse {
                text: text.into(),
                ..Default::default()
            }),
            None => Err(ProviderError::Http("fixture".into())),
        }
    }
}

fn scripted(reply: Option<&'static str>) -> Arc<Scripted> {
    Arc::new(Scripted {
        reply,
        seen: Mutex::new(Vec::new()),
    })
}

#[tokio::test]
async fn one_weight_per_line_is_read_back_in_order() {
    let heavy = typed_agent("heavy", "supergiant");
    let provider = scripted(Some("dwarf\nsubgiant\nsupergiant"));
    let decider = ChatTierDecider::new(provider.clone());
    assert_eq!(
        decider
            .decide(&[(&heavy, "a"), (&heavy, "b"), (&heavy, "c")])
            .await,
        vec![Tier::Dwarf, Tier::Subgiant, Tier::Supergiant]
    );
    let seen = provider.seen.lock().unwrap();
    assert_eq!(seen.len(), 1, "a wave costs one request");
    assert!(
        seen[0].tools.is_empty(),
        "the decision is an answer, not something that may act"
    );
}

#[tokio::test]
async fn nothing_but_a_weight_per_line_counts_as_an_answer() {
    let light = typed_agent("light", "dwarf");
    let heavy = typed_agent("heavy", "supergiant");
    let declared = vec![Tier::Dwarf, Tier::Supergiant];
    // The control: the right shape does change the outcome, so the
    // rejections below are rejecting something.
    let decider = ChatTierDecider::new(scripted(Some("dwarf\nsubgiant")));
    assert_eq!(
        decider.decide(&[(&light, "a"), (&heavy, "b")]).await,
        vec![Tier::Dwarf, Tier::Subgiant]
    );
    for reply in [
        // The words are there, wrapped in a sentence. Reading a decision
        // out of an explanation would accept whatever was mentioned.
        "この依頼は ultra です。\nもう一つは low でしょう。",
        // Fewer lines than tasks, more lines than tasks, and nothing at
        // all: none of them lines up, so none of it is used.
        "low",
        "low\nmedium\nultra",
        "",
    ] {
        let decider = ChatTierDecider::new(scripted(Some(reply)));
        assert_eq!(
            decider.decide(&[(&light, "a"), (&heavy, "b")]).await,
            declared,
            "{reply:?}"
        );
    }
}

#[tokio::test]
async fn a_model_that_cannot_answer_costs_no_delegation() {
    let heavy = typed_agent("heavy", "supergiant");
    let decider = ChatTierDecider::new(scripted(None));
    assert_eq!(
        decider.decide(&[(&heavy, "a")]).await,
        vec![Tier::Supergiant]
    );
}

#[tokio::test]
async fn nothing_to_price_asks_nothing() {
    let provider = scripted(Some("dwarf"));
    let decider = ChatTierDecider::new(provider.clone());
    assert!(decider.decide(&[]).await.is_empty());
    assert!(provider.seen.lock().unwrap().is_empty());
}
