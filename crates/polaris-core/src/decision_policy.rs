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
        if self.0.capabilities().network != NetworkBoundary::LocalOnly {
            return Err(DecisionError::Unavailable);
        }
        self.0.decide(state, questions).await
    }
}

#[cfg(test)]
mod tests;
