//! Request-local lifecycle observations, including dropped stages with unknown usage.
use super::{
    Decider, Decision, DecisionError, DecisionIdentity, DecisionObservation, DecisionTrace,
    DecisionUsage, Question,
};
use std::{
    collections::{BTreeMap, VecDeque},
    sync::{
        Arc, Mutex, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Instant, SystemTime, UNIX_EPOCH},
};

/// Host-owned audit callbacks receive metadata only. Start is written before
/// adapter invocation; an unmatched start means an unsettled stage, not success.
#[async_trait::async_trait]
pub trait DecisionObserver: Send + Sync {
    async fn started(&self, request_id: &str, configured: &DecisionIdentity) -> bool;
    async fn settled(&self, trace: &DecisionTrace) -> bool;
}

#[derive(Clone, Default)]
pub struct DecisionMeter {
    usage: Arc<Mutex<DecisionUsage>>,
    traces: Arc<Mutex<VecDeque<DecisionTrace>>>,
    observer: Arc<RwLock<Option<Arc<dyn DecisionObserver>>>>,
}
impl DecisionMeter {
    pub fn snapshot(&self) -> DecisionUsage {
        *self.usage.lock().expect("decision meter poisoned")
    }
    /// At most 32 recent stages, each bounded to 16 KiB, with no state or answers.
    pub fn recent(&self) -> Vec<DecisionTrace> {
        self.traces
            .lock()
            .expect("decision trace poisoned")
            .iter()
            .cloned()
            .collect()
    }
    /// One host owner per meter. Rebinding an active audit destination is refused.
    pub fn install_observer(&self, observer: Arc<dyn DecisionObserver>) -> bool {
        let mut held = self.observer.write().expect("decision observer poisoned");
        if held.is_some() {
            return false;
        }
        *held = Some(observer);
        true
    }
    pub fn wrap(&self, decider: Arc<dyn Decider>) -> Arc<dyn Decider> {
        Arc::new(MeteredDecider {
            decider,
            meter: self.clone(),
        })
    }
    fn retain(&self, trace: DecisionTrace) {
        if serde_json::to_vec(&trace).is_ok_and(|bytes| bytes.len() <= 16 * 1024) {
            let mut traces = self.traces.lock().expect("decision trace poisoned");
            if traces.len() == 32 {
                traces.pop_front();
            }
            traces.push_back(trace);
        }
    }
    fn observer_failed(&self) {
        let mut usage = self.usage.lock().expect("decision meter poisoned");
        usage.observer_failures = usage.observer_failures.saturating_add(1);
    }
}

struct Stage {
    meter: DecisionMeter,
    trace: Option<DecisionTrace>,
    started: Instant,
}
impl Stage {
    fn new(meter: DecisionMeter, configured: DecisionIdentity) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        Self {
            meter,
            trace: Some(DecisionTrace {
                configured,
                request_id: format!(
                    "{}-{stamp}-{}",
                    std::process::id(),
                    NEXT.fetch_add(1, Ordering::Relaxed)
                ),
                abandoned: true,
                elapsed_ms: 0,
                effective: None,
                capabilities: None,
                error: None,
                attempts: Vec::new(),
            }),
            started: Instant::now(),
        }
    }
    fn finish(&mut self, observation: &DecisionObservation) -> DecisionTrace {
        let mut trace = self.trace.take().expect("unsettled stage");
        trace.abandoned = false;
        trace.elapsed_ms = self
            .started
            .elapsed()
            .as_millis()
            .try_into()
            .unwrap_or(u64::MAX);
        let invoked = observation
            .attempts
            .last()
            .is_some_and(|attempt| attempt.invoked);
        trace.effective = invoked.then(|| observation.identity.clone());
        trace.capabilities = invoked.then_some(observation.capabilities);
        trace.error = observation.result.as_ref().err().cloned();
        trace.attempts = observation.attempts.clone();
        let mut usage = self.meter.usage.lock().expect("decision meter poisoned");
        for attempt in &observation.attempts {
            if attempt.error.is_some() {
                usage.failed = usage.failed.saturating_add(1);
            } else {
                if let Some(input) = attempt.input_tokens {
                    usage.input_tokens = usage.input_tokens.saturating_add(input);
                }
                if let Some(output) = attempt.output_tokens {
                    usage.output_tokens = usage.output_tokens.saturating_add(output);
                }
                if attempt.input_tokens.is_some() && attempt.output_tokens.is_some() {
                    usage.reported = usage.reported.saturating_add(1);
                } else {
                    usage.missing = usage.missing.saturating_add(1);
                }
            }
        }
        drop(usage);
        self.meter.retain(trace.clone());
        trace
    }
}
impl Drop for Stage {
    fn drop(&mut self) {
        if let Some(mut trace) = self.trace.take() {
            // A future can be dropped during any attempt or audit write. Do not
            // invent an effective backend or claim physical cancellation.
            trace.elapsed_ms = self
                .started
                .elapsed()
                .as_millis()
                .try_into()
                .unwrap_or(u64::MAX);
            let mut usage = self.meter.usage.lock().expect("decision meter poisoned");
            usage.abandoned = usage.abandoned.saturating_add(1);
            drop(usage);
            self.meter.retain(trace);
        }
    }
}

struct MeteredDecider {
    decider: Arc<dyn Decider>,
    meter: DecisionMeter,
}
#[async_trait::async_trait]
impl Decider for MeteredDecider {
    fn telemetry(&self) -> Option<DecisionMeter> {
        Some(self.meter.clone())
    }
    fn may_return_calibrated_bands(&self) -> bool {
        self.decider.may_return_calibrated_bands()
    }
    fn identity(&self) -> DecisionIdentity {
        self.decider.identity()
    }
    fn name(&self) -> &str {
        self.decider.name()
    }
    fn capabilities(&self) -> super::DecisionCapabilities {
        self.decider.capabilities()
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
        self.observe_with_context(
            state,
            questions,
            crate::decision::DecisionContext::default(),
        )
        .await
    }
    async fn observe_with_context(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
        context: crate::decision::DecisionContext,
    ) -> crate::decision::DecisionObservation {
        let observer = self
            .meter
            .observer
            .read()
            .expect("decision observer poisoned")
            .clone();
        let mut stage = Stage::new(self.meter.clone(), self.identity());
        if let Some(observer) = &observer {
            let trace = stage.trace.as_ref().expect("unsettled stage");
            if !observer.started(&trace.request_id, &trace.configured).await {
                self.meter.observer_failed();
                let mut refused = DecisionObservation::single(
                    self.identity(),
                    self.capabilities(),
                    Err(DecisionError::Unavailable),
                    stage.started.elapsed(),
                );
                refused.attempts[0].invoked = false;
                refused.attempts[0].reason = Some("audit_unavailable".into());
                stage.finish(&refused);
                return refused;
            }
        }
        let observation = self
            .decider
            .observe_with_context(state, questions, context)
            .await;
        let trace = stage.finish(&observation);
        if let Some(observer) = observer
            && !observer.settled(&trace).await
        {
            self.meter.observer_failed();
        }
        observation
    }
}

#[cfg(test)]
mod tests;
