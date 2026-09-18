//! Host prompt preparation blocks request snapshots until durable confirmation.
use super::*;
use polaris_desktop_protocol::request::McpPromptChoice;

fn choice() -> McpPromptChoice {
    McpPromptChoice {
        server: "fixture".into(),
        prompt: "prompt".into(),
        arguments: Default::default(),
    }
}

#[tokio::test]
async fn history_lane_runtime_gets_owner_query_without_connecting_and_rejects_after_shutdown() {
    let (target, mut owner, port) = owner();
    let (_root, mut writer) = store(&target);
    let mut runtime = RunRuntime::new(config(vec![]), port).unwrap();
    let before = writer.snapshot().unwrap();
    let mut history = Box::pin(runtime.request_history(pending()));
    assert!(history.as_mut().now_or_never().is_none());
    owner.step(&mut writer, &target).unwrap();
    let result = history.as_mut().now_or_never().unwrap().unwrap();
    drop(history);
    assert_eq!(result.query(), before.state.runs[0].input.text);
    assert_eq!(result.target(), &target);
    assert_eq!(result.marker(), &before.marker);
    assert!(runtime.connections.is_empty());
    assert_eq!(writer.snapshot().unwrap().marker, before.marker);
    runtime.shutdown().await.unwrap();
    assert!(matches!(
        runtime.request_history(pending()).await,
        Err(Error::Owner(McpCallError::Cancelled))
    ));
}

#[tokio::test]
async fn host_prompt_runtime_discovers_then_uses_the_single_owner_lane_once() {
    let (server, methods, fixture) = fixture::server_fixture("").await;
    let (target, mut owner, port) =
        owner_with_policy(ExternalCeiling::Denied, ApprovalPolicy::Never);
    let (_root, mut writer) = store_with_prompt(&target, Some(choice()));
    let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
    let selection = choice();
    let mut get = Box::pin(runtime.host_prompt(&selection, pending()));
    let result = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            if let Some(result) = get.as_mut().now_or_never() {
                break result.unwrap();
            }
            if methods
                .lock()
                .unwrap()
                .iter()
                .any(|method| method == "prompts/get")
            {
                assert!(
                    writer.snapshot().unwrap().state.runs[0].operations[0]
                        .prompt
                        .is_some()
                );
            }
            owner.step(&mut writer, &target).unwrap();
            assert!(!owner.awaiting_approval());
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    drop(get);
    assert!(result.permits_continuation());
    assert_eq!(runtime.prompt, PromptState::Confirmed);
    assert!(runtime.freeze().is_ok());
    let saved = writer.snapshot().unwrap();
    assert_eq!(saved.raw.len(), 2);
    assert_eq!(
        serde_json::to_value(&saved.raw[1].message).unwrap(),
        serde_json::to_value(&result.message).unwrap()
    );
    assert_eq!(
        methods
            .lock()
            .unwrap()
            .iter()
            .filter(|method| *method == "prompts/get")
            .count(),
        1
    );
    let count = methods.lock().unwrap().len();
    assert!(runtime.host_prompt(&selection, pending()).await.is_err());
    assert_eq!(methods.lock().unwrap().len(), count);
    runtime.shutdown().await.unwrap();
    assert!(runtime.connections.is_empty());
    fixture.abort();
    let _ = fixture.await;
}

#[tokio::test]
async fn host_prompt_runtime_cancelled_metadata_keeps_ownership_and_blocks_requests() {
    let (server, methods, fixture) = fixture::server_fixture("server/discover").await;
    let (target, mut owner, port) = owner();
    let (_root, mut writer) = store_with_prompt(&target, Some(choice()));
    let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
    let selection = choice();
    let mut get = Box::pin(runtime.host_prompt(&selection, pending()));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            assert!(get.as_mut().now_or_never().is_none());
            if methods
                .lock()
                .unwrap()
                .iter()
                .any(|method| method == "server/discover")
            {
                break;
            }
            owner.step(&mut writer, &target).unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    drop(get);
    assert_eq!(runtime.connections.len(), 1);
    assert!(owner.quiescent());
    assert!(
        writer.snapshot().unwrap().state.runs[0]
            .operations
            .is_empty()
    );
    assert!(runtime.freeze().is_err());
    assert!(matches!(
        runtime.request_history(pending()).await,
        Err(Error::Owner(McpCallError::Storage))
    ));
    assert!(owner.quiescent());
    let count = methods.lock().unwrap().len();
    assert!(runtime.host_prompt(&selection, pending()).await.is_err());
    assert_eq!(methods.lock().unwrap().len(), count);
    assert!(
        !methods
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "prompts/get")
    );
    runtime.shutdown().await.unwrap();
    assert!(runtime.connections.is_empty());
    fixture.abort();
    let _ = fixture.await;
}

#[tokio::test]
async fn host_prompt_runtime_disabled_capability_stops_before_get_and_freeze() {
    let (mut server, methods, fixture) = fixture::server_fixture("").await;
    server.capabilities.prompt_selection = false;
    let (_, _owner, port) = owner();
    let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
    let selection = choice();
    assert!(matches!(
        runtime.host_prompt(&selection, pending()).await,
        Err(Error::Transport(polaris_mcp::Error::Auth))
    ));
    assert!(runtime.freeze().is_err());
    assert!(
        !methods
            .lock()
            .unwrap()
            .iter()
            .any(|method| method == "prompts/get")
    );
    let count = methods.lock().unwrap().len();
    assert!(runtime.host_prompt(&selection, pending()).await.is_err());
    assert_eq!(methods.lock().unwrap().len(), count);
    runtime.shutdown().await.unwrap();
    fixture.abort();
    let _ = fixture.await;
}

#[tokio::test]
async fn host_prompt_runtime_late_selection_has_no_network_effect() {
    let (server, methods, fixture) = fixture::server_fixture("").await;
    let (_, _owner, port) = owner();
    let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
    assert!(runtime.freeze().is_ok());
    assert!(runtime.host_prompt(&choice(), pending()).await.is_err());
    assert!(methods.lock().unwrap().is_empty());
    assert!(runtime.freeze().is_ok());
    runtime.shutdown().await.unwrap();
    fixture.abort();
    let _ = fixture.await;
}
