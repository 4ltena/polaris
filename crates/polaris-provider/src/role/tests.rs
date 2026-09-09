//! Immutable role routing, missing-binding refusal, and usage propagation.
use super::*;
use crate::{Usage, UsageMeter, turn_affinity::TurnContext};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Fake {
    name: &'static str,
    calls: AtomicUsize,
}
#[async_trait::async_trait]
impl Provider for Fake {
    async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(CompletionResponse {
            text: self.name.into(),
            usage: Some(Usage {
                input_tokens: 2,
                output_tokens: 1,
                total_tokens: 3,
                cached_tokens: 0,
            }),
            ..Default::default()
        })
    }
}
fn fake(name: &'static str) -> Arc<Fake> {
    Arc::new(Fake {
        name,
        calls: AtomicUsize::new(0),
    })
}
fn request() -> CompletionRequest {
    CompletionRequest {
        system: String::new(),
        messages: vec![],
        tools: vec![],
    }
}

#[tokio::test]
async fn role_fixed_routes_and_missing_role_never_falls_back() {
    let parent = fake("parent");
    let child = fake("child");
    let router = RoleProvider::new(
        parent.clone(),
        HashMap::from([("reviewer".into(), child.clone() as Arc<dyn Provider>)]),
    );
    assert!(router.resolve_role("missing").is_err());
    assert_eq!(parent.calls.load(Ordering::SeqCst), 0);
    assert_eq!(child.calls.load(Ordering::SeqCst), 0);
    assert!(parent.resolve_role("anything").unwrap().is_none());
    let selected = router.resolve_role("reviewer").unwrap().unwrap();
    router.set_model("replacement");
    router.set_effort(Some("high"));
    let (a, b) = tokio::join!(router.complete(request()), selected.complete(request()));
    assert_eq!(a.unwrap().text, "parent");
    assert_eq!(b.unwrap().text, "child");
    assert_eq!(selected.complete(request()).await.unwrap().text, "child");
    assert_eq!(parent.calls.load(Ordering::SeqCst), 1);
    assert_eq!(child.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn role_arc_reference_and_nested_meters_preserve_shared_totals() {
    let parent = fake("parent");
    let child = fake("child");
    let router: Arc<dyn Provider> = Arc::new(RoleProvider::new(
        parent.clone(),
        HashMap::from([("reviewer".into(), child.clone() as Arc<dyn Provider>)]),
    ));
    let inner = UsageMeter::default();
    let outer = UsageMeter::default();
    let metered = inner.wrap(router);
    let wrapped = outer.wrap(&metered);
    // Force Provider dispatch through reference and Arc implementations, not autoderef.
    let reference: &dyn Provider = &(&wrapped);
    let selected = reference.resolve_role("reviewer").unwrap().unwrap();
    assert!(reference.resolve_role("missing").is_err());
    assert_eq!(outer.snapshot().reported_responses, 0);
    wrapped.complete(request()).await.unwrap();
    selected
        .complete_in_turn_with_observer(request(), &TurnContext::new(), None)
        .await
        .unwrap();
    selected.complete(request()).await.unwrap();
    for meter in [inner, outer] {
        assert_eq!(meter.snapshot().reported_responses, 3);
        assert_eq!(meter.snapshot().usage.total_tokens, 9);
        assert_eq!(meter.snapshot().failed_requests, 0);
    }
    assert_eq!(parent.calls.load(Ordering::SeqCst), 1);
    assert_eq!(child.calls.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn role_parent_preserves_turn_and_observer_hooks() {
    struct TurnOnly;
    #[async_trait::async_trait]
    impl Provider for TurnOnly {
        async fn complete(
            &self,
            _: CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            panic!("turn hook lost")
        }
        async fn complete_in_turn_with_observer(
            &self,
            _: CompletionRequest,
            _: &TurnContext,
            _: Option<&dyn ResponseObserver>,
        ) -> Result<CompletionResponse, ProviderError> {
            Ok(CompletionResponse {
                text: "observed".into(),
                ..Default::default()
            })
        }
    }
    let router = RoleProvider::new(Arc::new(TurnOnly), HashMap::new());
    assert_eq!(
        router
            .complete_in_turn_with_observer(request(), &TurnContext::new(), None)
            .await
            .unwrap()
            .text,
        "observed"
    );
}

#[tokio::test]
async fn role_resolved_errors_and_cancellation_reach_outer_meter() {
    struct Failing;
    struct Pending;
    #[async_trait::async_trait]
    impl Provider for Failing {
        async fn complete(
            &self,
            _: CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            Err(ProviderError::Http("fixture".into()))
        }
    }
    #[async_trait::async_trait]
    impl Provider for Pending {
        async fn complete(
            &self,
            _: CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            std::future::pending().await
        }
    }
    let router: Arc<dyn Provider> = Arc::new(RoleProvider::new(
        fake("parent"),
        HashMap::from([
            ("failure".into(), Arc::new(Failing) as Arc<dyn Provider>),
            ("pending".into(), Arc::new(Pending) as Arc<dyn Provider>),
        ]),
    ));
    // Exercise Arc's Provider implementation explicitly.
    assert!(Provider::resolve_role(&router, "missing").is_err());
    let meter = UsageMeter::default();
    let metered = meter.wrap(router);
    let failing = metered.resolve_role("failure").unwrap().unwrap();
    assert!(
        matches!(failing.complete(request()).await, Err(ProviderError::Http(s)) if s == "fixture")
    );
    let pending = metered.resolve_role("pending").unwrap().unwrap();
    let mut future = Box::pin(pending.complete(request()));
    assert!(futures_util::poll!(&mut future).is_pending());
    drop(future);
    assert_eq!(meter.snapshot().failed_requests, 2);
    assert_eq!(meter.snapshot().reported_responses, 0);
}
