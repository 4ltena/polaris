//! Generative receipts preserve calls, missing usage and unknown cancellation.
use super::*;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicUsize, Ordering},
};

struct Fixture {
    log: PathBuf,
    calls: AtomicUsize,
    mode: u8,
    break_audit: Option<Arc<Mutex<AuditLog>>>,
}
#[async_trait::async_trait]
impl Provider for Fixture {
    fn model(&self) -> Option<String> {
        Some("synthetic-selector".into())
    }
    fn planned_local_model_bytes(&self) -> Option<u64> {
        Some(7)
    }
    fn network_boundary(&self) -> NetworkBoundary {
        NetworkBoundary::LocalOnly
    }
    async fn complete(&self, request: CompletionRequest) -> Result {
        self.calls.fetch_add(1, Ordering::SeqCst);
        assert_eq!(request.messages[0].content, "private-synthetic-input");
        assert!(
            std::fs::read_to_string(&self.log)
                .unwrap()
                .contains("start")
        );
        if self.mode == 2 {
            std::future::pending::<()>().await;
        }
        if let Some(audit) = &self.break_audit {
            audit.lock().await.file = std::fs::File::open(&self.log).unwrap();
        }
        if self.mode == 3 {
            return Err(ProviderError::Auth("private-synthetic-error".into()));
        }
        if self.mode == 4 {
            return Err(ProviderError::ReceivedUsage {
                source: Box::new(ProviderError::Decode("private-synthetic-error".into())),
                usage: Usage {
                    input_tokens: 9,
                    ..Default::default()
                },
            });
        }
        Ok(CompletionResponse {
            text: "private-synthetic-answer".into(),
            usage: (self.mode != 1).then_some(Usage {
                input_tokens: 3,
                ..Default::default()
            }),
            ..Default::default()
        })
    }
}
fn request() -> CompletionRequest {
    CompletionRequest {
        system: "private-synthetic-system".into(),
        messages: vec![polaris_provider::Message::user("private-synthetic-input")],
        tools: Vec::new(),
    }
}
fn rows(path: &std::path::Path) -> Vec<serde_json::Value> {
    let saved = std::fs::read_to_string(path).unwrap();
    assert!(!saved.contains("private-synthetic"));
    saved
        .lines()
        .map(|line| {
            let row: serde_json::Value = serde_json::from_str(line).unwrap();
            assert_eq!(row["tool"], "generative_arbitration");
            serde_json::from_str(row["detail"].as_str().unwrap()).unwrap()
        })
        .collect()
}
#[tokio::test]
async fn arbitration_audit_correlates_receipts_without_content_or_invented_usage() {
    for mode in [0, 1, 3, 4] {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("audit");
        let audit = Arc::new(Mutex::new(AuditLog::open(&log).unwrap()));
        let fixture = Arc::new(Fixture {
            log: log.clone(),
            calls: AtomicUsize::new(0),
            mode,
            break_audit: None,
        });
        let provider = observe_arbitration(fixture.clone(), audit);
        assert_eq!(provider.planned_local_model_bytes(), Some(7));
        assert_eq!(provider.network_boundary(), NetworkBoundary::LocalOnly);
        let result = provider.complete(request()).await;
        assert_eq!(result.is_ok(), mode < 2);
        assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
        let saved = rows(&log);
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[0]["stage"], "generative_arbitration");
        assert_eq!(saved[0]["request_id"], saved[1]["request_id"]);
        assert_eq!(
            saved[1]["scope"],
            "provider_response_not_selection_validation"
        );
        assert_eq!(saved[1]["usage_missing"], mode == 1);
        if mode == 3 {
            assert_eq!(saved[1]["error_category"], "auth");
            assert!(saved[1]["usage"].is_null());
            assert_eq!(saved[1]["response_received"], false);
        } else if mode == 4 {
            assert_eq!(saved[1]["error_category"], "decode");
            assert_eq!(saved[1]["usage"]["input_tokens"], 9);
        } else if mode == 0 {
            assert_eq!(saved[1]["usage"]["input_tokens"], 3);
        } else {
            assert!(saved[1]["usage"].is_null());
        }
    }
}
#[tokio::test]
async fn arbitration_cancel_keeps_unmatched_start_and_never_retries() {
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("audit");
    let audit = Arc::new(Mutex::new(AuditLog::open(&log).unwrap()));
    let fixture = Arc::new(Fixture {
        log: log.clone(),
        calls: AtomicUsize::new(0),
        mode: 2,
        break_audit: None,
    });
    let provider = observe_arbitration(fixture.clone(), audit);
    let mut future = Box::pin(provider.complete(request()));
    assert!(futures_util::poll!(future.as_mut()).is_pending());
    drop(future);
    assert_eq!(rows(&log).len(), 1);
    assert_eq!(fixture.calls.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn arbitration_audit_failure_refuses_dispatch_or_retains_completed_usage() {
    for completion_failure in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("audit");
        let mut audit = AuditLog::open(&log).unwrap();
        if !completion_failure {
            audit.file = std::fs::File::open(&log).unwrap();
        }
        let audit = Arc::new(Mutex::new(audit));
        let fixture = Arc::new(Fixture {
            log: log.clone(),
            calls: AtomicUsize::new(0),
            mode: 0,
            break_audit: completion_failure.then_some(audit.clone()),
        });
        let meter = polaris_provider::UsageMeter::default();
        let provider = meter.wrap(observe_arbitration(fixture.clone(), audit));
        assert!(provider.complete(request()).await.is_err());
        assert_eq!(
            fixture.calls.load(Ordering::SeqCst),
            usize::from(completion_failure)
        );
        let usage = meter.snapshot();
        assert_eq!(
            usage.usage.input_tokens,
            if completion_failure { 3 } else { 0 }
        );
        assert_eq!(usage.reported_responses, u64::from(completion_failure));
        assert_eq!(rows(&log).len(), usize::from(completion_failure));
    }
}

#[tokio::test]
async fn arbitration_preserves_each_turn_hook_and_unknown_model_identity() {
    struct Hooks;
    #[async_trait::async_trait]
    impl Provider for Hooks {
        async fn complete(&self, _: CompletionRequest) -> Result {
            Ok(CompletionResponse {
                text: "plain".into(),
                ..Default::default()
            })
        }
        async fn complete_in_turn(&self, _: CompletionRequest, _: &TurnContext) -> Result {
            Ok(CompletionResponse {
                text: "turn".into(),
                ..Default::default()
            })
        }
        async fn complete_in_turn_with_observer(
            &self,
            _: CompletionRequest,
            _: &TurnContext,
            observer: Option<&dyn ResponseObserver>,
        ) -> Result {
            assert!(observer.is_none());
            Ok(CompletionResponse {
                text: "observed-none".into(),
                ..Default::default()
            })
        }
    }
    let root = tempfile::tempdir().unwrap();
    let log = root.path().join("audit");
    let provider = observe_arbitration(
        Arc::new(Hooks),
        Arc::new(Mutex::new(AuditLog::open(&log).unwrap())),
    );
    let turn = TurnContext::new();
    assert_eq!(provider.complete(request()).await.unwrap().text, "plain");
    assert_eq!(
        provider
            .complete_in_turn(request(), &turn)
            .await
            .unwrap()
            .text,
        "turn"
    );
    assert_eq!(
        provider
            .complete_in_turn_with_observer(request(), &turn, None)
            .await
            .unwrap()
            .text,
        "observed-none"
    );
    let saved = rows(&log);
    assert_eq!(saved.len(), 6);
    assert!(saved.iter().all(|row| row["requested_model"].is_null()));
}

#[tokio::test]
async fn arbitration_observer_cannot_publish_before_settlement_or_after_audit_failure() {
    struct Eager {
        log: PathBuf,
        audit: Arc<Mutex<AuditLog>>,
        fail: bool,
    }
    #[async_trait::async_trait]
    impl Provider for Eager {
        async fn complete(&self, _: CompletionRequest) -> Result {
            panic!("wrong hook")
        }
        async fn complete_in_turn_with_observer(
            &self,
            _: CompletionRequest,
            _: &TurnContext,
            observer: Option<&dyn ResponseObserver>,
        ) -> Result {
            if let Some(observer) = observer {
                observer
                    .try_emit(polaris_provider::ResponseEvent::FinalText {
                        text: "after-audit",
                        delta_matches: None,
                    })
                    .unwrap();
            }
            if self.fail {
                self.audit.lock().await.file = std::fs::File::open(&self.log).unwrap();
            }
            Ok(CompletionResponse {
                text: "after-audit".into(),
                ..Default::default()
            })
        }
    }
    struct Published {
        log: PathBuf,
        count: AtomicUsize,
    }
    impl ResponseObserver for Published {
        fn try_emit(
            &self,
            event: polaris_provider::ResponseEvent<'_>,
        ) -> std::result::Result<(), ObserverError> {
            assert!(
                std::fs::read_to_string(&self.log)
                    .unwrap()
                    .contains("settled"),
                "publication preceded durable settlement"
            );
            assert!(matches!(
                event,
                polaris_provider::ResponseEvent::FinalText {
                    text: "after-audit",
                    ..
                }
            ));
            self.count.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }
    for fail in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let log = root.path().join("audit");
        let audit = Arc::new(Mutex::new(AuditLog::open(&log).unwrap()));
        let provider = observe_arbitration(
            Arc::new(Eager {
                log: log.clone(),
                audit: audit.clone(),
                fail,
            }),
            audit,
        );
        let published = Published {
            log,
            count: AtomicUsize::new(0),
        };
        let response = provider
            .complete_in_turn_with_observer(request(), &TurnContext::new(), Some(&published))
            .await;
        assert_eq!(response.is_ok(), !fail);
        assert_eq!(published.count.load(Ordering::SeqCst), usize::from(!fail));
    }
}
