//! Host plans are request-local snapshots; unknown children and overflow never mean zero.
use super::*;
use polaris_provider::{CompletionRequest, CompletionResponse, ProviderError};
struct Model(Option<u64>);
#[async_trait::async_trait]
impl Provider for Model {
    fn planned_local_model_bytes(&self) -> Option<u64> {
        self.0
    }
    async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        panic!("no model request")
    }
}
struct Judge;
#[async_trait::async_trait]
impl Decider for Judge {
    async fn decide(
        &self,
        _: &serde_json::Value,
        _: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        panic!("context was lost")
    }
    async fn observe_with_context(
        &self,
        _: &serde_json::Value,
        _: &BTreeMap<String, Question>,
        context: DecisionContext,
    ) -> DecisionObservation {
        tokio::task::yield_now().await;
        let mut observed = DecisionObservation::single(
            self.identity(),
            self.capabilities(),
            Err(DecisionError::Unavailable),
            std::time::Duration::ZERO,
        );
        observed.attempts[0].reason = context
            .planned_local_model_bytes
            .map(|bytes| bytes.to_string());
        observed
    }
}
#[tokio::test]
async fn colocation_snapshots_survive_model_switches_and_meter_wrappers() {
    let sources = ColocationSources::new(vec![Arc::new(Model(Some(3))), Arc::new(Model(Some(4)))]);
    let meter = DecisionMeter::default();
    let judge = sources.bind(meter.wrap(Arc::new(Judge)));
    let questions = BTreeMap::new();
    let mut first = Box::pin(judge.observe(&serde_json::Value::Null, &questions));
    assert!(futures_util::poll!(first.as_mut()).is_pending());
    sources.replace(vec![Arc::new(Model(None))], false);
    let second = judge.observe(&serde_json::Value::Null, &questions).await;
    assert!(second.attempts[0].reason.is_none());
    assert_eq!(first.await.attempts[0].reason.as_deref(), Some("7"));
    assert_eq!(meter.recent().len(), 2);
    sources.replace(
        vec![Arc::new(Model(Some(u64::MAX))), Arc::new(Model(Some(1)))],
        false,
    );
    assert!(sources.context().planned_local_model_bytes.is_none());
    sources.replace(vec![Arc::new(Model(Some(0)))], true);
    assert!(sources.context().planned_local_model_bytes.is_none());
    assert!(
        ColocationSources::new(Vec::new())
            .context()
            .planned_local_model_bytes
            .is_none()
    );
}
#[test]
fn colocation_role_plans_include_primary_children_and_tiers() {
    use polaris_provider::role::RoleProvider;
    let known: Arc<dyn Provider> = Arc::new(Model(Some(2)));
    let unknown: Arc<dyn Provider> = Arc::new(Model(None));
    let roles = std::collections::HashMap::from([("reader".into(), known.clone())]);
    let provider = RoleProvider::new(known.clone(), roles);
    assert_eq!(provider.planned_local_model_bytes(), Some(2));
    let provider =
        provider.with_tiers(std::collections::HashMap::from([("small".into(), unknown)]));
    assert!(provider.planned_local_model_bytes().is_none());
}

#[tokio::test]
async fn colocation_context_reaches_every_explicit_fallback_attempt() {
    use polaris_provider::decision::fallback::{FallbackDecider, FallbackPolicy};
    let sources = ColocationSources::new(vec![Arc::new(Model(Some(7)))]);
    let chain = FallbackDecider::new(
        vec![Arc::new(Judge), Arc::new(Judge)],
        FallbackPolicy {
            max_attempts: 2,
            attempt_timeout: std::time::Duration::from_secs(1),
            total_timeout: std::time::Duration::from_secs(2),
        },
    )
    .unwrap();
    let meter = DecisionMeter::default();
    let judge = sources.bind(meter.wrap(Arc::new(chain)));
    let observed = judge
        .observe(&serde_json::Value::Null, &BTreeMap::new())
        .await;
    assert_eq!(observed.attempts.len(), 2);
    assert!(
        observed
            .attempts
            .iter()
            .all(|attempt| attempt.reason.as_deref() == Some("7"))
    );
}

#[tokio::test]
async fn desktop_event_wrapper_preserves_bound_plan_and_shared_provider_is_counted_once() {
    let provider: Arc<dyn Provider> = Arc::new(Model(Some(7)));
    let sources = ColocationSources::new(vec![provider.clone(), provider]);
    let (events, _receiver, _control) = crate::desktop_events::DesktopEventSink::channel(
        polaris_desktop_protocol::ids::RunId::new("capacity-fixture").unwrap(),
    );
    let judge = events.observe_decider(sources.bind(Arc::new(Judge)));
    let observed = judge
        .observe(&serde_json::Value::Null, &BTreeMap::new())
        .await;
    assert_eq!(observed.attempts[0].reason.as_deref(), Some("7"));
}
