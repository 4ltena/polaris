//! Local endpoint capacity, bounded waiters, cancellation, and quarantine tests.
use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::sync::Notify;

struct Fake {
    calls: AtomicUsize,
    release: Notify,
    text: &'static str,
    fail: bool,
}
#[async_trait::async_trait]
impl Provider for Fake {
    async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.release.notified().await;
        if self.fail {
            return Err(ProviderError::Http("synthetic".into()));
        }
        Ok(CompletionResponse {
            text: self.text.into(),
            tool_calls: vec![],
            reasoning: vec![],
            usage: None,
            hosted_web_search: vec![],
            url_citations: vec![],
        })
    }
}
fn fake(text: &'static str, fail: bool) -> Arc<Fake> {
    Arc::new(Fake {
        calls: AtomicUsize::new(0),
        release: Notify::new(),
        text,
        fail,
    })
}
fn binding(router: &LocalRouter, endpoint: &str, provider: Arc<Fake>) -> Arc<LocalBinding> {
    Arc::new(LocalBinding {
        gate: router.gate(&Endpoint::parse(endpoint).unwrap()),
        provider,
        selection: None,
    })
}
fn request() -> CompletionRequest {
    CompletionRequest {
        system: String::new(),
        messages: vec![],
        tools: vec![],
    }
}
async fn calls(fake: &Fake, count: usize) {
    tokio::time::timeout(std::time::Duration::from_secs(1), async {
        while fake.calls.load(Ordering::SeqCst) != count {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
fn run(
    binding: &Arc<LocalBinding>,
) -> tokio::task::JoinHandle<Result<CompletionResponse, ProviderError>> {
    let binding = binding.clone();
    tokio::spawn(async move { binding.complete(request()).await })
}
#[tokio::test]
async fn shared_endpoint_serializes_models_and_releases_before_tool_wait() {
    let router = LocalRouter::new(1);
    let a = fake("alpha", false);
    let b = fake("beta", false);
    let first = binding(&router, "http://127.0.0.1:11434", a.clone());
    let second = binding(&router, "http://127.0.0.1:11434/", b.clone());
    let task = run(&first);
    calls(&a, 1).await;
    let mut pending = Box::pin(second.complete(request()));
    assert!(futures_util::poll!(&mut pending).is_pending());
    assert_eq!(b.calls.load(Ordering::SeqCst), 0);
    a.release.notify_one();
    assert_eq!(task.await.unwrap().unwrap().text, "alpha");
    // The first binding remains alive during its caller's tool/approval work.
    b.release.notify_one();
    assert_eq!(pending.await.unwrap().text, "beta");
    assert_eq!(first.status(), EndpointStatus::Idle);
}
#[tokio::test]
async fn independent_endpoints_run_concurrently() {
    let router = LocalRouter::new(0);
    let a = fake("a", false);
    let b = fake("b", false);
    let first = binding(&router, "http://127.0.0.1:1", a.clone());
    let second = binding(&router, "http://127.0.0.1:2", b.clone());
    let x = run(&first);
    let y = run(&second);
    calls(&a, 1).await;
    calls(&b, 1).await;
    a.release.notify_one();
    b.release.notify_one();
    assert!(x.await.unwrap().is_ok());
    assert!(y.await.unwrap().is_ok());
}
#[tokio::test]
async fn bounded_pending_and_waiter_drop_do_not_dispatch_or_quarantine() {
    let router = LocalRouter::new(1);
    let fake = fake("ok", false);
    let binding = binding(&router, "http://127.0.0.1:1", fake.clone());
    let active = run(&binding);
    calls(&fake, 1).await;
    let mut waiting = Box::pin(binding.complete(request()));
    assert!(futures_util::poll!(&mut waiting).is_pending());
    assert!(binding.complete(request()).await.is_err());
    drop(waiting);
    let mut replacement = Box::pin(binding.complete(request()));
    assert!(futures_util::poll!(&mut replacement).is_pending());
    drop(replacement);
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
    assert_eq!(binding.status(), EndpointStatus::Running);
    fake.release.notify_one();
    assert!(active.await.unwrap().is_ok());
    assert_eq!(binding.status(), EndpointStatus::Idle);
}
#[tokio::test]
async fn dropped_flight_quarantines_existing_waiters_and_future_bindings() {
    let router = LocalRouter::new(1);
    let fake = fake("ok", false);
    let binding = binding(&router, "http://127.0.0.1:1", fake.clone());
    let active = run(&binding);
    calls(&fake, 1).await;
    let mut waiting = Box::pin(binding.complete(request()));
    assert!(futures_util::poll!(&mut waiting).is_pending());
    active.abort();
    assert!(active.await.unwrap_err().is_cancelled());
    assert!(waiting.await.is_err());
    assert_eq!(binding.status(), EndpointStatus::Quarantined);
    let rebound = super::tests::binding(&router, "http://127.0.0.1:1/", fake.clone());
    for _ in 0..20 {
        assert!(rebound.complete(request()).await.is_err());
    }
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn dispatched_error_is_preserved_and_never_automatically_retried() {
    let router = LocalRouter::new(0);
    let fake = fake("unused", true);
    let binding = binding(&router, "http://127.0.0.1:1", fake.clone());
    fake.release.notify_one();
    assert!(
        matches!(binding.complete(request()).await, Err(ProviderError::Http(s)) if s == "synthetic")
    );
    assert_eq!(binding.status(), EndpointStatus::Quarantined);
    assert!(binding.complete(request()).await.is_err());
    assert_eq!(fake.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn role_parent_and_child_share_local_slot_and_outer_meter() {
    use polaris_provider::{UsageMeter, role::RoleProvider};
    let router = LocalRouter::new(1);
    let a = fake("parent", false);
    let b = fake("child", false);
    let parent = binding(&router, "http://127.0.0.1:11434", a.clone());
    let child = binding(&router, "http://127.0.0.1:11434/", b.clone());
    let roles = Arc::new(RoleProvider::new(
        parent.clone(),
        HashMap::from([("reviewer".into(), child as Arc<dyn Provider>)]),
    ));
    let meter = UsageMeter::default();
    let provider = Arc::new(meter.wrap(roles));
    let selected = provider.resolve_role("reviewer").unwrap().unwrap();
    let active = {
        let provider = provider.clone();
        tokio::spawn(async move { provider.complete(request()).await })
    };
    calls(&a, 1).await;
    let mut waiting = Box::pin(selected.complete(request()));
    assert!(futures_util::poll!(&mut waiting).is_pending());
    assert_eq!(b.calls.load(Ordering::SeqCst), 0);
    a.release.notify_one();
    assert_eq!(active.await.unwrap().unwrap().text, "parent");
    b.release.notify_one();
    assert_eq!(waiting.await.unwrap().text, "child");
    assert_eq!(parent.status(), EndpointStatus::Idle);
    assert_eq!(meter.snapshot().missing_responses, 2);
    assert_eq!(meter.snapshot().failed_requests, 0);
}
