//! Catalog acquisition limits survive eviction, refresh, failure and cancellation.
use super::*;

fn large_tools() -> serde_json::Value {
    json!(
        (0..5)
            .map(|i| json!({
                "name":format!("tool-{i}"),
                "inputSchema":{"type":"object","description":"x".repeat(185000)}
            }))
            .collect::<Vec<_>>()
    )
}

#[tokio::test]
async fn catalog_run_budget_is_not_refunded_by_connection_eviction() {
    let (server, methods, fixture) =
        fixture::server_fixture_with_replies("", json!({"content":[]}), large_tools()).await;
    let servers = (0..6)
        .map(|i| {
            let mut server = server.clone();
            server.server_id = format!("server-{i}");
            server
        })
        .collect();
    let (_, _owner, port) = owner();
    let mut runtime = RunRuntime::new(config(servers), port).unwrap();
    for i in 0..4 {
        runtime
            .ensure_catalog(&format!("server-{i}"), pending())
            .await
            .unwrap();
    }
    let fifth = runtime.ensure_catalog("server-4", pending()).await;
    let before = methods.lock().unwrap().len();
    let sixth = runtime.ensure_catalog("server-5", pending()).await;
    let after = methods.lock().unwrap().len();
    runtime.shutdown().await.unwrap();
    fixture.abort();
    let _ = fixture.await;
    assert!(
        matches!(fifth, Err(Error::Transport(polaris_mcp::Error::Limit))),
        "fifth={fifth:?}"
    );
    assert!(
        matches!(sixth, Err(Error::Transport(polaris_mcp::Error::Limit))),
        "sixth={sixth:?}"
    );
    assert_eq!(before, after);
}

#[tokio::test]
async fn catalog_refresh_charges_again_while_cached_metadata_is_free() {
    let (server, _, fixture) =
        fixture::server_fixture_with_replies("", json!({"content":[]}), large_tools()).await;
    let (_, _owner, port) = owner();
    let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
    runtime.ensure_catalog("fixture", pending()).await.unwrap();
    let bytes = runtime.catalog_budget.bytes_received();
    assert!(bytes > 900000);
    runtime.ensure_catalog("fixture", pending()).await.unwrap();
    assert_eq!(runtime.catalog_budget.bytes_received(), bytes);
    let first = runtime.connections[0]
        .connection
        .catalog(1)
        .unwrap()
        .generation
        .clone();
    for round in 2..=4 {
        runtime.connections[0].connection.request_stop();
        runtime
            .ensure_catalog_with_delay("fixture", pending(), |_| Duration::ZERO)
            .await
            .unwrap();
        assert_eq!(runtime.catalog_budget.bytes_received(), bytes * round);
    }
    assert_ne!(
        runtime.connections[0]
            .connection
            .catalog(1)
            .unwrap()
            .generation,
        first
    );
    runtime.connections[0].connection.request_stop();
    let result = runtime
        .ensure_catalog_with_delay("fixture", pending(), |_| Duration::ZERO)
        .await;
    runtime.shutdown().await.unwrap();
    fixture.abort();
    let _ = fixture.await;
    assert!(matches!(
        result,
        Err(Error::Transport(polaris_mcp::Error::Limit))
    ));
    assert!(runtime.catalog_budget.bytes_received() > polaris_mcp::catalog::MAX_RUN_CATALOG_BYTES);
}

#[tokio::test]
async fn catalog_pages_before_failed_retry_or_abandonment_stay_charged() {
    for abandon in [false, true] {
        let (server, methods, fixture) = fixture::server_fixture_with_failures(
            if abandon { "resources/list" } else { "" },
            json!({"content":[]}),
            large_tools(),
            "resources/list",
            vec![503; 3],
        )
        .await;
        let (_, _owner, port) = owner();
        let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
        if abandon {
            let mut request = Box::pin(runtime.ensure_catalog("fixture", pending()));
            tokio::time::timeout(Duration::from_secs(3), async {
                loop {
                    assert!(request.as_mut().now_or_never().is_none());
                    if methods
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|m| m == "resources/list")
                    {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
            })
            .await
            .unwrap();
            drop(request);
            assert!(runtime.catalog_budget.bytes_received() > 900000);
            assert!(runtime.catalog_budget.bytes_received() < 1000000);
        } else {
            assert!(
                runtime
                    .ensure_catalog_with_delay("fixture", pending(), |_| Duration::ZERO)
                    .await
                    .is_err()
            );
            assert!(runtime.catalog_budget.bytes_received() > 2700000);
            assert!(runtime.catalog_budget.bytes_received() < 3000000);
        }
        let charged = runtime.catalog_budget.bytes_received();
        assert!(runtime.ensure_catalog("fixture", pending()).await.is_err());
        runtime.shutdown().await.unwrap();
        assert_eq!(runtime.catalog_budget.bytes_received(), charged);
        fixture.abort();
        let _ = fixture.await;
    }
}
