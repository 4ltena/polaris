//! Synthetic MCP operation and resource-template regression checks.

use super::super::tests::{OperationFixture, operation_fixture, secrets, server};
use super::*;
use serde_json::json;
use std::collections::BTreeMap;

async fn ready(
    reply: Value,
    delay: bool,
    disconnect: bool,
) -> (
    Connection,
    Arc<std::sync::Mutex<Vec<String>>>,
    tokio::task::JoinHandle<()>,
) {
    let (url, calls, task) = operation_fixture(OperationFixture {
        reply,
        delay,
        disconnect,
        tools: None,
    })
    .await;
    let mut server = server(
        Destination::StreamableHttp {
            url,
            secret_headers: BTreeMap::new(),
        },
        ProtocolProfile::Discover2026,
    );
    server.capabilities.tool_call = true;
    server.capabilities.resource_read = true;
    server.capabilities.prompt_selection = true;
    server.timeouts.operation_seconds = 1;
    let mut connection =
        Connection::prepare(&server, "project", &secrets(), None, || true).unwrap();
    connection
        .initialize(std::future::pending(), || true)
        .await
        .unwrap();
    connection
        .refresh_catalog(1, std::future::pending(), || true)
        .await
        .unwrap();
    (connection, calls, task)
}

fn operation(connection: &Connection, kind: Kind) -> BoundOperation {
    let catalog = connection.catalog(1).unwrap();
    if kind == Kind::Prompt {
        connection
            .host_select_prompt(1, &catalog.generation, "prompt", json!({}))
            .unwrap()
    } else {
        let item = catalog
            .items
            .iter()
            .find(|item| item.id.kind == kind)
            .unwrap();
        connection
            .prepare_operation(1, &catalog.generation, &item.id, json!({}))
            .unwrap()
    }
}

#[tokio::test]
async fn operation_authorization_and_staleness_reject_before_any_call() {
    let (mut connection, calls, task) = ready(
        json!({"content":[{"type":"text","text":"done"}]}),
        false,
        false,
    )
    .await;
    let denied = operation(&connection, Kind::Tool);
    let denied_progress = denied.progress();
    let binding = denied.binding().clone();
    assert_eq!(binding.item.key, "fixture");
    assert_eq!(binding.arguments_hash, canonical_hash(&json!({})).unwrap());
    assert!(matches!(
        connection
            .perform_once(denied, std::future::pending(), |_| false)
            .await,
        Err(OperationFailure::BeforeSend(Error::Auth))
    ));
    assert!(!denied_progress.may_have_sent());
    let cancelled = operation(&connection, Kind::Tool);
    let cancelled_progress = cancelled.progress();
    assert!(matches!(
        connection
            .perform_once(cancelled, std::future::ready(()), |_| true)
            .await,
        Err(OperationFailure::BeforeSend(Error::Cancelled))
    ));
    assert!(!cancelled_progress.may_have_sent());
    let stale = operation(&connection, Kind::Tool);
    let stale_progress = stale.progress();
    connection
        .handler
        .epoch
        .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
    assert!(matches!(
        connection
            .perform_once(stale, std::future::pending(), |_| true)
            .await,
        Err(OperationFailure::BeforeSend(_))
    ));
    assert!(!stale_progress.may_have_sent());
    assert!(
        !calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "tools/call")
    );
    connection
        .refresh_catalog(1, std::future::pending(), || true)
        .await
        .unwrap();
    let valid = operation(&connection, Kind::Tool);
    let valid_progress = valid.progress();
    assert!(!valid_progress.may_have_sent());
    let reply = connection
        .perform_once(valid, std::future::pending(), |seen| {
            seen.item.key == "fixture"
        })
        .await
        .unwrap();
    assert_eq!(reply.status, Status::Complete);
    assert!(valid_progress.may_have_sent());
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|m| *m == "tools/call")
            .count(),
        1
    );
    connection.shutdown().await.unwrap();
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn unpolled_operation_drop_has_no_send_and_keeps_connection_usable() {
    let (mut connection, calls, task) = ready(json!({"content":[]}), false, false).await;
    let bound = operation(&connection, Kind::Tool);
    let progress = bound.progress();
    drop(connection.perform_once(bound, std::future::pending(), |_| true));
    assert!(!progress.may_have_sent());
    assert!(connection.is_connected());
    assert!(!calls.lock().unwrap().iter().any(|m| m == "tools/call"));

    let next = operation(&connection, Kind::Tool);
    let next_progress = next.progress();
    connection
        .perform_once(next, std::future::pending(), |_| true)
        .await
        .unwrap();
    connection.shutdown().await.unwrap();
    assert!(!progress.may_have_sent());
    assert!(next_progress.may_have_sent());
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|m| *m == "tools/call")
            .count(),
        1
    );
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn cancellation_after_preflight_before_request_poll_keeps_progress_unsent() {
    let (mut connection, calls, task) = ready(json!({"content":[]}), false, false).await;
    let bound = operation(&connection, Kind::Tool);
    let progress = bound.progress();
    let polls = std::cell::Cell::new(0);
    let cancelled = std::future::poll_fn(|_| {
        polls.set(polls.get() + 1);
        if polls.get() == 1 {
            std::task::Poll::Pending
        } else {
            std::task::Poll::Ready(())
        }
    });
    assert!(matches!(
        connection.perform_once(bound, cancelled, |_| true).await,
        Err(OperationFailure::OutcomeUnknown(Error::Cancelled))
    ));
    assert_eq!(polls.get(), 2);
    assert!(!connection.is_connected());
    connection.shutdown().await.unwrap();
    assert!(!progress.may_have_sent());
    assert!(!calls.lock().unwrap().iter().any(|m| m == "tools/call"));
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn incomplete_all_operation_kinds_stop_without_input_responses_or_replay() {
    for kind in [Kind::Tool, Kind::Resource, Kind::Prompt] {
        let (mut connection, calls, task) = ready(
            json!({"resultType":"input_required","inputRequests":{},"requestState":"one-shot"}),
            false,
            false,
        )
        .await;
        let operation = operation(&connection, kind);
        let progress = operation.progress();
        assert_eq!(
            operation.binding.selection_id.is_some(),
            kind == Kind::Prompt
        );
        let reply = connection
            .perform_once(operation, std::future::pending(), |_| true)
            .await
            .unwrap();
        assert_eq!(reply.status, Status::InputRequired);
        assert!(!connection.is_connected());
        connection.shutdown().await.unwrap();
        assert!(progress.may_have_sent());
        let method = match kind {
            Kind::Tool => "tools/call",
            Kind::Resource => "resources/read",
            _ => "prompts/get",
        };
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|m| *m == method)
                .count(),
            1
        );
        task.abort();
        let _ = task.await;
    }
}

#[tokio::test]
async fn disconnected_timed_out_and_dropped_calls_quarantine_without_replay() {
    for (delay, disconnect, drop_wait) in [
        (false, true, false),
        (true, false, false),
        (true, false, true),
    ] {
        let (mut connection, calls, task) = ready(json!({"content":[]}), delay, disconnect).await;
        let operation = operation(&connection, Kind::Tool);
        let progress = operation.progress();
        if drop_wait {
            assert!(
                tokio::time::timeout(
                    Duration::from_millis(100),
                    connection.perform_once(operation, std::future::pending(), |_| true)
                )
                .await
                .is_err()
            );
        } else {
            assert!(matches!(
                connection
                    .perform_once(operation, std::future::pending(), |_| true)
                    .await,
                Err(OperationFailure::OutcomeUnknown(_))
            ));
        }
        assert!(!connection.is_connected());
        connection.shutdown().await.unwrap();
        assert!(progress.may_have_sent());
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|m| *m == "tools/call")
                .count(),
            1
        );
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|m| *m == "server/discover")
                .count(),
            1
        );
        task.abort();
        let _ = task.await;
    }
}

#[tokio::test]
async fn decoded_binary_secret_and_cross_connection_binding_are_rejected() {
    let (mut connection, calls, task) = ready(
        json!({"content":[{"type":"image","data":"Zml4dHVyZS1zZWNyZXQ=","mimeType":"image/png"}]}),
        false,
        false,
    )
    .await;
    connection.sensitive = sensitive::SensitiveValues::new(["fixture-secret".into()]);
    let operation = operation(&connection, Kind::Tool);
    let (mut other, other_calls, other_task) = ready(json!({"content":[]}), false, false).await;
    assert!(matches!(
        other
            .perform_once(operation, std::future::pending(), |_| true)
            .await,
        Err(OperationFailure::BeforeSend(_))
    ));
    assert!(
        !other_calls
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "tools/call")
    );
    other.shutdown().await.unwrap();
    other_task.abort();
    let _ = other_task.await;
    let operation = self::operation(&connection, Kind::Tool);
    assert!(matches!(
        connection
            .perform_once(operation, std::future::pending(), |_| true)
            .await,
        Err(OperationFailure::OutcomeUnknown(Error::Auth))
    ));
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|m| *m == "tools/call")
            .count(),
        1
    );
    connection.shutdown().await.unwrap();
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn observed_sent_call_then_cancellation_is_unknown_and_never_replayed() {
    let (mut connection, calls, task) = ready(json!({"content":[]}), true, false).await;
    let operation = operation(&connection, Kind::Tool);
    let progress = operation.progress();
    let observed = calls.clone();
    let (cancel_tx, cancel_rx) = tokio::sync::oneshot::channel();
    let watcher = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if observed
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|method| method == "tools/call")
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        cancel_tx.send(()).unwrap();
    });
    let result = connection
        .perform_once(
            operation,
            async {
                cancel_rx.await.unwrap();
            },
            |_| true,
        )
        .await;
    assert!(matches!(
        result,
        Err(OperationFailure::OutcomeUnknown(Error::Cancelled))
    ));
    watcher.await.unwrap();
    assert!(!connection.is_connected());
    connection.shutdown().await.unwrap();
    assert!(progress.may_have_sent());
    for method in ["tools/call", "server/discover"] {
        assert_eq!(
            calls
                .lock()
                .unwrap()
                .iter()
                .filter(|call| call.as_str() == method)
                .count(),
            1
        );
    }
    task.abort();
    let _ = task.await;
}

#[tokio::test]
async fn completed_resource_and_host_selected_prompt_preserve_content_and_read_capability() {
    for (kind, wire) in [
        (
            Kind::Resource,
            json!({"contents":[{"uri":"fixture://resource","text":"original"}]}),
        ),
        (
            Kind::Prompt,
            json!({"messages":[{"role":"user","content":{"type":"text","text":"selected"}}]}),
        ),
    ] {
        let (mut connection, calls, task) = ready(wire, false, false).await;
        let bound = operation(&connection, kind);
        let progress = bound.progress();
        if kind == Kind::Prompt {
            assert!(
                connection
                    .prepare_operation(
                        1,
                        &bound.binding.catalog_generation,
                        &bound.binding.item,
                        json!({})
                    )
                    .is_err()
            );
        }
        let reply = connection
            .perform_once(bound, std::future::pending(), |_| true)
            .await
            .unwrap();
        assert_eq!(reply.status, Status::Complete);
        assert!(progress.may_have_sent());
        assert!(matches!(reply.parts.first(),Some(Part::Text {text,..}) if !text.is_empty()));
        assert!(
            !calls
                .lock()
                .unwrap()
                .iter()
                .any(|method| method == "tools/call")
        );
        connection.server.capabilities.resource_read = false;
        connection.server.capabilities.prompt_selection = false;
        let catalog = connection.catalog(1).unwrap();
        let id = catalog
            .items
            .iter()
            .find(|item| item.id.kind == kind)
            .unwrap()
            .id
            .clone();
        assert!(
            connection
                .bind_operation(1, &catalog.generation, &id, json!({}), Some("host".into()))
                .is_err()
        );
        connection.shutdown().await.unwrap();
        task.abort();
        let _ = task.await;
    }
}

#[tokio::test]
async fn resource_template_binds_the_expanded_uri_without_changing_destination() {
    let (mut connection,calls,task) = ready(json!({"contents":[{"uri":"fixture://resource/%E6%97%A5%E6%9C%AC?lang=ja","text":"original"}]}),false,false).await;
    let catalog = connection.catalog(1).unwrap();
    let item = catalog
        .items
        .iter()
        .find(|item| item.id.kind == Kind::ResourceTemplate)
        .unwrap();
    let bound = connection
        .prepare_operation(
            1,
            &catalog.generation,
            &item.id,
            json!({"name":"日本","lang":"ja"}),
        )
        .unwrap();
    assert_eq!(
        bound.binding.resource_uri.as_deref(),
        Some("fixture://resource/%E6%97%A5%E6%9C%AC?lang=ja")
    );
    assert_eq!(
        bound.binding.destination_hash,
        connection.server.destination_hash().unwrap()
    );
    let reply = connection
        .perform_once(bound, std::future::pending(), |_| true)
        .await
        .unwrap();
    assert_eq!(reply.status, Status::Complete);
    assert_eq!(
        calls
            .lock()
            .unwrap()
            .iter()
            .filter(|method| *method == "resources/read")
            .count(),
        1
    );
    assert!(
        !calls
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "tools/call")
    );
    connection.shutdown().await.unwrap();
    task.abort();
    let _ = task.await;
}
