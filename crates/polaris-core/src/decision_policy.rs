//! Local-only admission for judgments and auxiliary generative arbitration.
//! This policy does not change the primary conversation provider or tool grants.
use polaris_provider::decision::{
    Decider, Decision, DecisionCapabilities, DecisionError, Question,
};
use polaris_provider::{
    CompletionRequest, CompletionResponse, NetworkBoundary, Provider, ProviderError,
};
use std::{collections::BTreeMap, sync::Arc};

pub fn restrict_arbiter(
    local_only: bool,
    provider: Option<Arc<dyn Provider>>,
) -> Option<Arc<dyn Provider>> {
    if !local_only {
        return provider;
    }
    let provider = provider?;
    if provider.network_boundary() != NetworkBoundary::LocalOnly {
        return None;
    }
    Some(Arc::new(LocalArbiter(provider)))
}

pub(crate) fn restrict_decider(
    local_only: bool,
    decider: Arc<dyn Decider>,
) -> Result<Arc<dyn Decider>, DecisionError> {
    if !local_only {
        return Ok(decider);
    }
    if decider.capabilities().network != NetworkBoundary::LocalOnly {
        return Err(DecisionError::Unavailable);
    }
    Ok(Arc::new(LocalDecider(decider)))
}

struct LocalArbiter(Arc<dyn Provider>);
#[async_trait::async_trait]
impl Provider for LocalArbiter {
    fn planned_local_model_bytes(&self) -> Option<u64> {
        self.0.planned_local_model_bytes()
    }

    fn network_boundary(&self) -> NetworkBoundary {
        self.0.network_boundary()
    }
    fn model(&self) -> Option<String> {
        self.0.model()
    }
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        if self.0.network_boundary() != NetworkBoundary::LocalOnly {
            return Err(ProviderError::Unsupported(
                "local-only arbitration boundary".into(),
            ));
        }
        self.0.complete(request).await
    }
}

struct LocalDecider(Arc<dyn Decider>);
#[async_trait::async_trait]
impl Decider for LocalDecider {
    fn telemetry(&self) -> Option<polaris_provider::decision::DecisionMeter> {
        self.0.telemetry()
    }

    fn may_return_calibrated_bands(&self) -> bool {
        self.0.may_return_calibrated_bands()
    }
    fn identity(&self) -> polaris_provider::decision::DecisionIdentity {
        self.0.identity()
    }
    fn name(&self) -> &str {
        self.0.name()
    }
    fn capabilities(&self) -> DecisionCapabilities {
        self.0.capabilities()
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
    ) -> polaris_provider::decision::DecisionObservation {
        self.observe_with_context(
            state,
            questions,
            polaris_provider::decision::DecisionContext::default(),
        )
        .await
    }
    async fn observe_with_context(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
        context: polaris_provider::decision::DecisionContext,
    ) -> polaris_provider::decision::DecisionObservation {
        if self.0.capabilities().network != NetworkBoundary::LocalOnly {
            let mut observation = polaris_provider::decision::DecisionObservation::single(
                self.identity(),
                self.capabilities(),
                Err(DecisionError::Unavailable),
                std::time::Duration::ZERO,
            );
            observation.attempts[0].invoked = false;
            observation.attempts[0].reason = Some("local_only".into());
            return observation;
        }
        self.0.observe_with_context(state, questions, context).await
    }
}

#[cfg(test)]
mod tests;
