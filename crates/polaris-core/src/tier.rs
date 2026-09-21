//! Deciding how much a delegated task is worth spending on.
//!
//! A type declares a weight in its `SKILL.md`, and that declaration is a
//! ceiling: a decider may say a particular task is lighter than the type
//! usually is, never heavier. The author knows what the type does; a
//! per-task guess does not, and the two errors are not symmetric. Guessing
//! too light costs one schema retry, which the existing `spawn` path
//! already handles. Guessing too heavy spends real money on every task a
//! cheap type runs, for as long as nobody notices.
//!
//! Having no decider at all is the default and is not a degraded mode: the
//! declared weight is used, which is the whole feature. A decider only
//! refines it.
use std::sync::Arc;

use polaris_skills::{AgentType, Tier};

pub mod chat;
pub mod typed;

#[async_trait::async_trait]
pub trait TierDecider: Send + Sync {
    /// One answer per task, in the order given. A decider that cannot
    /// answer returns `Tier::DEFAULT.min(agent.tier)` rather than failing: routing is
    /// an optimization, and a delegation must not be lost because the
    /// service that prices it was unreachable.
    ///
    /// Returning something heavier than declared is not an error the caller
    /// reports — `decide_all` clamps every answer — so an implementation
    /// need not enforce the ceiling itself.
    async fn decide(&self, tasks: &[(&AgentType, &str)]) -> Vec<Tier>;
}

/// Asks the decider, then applies the ceiling. The clamp lives here, once,
/// rather than in each implementation, so a new decider cannot raise a
/// weight by forgetting to.
pub async fn decide_all(
    decider: Option<&dyn TierDecider>,
    tasks: &[(&AgentType, &str)],
) -> Vec<Tier> {
    // Without a routing answer the weight falls to the documented default,
    // held down by the type's ceiling — never to the ceiling itself.
    //
    // `polaris-tier` is a ceiling: `implementer` declares `supergiant` so
    // that the judge can reach all four rungs, not because every
    // implementation is worth `sol`. Running the ceiling when nobody
    // routed meant the ordinary case paid 33.7 against `subgiant`'s 10.0,
    // in exactly the configuration that has no judge to notice. The
    // ceiling still binds: a type that declares `dwarf` keeps `dwarf`,
    // because `min` cannot hand it something heavier than it allows.
    // Both ends of the declaration, applied in one place so no path can
    // honour one and forget the other: `tier` is the ceiling a decision
    // may not route above, `tier_min` the floor it may not route below.
    let bounded = |agent: &AgentType, decided: Tier| decided.min(agent.tier).max(agent.tier_min);
    let unrouted = || {
        tasks
            .iter()
            .map(|(agent, _)| bounded(agent, Tier::DEFAULT))
            .collect::<Vec<_>>()
    };
    let Some(decider) = decider else {
        return unrouted();
    };
    let answered = decider.decide(tasks).await;
    // A decider that returns the wrong number of answers has told us
    // nothing we can line up with the tasks, so none of it is used.
    if answered.len() != tasks.len() {
        return unrouted();
    }
    // One call is one piece of work. Pricing its parts separately is
    // micro-management: splitting a feature across files and running one
    // on `luna` and its neighbour on `sol` buys a cheap model for half of
    // something that has to hold together, and the half that was judged
    // heavy is the half that says what the work is worth. So the wave
    // takes its heaviest answer — still held down for each task by its own
    // declared ceiling, which no decision may raise.
    let heaviest = answered.iter().copied().max().unwrap_or(Tier::DEFAULT);
    tasks
        .iter()
        .map(|(agent, _)| bounded(agent, heaviest))
        .collect()
}

/// Answers `resolve_tier` with one fixed weight, whatever is asked. The
/// wave decides a task's weight once and hands the child a provider that
/// already knows it, which keeps the decision out of the per-task path and
/// leaves `resolve_role` — and so a role binding's precedence — untouched.
pub(crate) struct PinnedTier {
    inner: Arc<dyn polaris_provider::Provider>,
    tier: Tier,
}

impl PinnedTier {
    /// `None` when the decision landed on what the type already declared,
    /// since then there is nothing to pin and the plain provider will ask
    /// for the same weight anyway.
    pub(crate) fn wrap(
        inner: Arc<dyn polaris_provider::Provider>,
        declared: Tier,
        decided: Tier,
    ) -> Arc<dyn polaris_provider::Provider> {
        if decided == declared {
            return inner;
        }
        Arc::new(Self {
            inner,
            tier: decided,
        })
    }
}

#[async_trait::async_trait]
impl polaris_provider::Provider for PinnedTier {
    async fn complete(
        &self,
        request: polaris_provider::CompletionRequest,
    ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
        self.inner.complete(request).await
    }

    async fn complete_in_turn(
        &self,
        request: polaris_provider::CompletionRequest,
        turn: &polaris_provider::turn_affinity::TurnContext,
    ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
        self.inner.complete_in_turn(request, turn).await
    }

    async fn complete_in_turn_with_observer(
        &self,
        request: polaris_provider::CompletionRequest,
        turn: &polaris_provider::turn_affinity::TurnContext,
        observer: Option<&dyn polaris_provider::ResponseObserver>,
    ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
        self.inner
            .complete_in_turn_with_observer(request, turn, observer)
            .await
    }

    fn resolve_role(
        &self,
        role: &str,
    ) -> Result<Option<Arc<dyn polaris_provider::Provider>>, polaris_provider::ProviderError> {
        self.inner.resolve_role(role)
    }

    fn resolve_tier(
        &self,
        _asked: &str,
    ) -> Result<Option<Arc<dyn polaris_provider::Provider>>, polaris_provider::ProviderError> {
        self.inner.resolve_tier(self.tier.name())
    }

    fn set_model(&self, model: &str) {
        self.inner.set_model(model);
    }

    fn model(&self) -> Option<String> {
        self.inner.model()
    }

    fn set_effort(&self, effort: Option<&str>) {
        self.inner.set_effort(effort);
    }
}

#[cfg(test)]
#[path = "tier/tests.rs"]
mod tests;
