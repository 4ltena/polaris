//! Lifecycle checks use a pending fake adapter and metadata-only observer.
use super::*;
use std::sync::atomic::AtomicUsize;
struct Judge {
    calls: Arc<AtomicUsize>,
    pending: bool,
}
#[async_trait::async_trait]
impl Decider for Judge {
    fn name(&self) -> &str {
        "fixture"
    }
    async fn decide(
        &self,
        _: &serde_json::Value,
        _: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if self.pending {
            std::future::pending::<()>().await;
        }
        Ok(Decision {
            answers: BTreeMap::new(),
            input_tokens: Some(2),
            output_tokens: Some(1),
        })
    }
}
struct Observer {
    starts: AtomicUsize,
    ends: AtomicUsize,
    accept: bool,
}
#[async_trait::async_trait]
impl DecisionObserver for Observer {
    async fn started(&self, _: &str, _: &DecisionIdentity) -> bool {
        self.starts.fetch_add(1, Ordering::SeqCst);
        self.accept
    }
    async fn settled(&self, _: &DecisionTrace) -> bool {
        self.ends.fetch_add(1, Ordering::SeqCst);
        true
    }
}
#[tokio::test]
async fn abandoned_stage_is_unknown_and_never_invents_an_effective_backend() {
    let meter = DecisionMeter::default();
    let observer = Arc::new(Observer {
        starts: AtomicUsize::new(0),
        ends: AtomicUsize::new(0),
        accept: true,
    });
    assert!(meter.install_observer(observer.clone()));
    assert!(!meter.install_observer(observer.clone()));
    let calls = Arc::new(AtomicUsize::new(0));
    let judge = meter.wrap(Arc::new(Judge {
        calls: calls.clone(),
        pending: true,
    }));
    let questions = BTreeMap::new();
    let mut future = Box::pin(judge.decide(&serde_json::Value::Null, &questions));
    assert!(futures_util::poll!(future.as_mut()).is_pending());
    drop(future);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(observer.starts.load(Ordering::SeqCst), 1);
    assert_eq!(observer.ends.load(Ordering::SeqCst), 0);
    let trace = meter.recent().pop().unwrap();
    assert!(trace.abandoned);
    assert!(trace.effective.is_none() && trace.capabilities.is_none() && trace.attempts.is_empty());
    assert_eq!(meter.snapshot().abandoned, 1);
    assert_eq!(meter.snapshot().reported, 0);
}
#[tokio::test]
async fn observer_start_failure_prevents_dispatch_and_success_keeps_usage() {
    for accept in [false, true] {
        let meter = DecisionMeter::default();
        let observer = Arc::new(Observer {
            starts: AtomicUsize::new(0),
            ends: AtomicUsize::new(0),
            accept,
        });
        meter.install_observer(observer.clone());
        let calls = Arc::new(AtomicUsize::new(0));
        let judge = meter.wrap(Arc::new(Judge {
            calls: calls.clone(),
            pending: false,
        }));
        let reply = judge
            .observe(
                &serde_json::json!({"private":"never-store-content"}),
                &BTreeMap::new(),
            )
            .await;
        assert_eq!(reply.result.is_ok(), accept);
        assert_eq!(calls.load(Ordering::SeqCst), usize::from(accept));
        assert_eq!(observer.ends.load(Ordering::SeqCst), usize::from(accept));
        assert_eq!(meter.snapshot().observer_failures, u64::from(!accept));
        assert_eq!(meter.snapshot().reported, u64::from(accept));
        assert!(
            !serde_json::to_string(&meter.recent())
                .unwrap()
                .contains("never-store-content")
        );
        assert!(!meter.recent()[0].abandoned);
    }
}

#[tokio::test]
async fn a_final_uninvoked_target_does_not_inherit_a_previous_dispatch() {
    struct Unavailable {
        name: &'static str,
        invoked: bool,
    }
    #[async_trait::async_trait]
    impl Decider for Unavailable {
        fn name(&self) -> &str {
            self.name
        }
        async fn decide(
            &self,
            _: &serde_json::Value,
            _: &BTreeMap<String, Question>,
        ) -> Result<Decision, DecisionError> {
            Err(DecisionError::Unavailable)
        }
        async fn observe(
            &self,
            _: &serde_json::Value,
            _: &BTreeMap<String, Question>,
        ) -> DecisionObservation {
            let mut reply = DecisionObservation::single(
                self.identity(),
                self.capabilities(),
                Err(DecisionError::Unavailable),
                std::time::Duration::ZERO,
            );
            reply.attempts[0].invoked = self.invoked;
            reply
        }
    }
    let chain = super::super::fallback::FallbackDecider::new(
        vec![
            Arc::new(Unavailable {
                name: "invoked-first",
                invoked: true,
            }),
            Arc::new(Unavailable {
                name: "uninstalled-last",
                invoked: false,
            }),
        ],
        super::super::fallback::FallbackPolicy {
            max_attempts: 2,
            attempt_timeout: std::time::Duration::from_secs(1),
            total_timeout: std::time::Duration::from_secs(2),
        },
    )
    .unwrap();
    let meter = DecisionMeter::default();
    let judge = meter.wrap(Arc::new(chain));
    assert!(
        judge
            .observe(&serde_json::Value::Null, &BTreeMap::new())
            .await
            .result
            .is_err()
    );
    let trace = meter.recent().pop().unwrap();
    assert!(trace.effective.is_none() && trace.capabilities.is_none());
    assert!(trace.attempts[0].invoked);
    assert_eq!(trace.attempts[0].identity.backend, "invoked-first");
    assert!(!trace.attempts[1].invoked);
    assert_eq!(trace.attempts[1].identity.backend, "uninstalled-last");
}
