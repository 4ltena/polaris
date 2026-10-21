//! Synthetic dispatch probes for opt-in local-only admission and wrappers.
use super::*;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

struct Probe {
    local: AtomicBool,
    calls: AtomicUsize,
}

impl Probe {
    fn new(local: bool) -> Self {
        Self {
            local: AtomicBool::new(local),
            calls: AtomicUsize::new(0),
        }
    }
}

#[async_trait::async_trait]
impl Provider for Probe {
    fn network_boundary(&self) -> NetworkBoundary {
        if self.local.load(Ordering::SeqCst) {
            NetworkBoundary::LocalOnly
        } else {
            NetworkBoundary::RemoteAllowed
        }
    }
    async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            text: "fixture".into(),
            ..Default::default()
        })
    }
}

#[async_trait::async_trait]
impl Decider for Probe {
    fn capabilities(&self) -> DecisionCapabilities {
        DecisionCapabilities {
            network: self.network_boundary(),
            ..Default::default()
        }
    }
    async fn decide(
        &self,
        _: &serde_json::Value,
        _: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(Decision {
            answers: BTreeMap::new(),
            input_tokens: Some(1),
            output_tokens: Some(0),
        })
    }
}

fn request() -> CompletionRequest {
    CompletionRequest {
        system: String::new(),
        messages: vec![],
        tools: vec![],
    }
}

#[tokio::test]
async fn normal_mode_preserves_remote_judges_and_arbiters() {
    let probe = Arc::new(Probe::new(false));
    let arbiter = restrict_arbiter(false, Some(probe.clone())).unwrap();
    assert_eq!(arbiter.complete(request()).await.unwrap().text, "fixture");
    let judge = restrict_decider(false, probe.clone()).unwrap();
    judge
        .decide(&serde_json::Value::Null, &BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn local_only_allows_verified_dispatch_and_rechecks_each_request() {
    let probe = Arc::new(Probe::new(true));
    let arbiter = restrict_arbiter(true, Some(probe.clone())).unwrap();
    let judge = restrict_decider(true, probe.clone()).unwrap();
    assert_eq!(arbiter.complete(request()).await.unwrap().text, "fixture");
    judge
        .decide(&serde_json::Value::Null, &BTreeMap::new())
        .await
        .unwrap();
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
    probe.local.store(false, Ordering::SeqCst);
    assert!(arbiter.complete(request()).await.is_err());
    assert_eq!(
        judge
            .decide(&serde_json::Value::Null, &BTreeMap::new())
            .await,
        Err(DecisionError::Unavailable)
    );
    assert_eq!(probe.calls.load(Ordering::SeqCst), 2);
    assert!(restrict_arbiter(true, Some(probe.clone())).is_none());
    assert!(restrict_decider(true, probe).is_err());
}

#[tokio::test]
async fn provider_wrappers_preserve_the_attested_boundary() {
    let probe = Arc::new(Probe::new(true));
    let boxed: Box<dyn Provider> = Box::new(probe.clone());
    let meter = polaris_provider::UsageMeter::default();
    let wrapped: Arc<dyn Provider> = Arc::new(meter.wrap(boxed));
    assert_eq!(wrapped.network_boundary(), NetworkBoundary::LocalOnly);
    restrict_arbiter(true, Some(wrapped))
        .unwrap()
        .complete(request())
        .await
        .unwrap();
    assert_eq!(probe.calls.load(Ordering::SeqCst), 1);
}

struct Unknown;
#[async_trait::async_trait]
impl Provider for Unknown {
    async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        panic!("unknown confinement must not dispatch")
    }
}

#[test]
fn an_unknown_boundary_is_not_a_local_execution_guarantee() {
    assert!(restrict_arbiter(true, Some(Arc::new(Unknown))).is_none());
    assert!(restrict_arbiter(false, Some(Arc::new(Unknown))).is_some());
}
