//! The ceiling, and what happens when a decider misbehaves.
use super::*;
use std::path::Path;

fn typed(name: &str, tier: &str) -> AgentType {
    typed_with(name, tier, None)
}

fn typed_with(name: &str, tier: &str, floor: Option<&str>) -> AgentType {
    let floor = floor.map_or(String::new(), |rung| {
        format!("  polaris-tier-min: {rung}\n")
    });
    let text = format!(
        "---\nname: {name}\ndescription: 説明。\nallowed-tools: read\nmetadata:\n  polaris-access: read\n  polaris-tier: {tier}\n{floor}  polaris-wall-seconds: \"60\"\n  polaris-max-turns: \"4\"\n  polaris-continuation: \"denied\"\n  polaris-output: r.json\n---\n本文。\n"
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

/// Nothing routed the task, so it runs at the ordinary rung — not at the
/// ceiling its type happens to allow.
///
/// This test used to assert the opposite, and that assertion was the
/// defect. `polaris-tier` is a ceiling: `implementer` declares
/// `supergiant` so the judge can reach all four rungs, not because every
/// implementation is worth `sol`. Running the ceiling unrouted put the
/// ordinary case on 33.7 against `subgiant`'s 10.0, in exactly the
/// configuration that has no judge to notice it.
#[tokio::test]
async fn nothing_routed_it_means_the_ordinary_rung_not_the_ceiling() {
    let light = typed("light", "dwarf");
    let heavy = typed("heavy", "supergiant");
    let middle = typed("middle", "giant");
    let tasks = [(&light, "a"), (&heavy, "b"), (&middle, "c")];
    assert_eq!(
        decide_all(None, &tasks).await,
        // `dwarf` keeps its own: the ceiling still binds downward, so a
        // type that allows less than the default is not handed more.
        vec![Tier::Dwarf, Tier::DEFAULT, Tier::DEFAULT],
        "unrouted work runs at the documented default, capped by the ceiling"
    );
    assert_eq!(
        Tier::DEFAULT,
        Tier::Subgiant,
        "M16 names subgiant the default"
    );
    assert!(
        Tier::DEFAULT < Tier::Supergiant,
        "a default at or above the top rung would make this rule a no-op"
    );
}

#[tokio::test]
async fn a_decision_may_lower_a_weight_but_never_raise_it() {
    let light = typed("light", "dwarf");
    let heavy = typed("heavy", "supergiant");
    // One task at a time, so the ceiling is the only rule in play.
    let raised = Fixed(vec![Tier::Supergiant]);
    assert_eq!(
        decide_all(Some(&raised), &[(&light, "a")]).await,
        vec![Tier::Dwarf],
        "a type declared light stays light however heavy the task looks"
    );
    let lowered = Fixed(vec![Tier::Dwarf]);
    assert_eq!(
        decide_all(Some(&lowered), &[(&heavy, "b")]).await,
        vec![Tier::Dwarf],
        "a type declared heavy still runs light when the task is light"
    );
}

/// One call is one piece of work, so its parts are not priced separately.
/// Splitting a feature per file and buying a cheap model for some of them
/// is the micro-management this avoids; the ceiling still binds each task.
#[tokio::test]
async fn a_wave_runs_at_its_heaviest_answer() {
    let light = typed("light", "dwarf");
    let middling = typed("middling", "giant");
    let heavy = typed("heavy", "supergiant");
    let decider = Fixed(vec![Tier::Dwarf, Tier::Dwarf, Tier::Giant]);
    assert_eq!(
        decide_all(
            Some(&decider),
            &[(&heavy, "a"), (&middling, "b"), (&light, "c")]
        )
        .await,
        vec![Tier::Giant, Tier::Giant, Tier::Dwarf],
        "the heaviest answer carries the wave, and each ceiling still holds"
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
        // A reply that cannot be lined up routed nothing, so this lands on
        // the same unrouted default as having no decider at all — not on
        // the ceiling, and not on the one answer that did arrive.
        vec![Tier::Dwarf, Tier::DEFAULT]
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

/// A type may also declare the rung it must not fall below. A document
/// someone will act on is worth writing properly, and a judge that reads
/// the request as light would otherwise buy the cheapest model for it.
#[tokio::test]
async fn a_declared_floor_holds_against_a_light_answer() {
    let written = typed_with("written", "giant", Some("subgiant"));
    let light = Fixed(vec![Tier::Dwarf]);
    assert_eq!(
        decide_all(Some(&light), &[(&written, "一行直す")]).await,
        vec![Tier::Subgiant],
        "the floor holds when the judge says the task is light"
    );
    // The ceiling still binds above it, and an unrouted run sits between.
    let heavy = Fixed(vec![Tier::Supergiant]);
    assert_eq!(
        decide_all(Some(&heavy), &[(&written, "全面的に書き直す")]).await,
        vec![Tier::Giant]
    );
    assert_eq!(
        decide_all(None, &[(&written, "書く")]).await,
        vec![Tier::DEFAULT],
        "no judge means the documented default, which is inside both bounds"
    );
}
