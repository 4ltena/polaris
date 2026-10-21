//! Host-owned provider plans captured per typed request, not a shared last-budget slot.
use polaris_provider::{
    Provider,
    decision::{
        Decider, Decision, DecisionCapabilities, DecisionContext, DecisionError, DecisionIdentity,
        DecisionMeter, DecisionObservation, Question,
    },
};
use std::{
    collections::BTreeMap,
    sync::{Arc, RwLock},
};

#[derive(Clone)]
pub struct ColocationSources(Arc<RwLock<Plans>>);
struct Plans {
    providers: Vec<Arc<dyn Provider>>,
    unknown_resources: bool,
}
impl ColocationSources {
    pub fn new(providers: Vec<Arc<dyn Provider>>) -> Self {
        Self(Arc::new(RwLock::new(Plans {
            providers,
            unknown_resources: false,
        })))
    }
    /// The TUI host refreshes this after model/session activation. It is not a
    /// project-configurable byte allowance; each observation captures its own copy.
    pub fn replace(&self, providers: Vec<Arc<dyn Provider>>, unknown_resources: bool) {
        *self.0.write().expect("colocation sources poisoned") = Plans {
            providers,
            unknown_resources,
        };
    }
    pub fn context(&self) -> DecisionContext {
        let sources = self.0.read().expect("colocation sources poisoned");
        DecisionContext {
            planned_local_model_bytes: (!sources.providers.is_empty()
                && !sources.unknown_resources)
                .then(|| polaris_provider::planned_local_model_total(&sources.providers))
                .flatten(),
        }
    }
    pub fn bind(&self, inner: Arc<dyn Decider>) -> Arc<dyn Decider> {
        Arc::new(Bound {
            inner,
            sources: self.clone(),
        })
    }
}
struct Bound {
    inner: Arc<dyn Decider>,
    sources: ColocationSources,
}
#[async_trait::async_trait]
impl Decider for Bound {
    fn telemetry(&self) -> Option<DecisionMeter> {
        self.inner.telemetry()
    }
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn identity(&self) -> DecisionIdentity {
        self.inner.identity()
    }
    fn capabilities(&self) -> DecisionCapabilities {
        self.inner.capabilities()
    }
    fn may_return_calibrated_bands(&self) -> bool {
        self.inner.may_return_calibrated_bands()
    }
    async fn decide(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        self.observe(state, questions).await.result
    }
    async fn observe(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> DecisionObservation {
        self.inner
            .observe_with_context(state, questions, self.sources.context())
            .await
    }
    async fn observe_with_context(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
        _context: DecisionContext,
    ) -> DecisionObservation {
        // A host binding is authoritative. Event wrappers supply default context;
        // that must not replace this run's snapshotted provider/resource plan.
        self.observe(state, questions).await
    }
}

#[cfg(test)]
mod tests;
