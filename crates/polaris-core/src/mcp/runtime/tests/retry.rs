//! Retry only pre-operation metadata failures, preserving cancellation and bounds.
use super::*;

#[cfg(unix)]
async fn failed_server(
    method: &'static str,
    statuses: Vec<u16>,
) -> (
    Server,
    Arc<std::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    fixture::server_fixture_with_failures(
        "",
        json!({"content":[]}),
        json!([{"name":"fixture","inputSchema":{"type":"object"}}]),
        method,
        statuses,
    )
    .await
}

#[cfg(unix)]
#[tokio::test]
async fn metadata_retry_recovers_discovery_and_catalog_on_a_new_connection() {
    for method in ["server/discover", "tools/list"] {
        let (server, methods, fixture) = failed_server(method, vec![503]).await;
        let (_, _owner, port) = owner();
        let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
        let result = runtime.ensure_catalog("fixture", pending()).await;
        let found = result
            .is_ok()
            .then(|| runtime.candidates("fixture", None, "implement"));
        runtime.shutdown().await.unwrap();
        fixture.abort();
        let _ = fixture.await;
        assert!(result.is_ok(), "metadata retry failed: {result:?}");
        assert_eq!(found.unwrap().unwrap().len(), 1);
        let calls = methods.lock().unwrap();
        assert_eq!(calls.iter().filter(|m| m.as_str() == method).count(), 2);
        assert_eq!(
            calls
                .iter()
                .filter(|m| m.as_str() == "server/discover")
                .count(),
            2
        );
        assert!(!calls.iter().any(|m| m == "tools/call"));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn metadata_auth_failure_is_never_classified_as_retryable_transport() {
    for method in ["server/discover", "tools/list"] {
        for status in [401, 403] {
            let (server, methods, fixture) = failed_server(method, vec![status]).await;
            let (_, _owner, port) = owner();
            let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
            let result = runtime.ensure_catalog("fixture", pending()).await;
            runtime.shutdown().await.unwrap();
            fixture.abort();
            let _ = fixture.await;
            assert!(
                matches!(result, Err(Error::Transport(polaris_mcp::Error::Auth))),
                "{result:?}"
            );
            assert_eq!(
                methods
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|m| m.as_str() == method)
                    .count(),
                1
            );
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn metadata_retry_limit_is_shared_by_all_attempts_and_not_reset_by_a_new_search() {
    for retries in [0, 2, 5] {
        let (mut server, methods, fixture) = failed_server("server/discover", vec![503; 8]).await;
        server.timeouts.connect_retries = retries;
        let (_, _owner, port) = owner();
        let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
        let mut delays = Vec::new();
        assert!(
            runtime
                .ensure_catalog_with_delay("fixture", pending(), |retry| {
                    delays.push(retry);
                    Duration::ZERO
                })
                .await
                .is_err()
        );
        let sent = methods.lock().unwrap().len();
        assert!(runtime.ensure_catalog("fixture", pending()).await.is_err());
        runtime.shutdown().await.unwrap();
        fixture.abort();
        let _ = fixture.await;
        assert_eq!(sent, usize::from(retries) + 1);
        assert_eq!(delays, (0..retries).collect::<Vec<_>>());
        assert_eq!(methods.lock().unwrap().len(), sent);
    }
}

#[test]
fn metadata_retry_clock_has_bounded_backoff_and_injectable_jitter() {
    for (retry, millis) in [250, 1000, 2000, 4000, 4000].into_iter().enumerate() {
        assert_eq!(
            metadata::delay_with_jitter(retry as u8, Duration::ZERO),
            Duration::from_millis(millis)
        );
        assert_eq!(
            metadata::delay_with_jitter(retry as u8, Duration::from_secs(9)),
            Duration::from_millis((millis + 50).min(4000))
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn metadata_retry_cancel_or_abandon_backoff_never_releases_a_fresh_attempt() {
    for abandon in [false, true] {
        let (server, methods, fixture) = failed_server("server/discover", vec![503]).await;
        let (_, mut owner, port) = owner();
        let other = port.clone();
        let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
        let entered = Arc::new(AtomicBool::new(false));
        let seen = entered.clone();
        let mut request =
            Box::pin(
                runtime.ensure_catalog_with_delay("fixture", pending(), move |_| {
                    seen.store(true, std::sync::atomic::Ordering::Release);
                    Duration::from_secs(4)
                }),
            );
        tokio::time::timeout(Duration::from_secs(3), async {
            while !entered.load(std::sync::atomic::Ordering::Acquire) {
                assert!(request.as_mut().now_or_never().is_none());
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        assert!(!owner.quiescent());
        assert!(other.reserve_metadata().is_err());
        if abandon {
            drop(request);
        } else {
            owner.cancel();
            assert!(matches!(
                request.await,
                Err(Error::Owner(McpCallError::Cancelled))
            ));
        }
        assert!(owner.quiescent());
        assert_eq!(runtime.connections.len(), 1);
        assert!(runtime.connections[0].phase == Phase::Closing);
        assert!(runtime.ensure_catalog("fixture", pending()).await.is_err());
        runtime.shutdown().await.unwrap();
        fixture.abort();
        let _ = fixture.await;
        assert_eq!(
            methods
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.as_str() == "server/discover")
                .count(),
            1
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn metadata_retry_rejects_protocol_failures_without_changing_lifecycle() {
    for method in ["server/discover", "tools/list"] {
        let (server, methods, fixture) = failed_server(method, vec![400]).await;
        let (_, _owner, port) = owner();
        let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
        let result = runtime.ensure_catalog("fixture", pending()).await;
        runtime.shutdown().await.unwrap();
        fixture.abort();
        let _ = fixture.await;
        assert!(
            matches!(result, Err(Error::Transport(polaris_mcp::Error::Protocol))),
            "{result:?}"
        );
        assert_eq!(
            methods
                .lock()
                .unwrap()
                .iter()
                .filter(|m| m.as_str() == method)
                .count(),
            1
        );
    }
}
