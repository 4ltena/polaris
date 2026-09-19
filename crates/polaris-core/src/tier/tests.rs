//! The ceiling, and what happens when a decider misbehaves.
use super::*;
use std::path::Path;

fn typed(name: &str, tier: &str) -> AgentType {
    let text = format!(
        "---\nname: {name}\ndescription: 説明。\nallowed-tools: read\nmetadata:\n  polaris-access: read\n  polaris-tier: {tier}\n  polaris-wall-seconds: \"60\"\n  polaris-max-turns: \"4\"\n  polaris-continuation: \"denied\"\n  polaris-output: r.json\n---\n本文。\n"
    );
    polaris_skills::agent_type::parse(&text, name, Path::new("agents").join(name).as_path())
        .expect("the fixture parses")
}

struct Fixed(Vec<Tier>);
#[async_trait::async_trait]
impl TierDecider for Fixed {
    async fn decide(&self, _: &[(&AgentType, &str)]) -> Vec<Tier> {
        self.0.clone()
    }
}

#[tokio::test]
async fn no_decider_uses_what_each_type_declared() {
    let light = typed("light", "dwarf");
    let heavy = typed("heavy", "supergiant");
    let tasks = [(&light, "a"), (&heavy, "b")];
    assert_eq!(
        decide_all(None, &tasks).await,
        vec![Tier::Dwarf, Tier::Supergiant],
        "having no decider is the ordinary case, not a degraded one"
    );
}

#[tokio::test]
async fn a_decision_may_lower_a_weight_but_never_raise_it() {
    let light = typed("light", "dwarf");
    let heavy = typed("heavy", "supergiant");
    let tasks = [(&light, "a"), (&heavy, "b")];
    // The decider asks for the opposite of each declaration: heavier for
    // the type declared light, lighter for the one declared heavy.
    let decider = Fixed(vec![Tier::Supergiant, Tier::Dwarf]);
    assert_eq!(
        decide_all(Some(&decider), &tasks).await,
        vec![Tier::Dwarf, Tier::Dwarf],
        "the raise is clamped to the declaration; the lowering is kept"
    );
}

#[tokio::test]
async fn answers_that_do_not_line_up_with_the_tasks_are_discarded_whole() {
    let light = typed("light", "dwarf");
    let heavy = typed("heavy", "supergiant");
    let tasks = [(&light, "a"), (&heavy, "b")];
    // Two tasks, one answer. Pairing them by position would silently give
    // task b task a's weight, so none of it is used.
    let decider = Fixed(vec![Tier::Dwarf]);
    assert_eq!(
        decide_all(Some(&decider), &tasks).await,
        vec![Tier::Dwarf, Tier::Supergiant]
    );
}

#[tokio::test]
async fn a_pinned_weight_is_what_gets_asked_for_and_roles_still_come_first() {
    use polaris_provider::Provider;
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Named(&'static str, AtomicUsize);
    #[async_trait::async_trait]
    impl Provider for Named {
        async fn complete(
            &self,
            _: polaris_provider::CompletionRequest,
        ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
            self.1.fetch_add(1, Ordering::SeqCst);
            Ok(polaris_provider::CompletionResponse {
                text: self.0.into(),
                ..Default::default()
            })
        }
    }
    let named = |name: &'static str| Arc::new(Named(name, AtomicUsize::new(0)));
    let low = named("dwarf");
    let medium = named("subgiant");
    let bound = named("bound");
    let router: Arc<dyn Provider> = Arc::new(
        polaris_provider::role::RoleProvider::new(
            named("parent"),
            HashMap::from([("reviewer".into(), bound.clone() as Arc<dyn Provider>)]),
        )
        .with_tiers(HashMap::from([
            ("dwarf".into(), low.clone() as Arc<dyn Provider>),
            ("subgiant".into(), medium.clone() as Arc<dyn Provider>),
        ])),
    );

    // Declared medium, decided low: asking for anything returns low's.
    let pinned = PinnedTier::wrap(router.clone(), Tier::Subgiant, Tier::Dwarf);
    let resolved = pinned.resolve_tier("subgiant").unwrap().unwrap();
    assert_eq!(resolved.complete(request()).await.unwrap().text, "dwarf");
    assert_eq!(medium.1.load(Ordering::SeqCst), 0);

    // A role is more specific than a weight, so it still answers first.
    assert!(pinned.resolve_role("reviewer").unwrap().is_some());
    assert!(pinned.resolve_role("missing").is_err());

    // Deciding what was already declared wraps nothing: there is no weight
    // to pin, and an extra layer would only be one more thing to forward.
    let same = PinnedTier::wrap(router.clone(), Tier::Dwarf, Tier::Dwarf);
    assert!(Arc::ptr_eq(&same, &router));
}

fn request() -> polaris_provider::CompletionRequest {
    polaris_provider::CompletionRequest {
        system: String::new(),
        messages: vec![],
        tools: vec![],
    }
}
