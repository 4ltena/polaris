//! Ordered typed failover with one request-local deadline and no background retries.
use super::{
    Decider, Decision, DecisionCapabilities, DecisionError, DecisionIdentity, DecisionObservation,
    Question, checked,
};
use crate::NetworkBoundary;
use std::{collections::BTreeMap, sync::Arc, time::Duration};

#[derive(Debug, Clone, Copy)]
pub struct FallbackPolicy {
    pub max_attempts: usize,
    pub attempt_timeout: Duration,
    pub total_timeout: Duration,
}
impl FallbackPolicy {
    pub fn validate(self, backends: usize) -> Result<(), DecisionError> {
        if backends == 0
            || backends > 4
            || self.max_attempts == 0
            || self.max_attempts > backends
            || self.attempt_timeout.is_zero()
            || self.attempt_timeout > self.total_timeout
            || self.total_timeout > Duration::from_secs(300)
        {
            return Err(DecisionError::Malformed);
        }
        Ok(())
    }
}

pub struct FallbackDecider {
    backends: Vec<Arc<dyn Decider>>,
    policy: FallbackPolicy,
}

impl FallbackDecider {
    pub fn new(
        backends: Vec<Arc<dyn Decider>>,
        policy: FallbackPolicy,
    ) -> Result<Self, DecisionError> {
        policy.validate(backends.len())?;
        Ok(Self { backends, policy })
    }
}

#[async_trait::async_trait]
impl Decider for FallbackDecider {
    fn may_return_calibrated_bands(&self) -> bool {
        self.backends
            .iter()
            .take(self.policy.max_attempts)
            .any(|backend| backend.may_return_calibrated_bands())
    }
    fn name(&self) -> &str {
        self.backends[0].name()
    }
    fn identity(&self) -> DecisionIdentity {
        self.backends[0].identity()
    }
    fn capabilities(&self) -> DecisionCapabilities {
        let mut capabilities = self.backends[0].capabilities();
        // Admission must cover every backend the chain could reach.
        if !self
            .backends
            .iter()
            .all(|backend| backend.capabilities().network == NetworkBoundary::LocalOnly)
        {
            capabilities.network = NetworkBoundary::Unknown;
        }
        capabilities
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
        let deadline = tokio::time::Instant::now() + self.policy.total_timeout;
        let mut attempts = Vec::new();
        let mut previous: Option<DecisionObservation> = None;
        for (index, backend) in self
            .backends
            .iter()
            .take(self.policy.max_attempts)
            .enumerate()
        {
            if tokio::time::Instant::now() >= deadline {
                let mut observation = previous.unwrap_or_else(|| DecisionObservation {
                    result: Err(DecisionError::Unavailable),
                    identity: self.identity(),
                    capabilities: self.capabilities(),
                    attempts: Vec::new(),
                });
                observation.result = Err(DecisionError::Unavailable);
                observation.attempts = attempts;
                return observation;
            }
            // No spawned task survives cancellation of this future. A dropped
            // call never reaches the next iteration; explicit Cancelled is terminal.
            let started = tokio::time::Instant::now();
            let until = deadline.min(started + self.policy.attempt_timeout);
            let mut observation =
                match tokio::time::timeout_at(until, backend.observe(state, questions)).await {
                    Ok(observation) => observation,
                    Err(_) => {
                        let mut observation = DecisionObservation::single(
                            backend.identity(),
                            backend.capabilities(),
                            Err(DecisionError::Unavailable),
                            started.elapsed(),
                        );
                        observation.attempts[0].timed_out = true;
                        observation
                    }
                };
            if let Ok(decision) = &observation.result
                && let Err(error) = checked(decision, questions)
            {
                observation.result = Err(error.clone());
                if let Some(attempt) = observation.attempts.last_mut() {
                    attempt.error = Some(error);
                }
            }
            attempts.append(&mut observation.attempts);
            let may_continue = matches!(
                observation.result,
                Err(DecisionError::Unavailable | DecisionError::TooLarge)
            ) && index + 1 < self.policy.max_attempts
                && tokio::time::Instant::now() < deadline;
            if !may_continue {
                observation.attempts = attempts;
                return observation;
            }
            previous = Some(observation);
        }
        unreachable!("constructor admits a nonempty bounded chain")
    }
}

#[cfg(test)]
mod tests;
