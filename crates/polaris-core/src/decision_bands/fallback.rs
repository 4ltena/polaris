//! Build only an explicitly configured routing chain; finalizers never use it.
use super::*;
use crate::{
    config::{DecisionBackend, DecisionConfig},
    managed_decision::ManagedDecisionError,
};
use polaris_provider::decision::{
    Decision, DecisionCapabilities, DecisionIdentity,
    fallback::{FallbackDecider, FallbackPolicy},
};
use std::{sync::Arc, time::Duration};

pub(super) fn build(
    config: &DecisionConfig,
    mut key: impl FnMut(&str) -> Option<String>,
    mut clef: impl FnMut(&str) -> Result<Arc<dyn Decider>, ManagedDecisionError>,
) -> Result<Arc<dyn Decider>, DecisionBuildError> {
    let fallback = config.fallback.as_ref().expect("caller selected a chain");
    fallback
        .validate(config)
        .map_err(|_| DecisionBuildError::InvalidSettings(DecisionError::Malformed))?;
    let policy = FallbackPolicy {
        max_attempts: fallback.max_attempts,
        attempt_timeout: Duration::from_millis(fallback.attempt_timeout_ms),
        total_timeout: Duration::from_millis(fallback.total_timeout_ms),
    };
    policy
        .validate(fallback.targets.len() + 1)
        .map_err(DecisionBuildError::InvalidSettings)?;
    if config.local_only
        && std::iter::once(config)
            .chain(&fallback.targets)
            .any(|target| target.backend != DecisionBackend::Clef)
    {
        return Err(DecisionBuildError::LocalOnlyUnavailable);
    }
    let mut backends = Vec::new();
    for (index, target) in std::iter::once(config)
        .chain(&fallback.targets)
        .take(policy.max_attempts)
        .enumerate()
    {
        let target = DecisionConfig {
            fallback: None,
            local_only: config.local_only || target.local_only,
            ..target.clone()
        };
        let result =
            super::build_one_with_sources(&target, DecisionUse::Routing, &mut key, &mut clef, true);
        let backend: Arc<dyn Decider> = match result {
            Ok(Some(backend)) => backend,
            Ok(None) => {
                return Err(DecisionBuildError::InvalidSettings(
                    DecisionError::Malformed,
                ));
            }
            Err(error) => {
                let (error, reason) = match error {
                    DecisionBuildError::MissingSemIfHelper => {
                        (DecisionError::Unavailable, "helper_unavailable")
                    }
                    DecisionBuildError::Clef(
                        error @ (ManagedDecisionError::UnverifiedProfile
                        | ManagedDecisionError::NotInstalled
                        | ManagedDecisionError::UnsupportedPlatform
                        | ManagedDecisionError::UnsupportedDevice),
                    ) => (
                        DecisionError::Unavailable,
                        match error {
                            ManagedDecisionError::UnverifiedProfile => "unverified_profile",
                            ManagedDecisionError::NotInstalled => "not_installed",
                            ManagedDecisionError::UnsupportedPlatform => "unsupported_platform",
                            _ => "unsupported_device",
                        },
                    ),
                    DecisionBuildError::MissingJevKey | DecisionBuildError::MissingSystemOneKey
                        if index > 0 =>
                    {
                        (DecisionError::Auth, "missing_credential")
                    }
                    DecisionBuildError::InvalidSettings(DecisionError::Auth) if index > 0 => {
                        (DecisionError::Auth, "invalid_credential")
                    }
                    other => return Err(other),
                };
                Arc::new(Unavailable {
                    identity: DecisionIdentity {
                        backend: target.backend.as_str().into(),
                        model: if target.backend == DecisionBackend::SystemOne {
                            target
                                .system_one
                                .as_ref()
                                .map(|profile| profile.model.clone())
                        } else {
                            target.model.clone()
                        },
                        revision: None,
                        quantization: None,
                    },
                    error,
                    reason,
                })
            }
        };
        backends.push(backend);
    }
    let chain: Arc<dyn Decider> = Arc::new(
        FallbackDecider::new(backends, policy).map_err(DecisionBuildError::InvalidSettings)?,
    );
    crate::decision_policy::restrict_decider(config.local_only, chain)
        .map_err(|_| DecisionBuildError::LocalOnlyUnavailable)
}

struct Unavailable {
    identity: DecisionIdentity,
    error: DecisionError,
    reason: &'static str,
}
#[async_trait::async_trait]
impl Decider for Unavailable {
    fn name(&self) -> &str {
        &self.identity.backend
    }
    fn identity(&self) -> DecisionIdentity {
        self.identity.clone()
    }
    fn capabilities(&self) -> DecisionCapabilities {
        // This placeholder performs no I/O. It does not attest a model's runtime.
        DecisionCapabilities {
            network: polaris_provider::NetworkBoundary::LocalOnly,
            ..Default::default()
        }
    }
    async fn decide(
        &self,
        _: &serde_json::Value,
        _: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        Err(self.error.clone())
    }
    async fn observe(
        &self,
        _: &serde_json::Value,
        _: &BTreeMap<String, Question>,
    ) -> polaris_provider::decision::DecisionObservation {
        let mut observation = polaris_provider::decision::DecisionObservation::single(
            self.identity(),
            self.capabilities(),
            Err(self.error.clone()),
            Duration::ZERO,
        );
        observation.attempts[0].invoked = false;
        observation.attempts[0].reason = Some(self.reason.into());
        observation
    }
}

pub(super) fn identify(config: &DecisionConfig, inner: Arc<dyn Decider>) -> Arc<dyn Decider> {
    let mut identity = inner.identity();
    // Preserve the managed owner's accepted labels. A profile ID is only a
    // configured selector when that owner has no model metadata.
    if identity.model.is_none() {
        identity.model = config.model.clone();
    }
    Arc::new(Identified { inner, identity })
}
struct Identified {
    inner: Arc<dyn Decider>,
    identity: DecisionIdentity,
}
#[async_trait::async_trait]
impl Decider for Identified {
    async fn observe(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> polaris_provider::decision::DecisionObservation {
        self.observe_with_context(state, questions, Default::default())
            .await
    }
    async fn observe_with_context(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
        context: polaris_provider::decision::DecisionContext,
    ) -> polaris_provider::decision::DecisionObservation {
        self.inner
            .observe_with_context(state, questions, context)
            .await
    }

    fn may_return_calibrated_bands(&self) -> bool {
        self.inner.may_return_calibrated_bands()
    }
    fn name(&self) -> &str {
        self.inner.name()
    }
    fn identity(&self) -> DecisionIdentity {
        self.identity.clone()
    }
    fn capabilities(&self) -> DecisionCapabilities {
        self.inner.capabilities()
    }
    async fn decide(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        self.observe(state, questions).await.result
    }
}

#[cfg(test)]
mod tests;
