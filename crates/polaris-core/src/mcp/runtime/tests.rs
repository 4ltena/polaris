//! Loopback runtime ownership, bounded catalog selection and durable dispatch.
use super::test_fixture as fixture;
use super::*;
use crate::{
    approval::ApprovalPolicy, execution_owner::RunExecution, mcp::operation::ExternalCeiling,
};
use polaris_desktop_protocol::{ids::*, request::RunTarget};
use polaris_mcp::catalog::Kind;
use serde_json::json;
use std::{future::pending, sync::atomic::AtomicBool, time::Duration};
mod budget;
mod prompt;
mod retry;

fn read_call(runtime: &RunRuntime, template: bool) -> ToolCall {
    let entry = &runtime.connections[0];
    let catalog = entry.connection.catalog(1).unwrap();
    ToolCall {
        id: "resource-call".into(),
        name: "mcp_catalog".into(),
        arguments: json!({
            "op":"read", "server":"fixture", "kind":if template { "resource_template" } else { "resource" },
            "key":if template { "memory://note/{id}" } else { "memory://note" },
            "generation":catalog.generation,
            "arguments":if template { json!({"id":"blue"}) } else { json!({}) }
        }),
    }
}

#[tokio::test]
async fn resource_and_template_read_use_capability_without_a_tool_approval_and_reopen_once() {
    for template in [false, true] {
        let (server, methods, fixture) = fixture::server_fixture("").await;
        let (target, mut owner, port) =
            owner_with_policy(ExternalCeiling::Denied, ApprovalPolicy::Never);
        let (root, mut writer) = store(&target);
        let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
        runtime.ensure_catalog("fixture", pending()).await.unwrap();
        let request = runtime.freeze().unwrap();
        let call = read_call(&runtime, template);
        let suffix = vec![Message::assistant_with_tool_calls("", vec![call.clone()])];
        let mut read = Box::pin(runtime.read_resource(&request, &call, suffix, pending()));
        assert!(read.as_mut().now_or_never().is_none());
        owner.step(&mut writer, &target).unwrap();
        assert!(!owner.awaiting_approval());
        assert!(
            writer.snapshot().unwrap().state.runs[0]
                .operations
                .is_empty()
        );
        assert!(
            !methods
                .lock()
                .unwrap()
                .iter()
                .any(|m| m == "resources/read")
        );
        owner.step(&mut writer, &target).unwrap();
        assert_eq!(writer.snapshot().unwrap().state.runs[0].operations.len(), 1);
        let result = tokio::time::timeout(Duration::from_secs(4), async {
            loop {
                owner.step(&mut writer, &target).unwrap();
                if let Some(result) = read.as_mut().now_or_never() {
                    break result.unwrap();
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        })
        .await
        .unwrap();
        drop(read);
        assert!(result.permits_continuation());
        let saved = writer.snapshot().unwrap();
        assert!(
            saved.state.approval_records.is_empty() && saved.state.unresolved_approvals.is_empty()
        );
        assert_eq!(saved.raw.len(), 3);
        assert_eq!(
            serde_json::to_value(&saved.raw[2].message).unwrap(),
            serde_json::to_value(&result.message).unwrap()
        );
        let body: serde_json::Value = serde_json::from_str(&result.message.content).unwrap();
        assert_eq!(
            body["intent"]["resource"]["uri"],
            if template {
                "memory://note/blue"
            } else {
                "memory://note"
            }
        );
        runtime.shutdown().await.unwrap();
        drop(writer);
        let reopened = root
            .open(
                &ProjectId::new("project").unwrap(),
                &SessionId::new("runtime-session").unwrap(),
            )
            .unwrap();
        assert_eq!(reopened.snapshot().unwrap().raw.len(), 3);
        assert_eq!(
            methods
                .lock()
                .unwrap()
                .iter()
                .filter(|m| *m == "resources/read")
                .count(),
            1
        );
        fixture.abort();
        let _ = fixture.await;
    }
}

#[tokio::test]
async fn disabled_read_capability_cannot_be_supplied_by_meta_arguments() {
    let (mut server, methods, fixture) = fixture::server_fixture("").await;
    server.capabilities.resource_read = false;
    let (_, _owner, port) = owner();
    let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
    runtime.ensure_catalog("fixture", pending()).await.unwrap();
    let request = runtime.freeze().unwrap();
    let call = read_call(&runtime, false);
    let suffix = vec![Message::assistant_with_tool_calls("", vec![call.clone()])];
    assert!(matches!(
        runtime
            .read_resource(&request, &call, suffix, pending())
            .await,
        Err(Error::Transport(polaris_mcp::Error::Auth))
    ));
    assert!(
        !methods
            .lock()
            .unwrap()
            .iter()
            .any(|m| m == "resources/read")
    );
    runtime.shutdown().await.unwrap();
    fixture.abort();
    let _ = fixture.await;
}

#[tokio::test]
async fn resource_read_drop_after_send_is_unknown_and_never_replayed() {
    use polaris_desktop_protocol::run_state::Observation;
    let (server, methods, fixture) = fixture::server_fixture("resources/read").await;
    let (target, mut owner, port) =
        owner_with_policy(ExternalCeiling::Denied, ApprovalPolicy::Never);
    let (root, mut writer) = store(&target);
    let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
    runtime.ensure_catalog("fixture", pending()).await.unwrap();
    let request = runtime.freeze().unwrap();
    let call = read_call(&runtime, false);
    let suffix = vec![Message::assistant_with_tool_calls("", vec![call.clone()])];
    let mut read = Box::pin(runtime.read_resource(&request, &call, suffix, pending()));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            assert!(read.as_mut().now_or_never().is_none());
            owner.step(&mut writer, &target).unwrap();
            if methods
                .lock()
                .unwrap()
                .iter()
                .any(|m| m == "resources/read")
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    drop(read);
    owner.step(&mut writer, &target).unwrap();
    assert!(owner.quiescent());
    assert_eq!(
        owner.observation(Observation::Cancelled),
        Observation::OutcomeUnknown
    );
    let saved = writer.snapshot().unwrap();
    assert_eq!(
        saved.state.runs[0].operations[0]
            .mcp
            .as_ref()
            .unwrap()
            .disposition(),
        Some(crate::desktop_store::McpDisposition::OutcomeUnknown)
    );
    runtime.shutdown().await.unwrap();
    drop(writer);
    let reopened = root
        .open(
            &ProjectId::new("project").unwrap(),
            &SessionId::new("runtime-session").unwrap(),
        )
        .unwrap();
    assert_eq!(reopened.snapshot().unwrap().raw.len(), 3);
    assert_eq!(
        methods
            .lock()
            .unwrap()
            .iter()
            .filter(|m| *m == "resources/read")
            .count(),
        1
    );
    fixture.abort();
    let _ = fixture.await;
}

#[tokio::test]
async fn dedicated_check_projects_one_server_catalog_without_selection_or_calls() {
    let (server, methods, fixture) = fixture::server_fixture("").await;
    let (_target, _owner, port) = owner();
    let mut runtime = RunRuntime::new(config(vec![server.clone()]), port).unwrap();
    let entry = runtime.index_entry("fixture", pending()).await.unwrap();
    assert_eq!(entry.destination_hash, server.destination_hash().unwrap());
    assert!(!entry.partial);
    assert_eq!(
        entry
            .tools
            .iter()
            .map(|t| t.key.as_str())
            .collect::<Vec<_>>(),
        ["fixture"]
    );
    assert_eq!(runtime.config().registry().revision, 1);
    assert!(runtime.freeze().unwrap().specs().is_empty());
    assert!(matches!(
        runtime.index_entry("missing", pending()).await,
        Err(Error::Transport(polaris_mcp::Error::Auth))
    ));
    runtime.shutdown().await.unwrap();
    assert!(!methods.lock().unwrap().iter().any(|m| m == "tools/call"));
    fixture.abort();
    let _ = fixture.await;
}

#[tokio::test]
async fn preselection_is_rematched_per_request_and_changed_items_are_dropped() {
    let (server, methods, fixture) = fixture::server_fixture("").await;
    let (_target, _owner, port) = owner();
    let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
    let pick = {
        let (_t, _o, port) = owner();
        let mut probe = RunRuntime::new(runtime.config().clone(), port).unwrap();
        let tool = probe.index_entry("fixture", pending()).await.unwrap().tools[0].clone();
        probe.shutdown().await.unwrap();
        crate::mcp::preselection::Pick {
            server_id: "fixture".into(),
            key: tool.key,
            summary_hash: tool.summary_hash,
            schema_hash: tool.schema_hash,
        }
    };
    let changed = crate::mcp::preselection::Pick {
        key: "fixture".into(),
        schema_hash: "b".repeat(64),
        ..pick.clone()
    };
    runtime.set_preselection(vec![changed, pick.clone()]);
    for _ in 0..2 {
        runtime.refresh_preselected(pending()).await.unwrap();
        let request = runtime.freeze().unwrap();
        assert_eq!(request.specs().len(), 1, "sticky across requests");
    }
    assert_eq!(runtime.preselected, vec![pick]);
    choose(&mut runtime, "fixture");
    assert_eq!(runtime.freeze().unwrap().specs().len(), 1);
    runtime.shutdown().await.unwrap();
    assert!(!methods.lock().unwrap().iter().any(|m| m == "tools/call"));
    fixture.abort();
    let _ = fixture.await;
}

async fn pick_for(config: &RuntimeConfig, server: &str) -> crate::mcp::preselection::Pick {
    let (_t, _o, port) = owner();
    let mut probe = RunRuntime::new(config.clone(), port).unwrap();
    let tool = probe.index_entry(server, pending()).await.unwrap().tools[0].clone();
    probe.shutdown().await.unwrap();
    crate::mcp::preselection::Pick {
        server_id: server.into(),
        key: tool.key,
        summary_hash: tool.summary_hash,
        schema_hash: tool.schema_hash,
    }
}

#[tokio::test]
async fn an_expired_unreachable_preselected_server_leaves_only_that_request() {
    let (first, _methods, first_task) = fixture::server_fixture("").await;
    let (mut second, _second_methods, second_task) = fixture::server_fixture("").await;
    second.server_id = "second".into();
    second.timeouts.connect_retries = 0;
    let config = config(vec![first, second]);
    let picks = vec![
        pick_for(&config, "fixture").await,
        pick_for(&config, "second").await,
    ];
    let (_target, _owner, port) = owner();
    let mut runtime = RunRuntime::new(config, port).unwrap();
    runtime.set_preselection(picks.clone());
    runtime.refresh_preselected(pending()).await.unwrap();
    assert_eq!(runtime.freeze().unwrap().specs().len(), 2);
    second_task.abort();
    let _ = second_task.await;
    runtime
        .connections
        .iter_mut()
        .find(|entry| entry.connection.server_id() == "second")
        .unwrap()
        .connection
        .expire_catalog_for_test();
    runtime.refresh_preselected(pending()).await.unwrap();
    assert_eq!(runtime.freeze().unwrap().specs().len(), 1);
    assert_eq!(runtime.preselected, picks, "left out for that request only");
    assert_eq!(
        runtime
            .candidates("fixture", Some("fixture"), "")
            .unwrap()
            .len(),
        1
    );
    runtime.shutdown().await.unwrap();
    first_task.abort();
    let _ = first_task.await;
}

fn owner() -> (RunTarget, RunExecution, McpPort) {
    owner_with_policy(ExternalCeiling::Allowed, ApprovalPolicy::Always)
}
fn owner_with_policy(
    ceiling: ExternalCeiling,
    policy: ApprovalPolicy,
) -> (RunTarget, RunExecution, McpPort) {
    let target = RunTarget {
        run_id: RunId::new("runtime-run").unwrap(),
        attempt_id: AttemptId::new("attempt").unwrap(),
    };
    let mut owner = RunExecution::new(&target, Arc::new(AtomicBool::new(false)));
    let port = owner
        .configure_mcp(
            &target,
            SessionId::new("runtime-session").unwrap(),
            ceiling,
            policy,
        )
        .unwrap();
    (target, owner, port)
}

fn config(servers: Vec<Server>) -> RuntimeConfig {
    RuntimeConfig::new(
        Registry {
            schema_version: 1,
            revision: 1,
            servers,
        },
        "project".into(),
        Arc::new(Secrets::new(1, BTreeMap::new()).unwrap()),
        BTreeMap::new(),
    )
    .unwrap()
}

fn choose(runtime: &mut RunRuntime, server: &str) {
    let found = runtime
        .candidates("fixture", Some(server), "implement")
        .unwrap();
    assert_eq!(found.len(), 1);
    runtime
        .select(
            &ItemId {
                server_id: server.into(),
                kind: Kind::Tool,
                key: "fixture".into(),
            },
            &found[0].generation,
        )
        .unwrap();
}

#[tokio::test]
async fn configuration_is_pure_and_rejects_unbound_or_missing_secret_before_connection() {
    let (server, methods, fixture) = fixture::server_fixture("").await;
    let (_, _owner, port) = owner();
    let mut runtime = RunRuntime::new(config(vec![server.clone()]), port).unwrap();
    assert!(methods.lock().unwrap().is_empty());
    assert!(runtime.connections.is_empty());
    assert_eq!(runtime.server_ids(), vec!["fixture"]);
    assert!(matches!(
        runtime.ensure_catalog("unregistered", pending()).await,
        Err(Error::Transport(polaris_mcp::Error::Auth))
    ));
    assert!(matches!(
        runtime
            .ensure_catalog("fixture", std::future::ready(()))
            .await,
        Err(Error::Owner(McpCallError::Cancelled))
    ));
    assert!(methods.lock().unwrap().is_empty());
    let mut secret_server = server.clone();
    if let Destination::StreamableHttp { secret_headers, .. } = &mut secret_server.transport {
        secret_headers.insert("Authorization".into(), "missing".into());
    }
    assert!(
        RuntimeConfig::new(
            Registry {
                schema_version: 1,
                revision: 1,
                servers: vec![secret_server]
            },
            "project".into(),
            Arc::new(Secrets::new(1, BTreeMap::new()).unwrap()),
            BTreeMap::new()
        )
        .is_err()
    );
    let mut disabled = server;
    disabled.enabled_projects = BTreeSet::from(["another-project".into()]);
    assert!(config(vec![disabled]).server_ids().is_empty());
    runtime.shutdown().await.unwrap();
    fixture.abort();
    let _ = fixture.await;
}

#[tokio::test]
async fn metadata_uses_the_tool_permit_and_cancelled_loading_is_never_retried() {
    let (server, methods, fixture) = fixture::server_fixture("tools/list").await;
    let (_, mut owner, port) = owner();
    let other_port = port.clone();
    let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
    let mut request = Box::pin(runtime.ensure_catalog("fixture", pending()));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            assert!(request.as_mut().now_or_never().is_none());
            if methods.lock().unwrap().iter().any(|m| m == "tools/list") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert!(!owner.quiescent());
    assert!(matches!(
        other_port.reserve_metadata(),
        Err(McpCallError::Unavailable)
    ));
    owner.cancel();
    assert!(request.await.is_err());
    assert!(owner.quiescent());
    assert_eq!(runtime.connections.len(), 1);
    assert!(runtime.connections[0].phase == Phase::Failed);
    assert!(runtime.ensure_catalog("fixture", pending()).await.is_err());
    assert_eq!(
        methods
            .lock()
            .unwrap()
            .iter()
            .filter(|m| *m == "tools/list")
            .count(),
        1
    );
    runtime.shutdown().await.unwrap();
    assert!(runtime.connections.is_empty());
    fixture.abort();
    let _ = fixture.await;
}

#[tokio::test]
async fn abandoned_initialize_retains_connection_for_outer_shutdown() {
    let (server, methods, fixture) = fixture::server_fixture("server/discover").await;
    let (_, owner, port) = owner();
    let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
    let mut request = Box::pin(runtime.ensure_catalog("fixture", pending()));
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            assert!(request.as_mut().now_or_never().is_none());
            if !methods.lock().unwrap().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    assert!(!owner.quiescent());
    drop(request);
    assert!(owner.quiescent()); // The permit is free; cleanup is still pending.
    assert_eq!(runtime.connections.len(), 1);
    assert!(runtime.connections[0].phase == Phase::Loading);
    assert!(runtime.ensure_catalog("fixture", pending()).await.is_err());
    assert_eq!(methods.lock().unwrap().len(), 1);
    runtime.shutdown().await.unwrap();
    assert!(runtime.closed && runtime.connections.is_empty());
    assert!(runtime.freeze().is_err());
    fixture.abort();
    let _ = fixture.await;
}

#[tokio::test]
async fn four_connection_limit_preserves_current_and_pending_selection_then_allows_explicit_discovery()
 {
    let (server, methods, fixture) = fixture::server_fixture("").await;
    let (_, _owner, port) = owner();
    let servers = (0..6)
        .map(|i| {
            let mut s = server.clone();
            s.server_id = format!("server-{i}");
            s
        })
        .collect();
    let mut runtime = RunRuntime::new(config(servers), port).unwrap();
    for i in 0..4 {
        let id = format!("server-{i}");
        runtime.ensure_catalog(&id, pending()).await.unwrap();
        choose(&mut runtime, &id);
    }
    let first = runtime.freeze().unwrap();
    assert_eq!(first.specs().len(), 4);
    let before = methods.lock().unwrap().len();
    assert!(matches!(
        runtime.ensure_catalog("server-4", pending()).await,
        Err(Error::Transport(polaris_mcp::Error::Limit))
    ));
    assert_eq!(methods.lock().unwrap().len(), before);
    assert_eq!(runtime.connections.len(), 4);
    assert!(runtime.freeze().unwrap().specs().is_empty());
    choose(&mut runtime, "server-0");
    runtime.ensure_catalog("server-4", pending()).await.unwrap();
    assert_eq!(runtime.connections.len(), 4);
    assert!(
        runtime
            .connections
            .iter()
            .any(|e| e.connection.server_id() == "server-0")
    );
    assert!(
        !runtime
            .connections
            .iter()
            .any(|e| e.connection.server_id() == "server-1")
    );
    let second = runtime.freeze().unwrap();
    assert_eq!(second.specs().len(), 1);
    runtime.ensure_catalog("server-5", pending()).await.unwrap();
    assert!(
        second
            .snapshot
            .resolve(
                &runtime.view().unwrap(),
                &second.specs()[0].name,
                &json!({})
            )
            .is_ok()
    );
    let before = methods.lock().unwrap().len();
    runtime.ensure_catalog("server-0", pending()).await.unwrap();
    assert_eq!(methods.lock().unwrap().len(), before);
    runtime.shutdown().await.unwrap();
    fixture.abort();
    let _ = fixture.await;
}

#[tokio::test]
async fn failed_request_view_consumes_selection_and_never_restores_it() {
    let (server, _, fixture) = fixture::server_fixture("").await;
    let (_, _owner, port) = owner();
    let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
    runtime.ensure_catalog("fixture", pending()).await.unwrap();
    choose(&mut runtime, "fixture");
    runtime.connections[0].connection.request_stop();
    assert!(runtime.freeze().is_err());
    assert!(!runtime.pending.references_server("fixture"));
    runtime.connections[0].phase = Phase::Failed;
    assert!(runtime.freeze().unwrap().specs().is_empty());
    runtime.shutdown().await.unwrap();
    fixture.abort();
    let _ = fixture.await;
}

#[tokio::test]
async fn panic_boundary_keeps_runtime_owned_until_shutdown() {
    let (server, _, fixture) = fixture::server_fixture("").await;
    let (_, _owner, port) = owner();
    let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
    let result = std::panic::AssertUnwindSafe(async {
        runtime.ensure_catalog("fixture", pending()).await.unwrap();
        panic!("synthetic agent unwind");
    })
    .catch_unwind()
    .await;
    assert!(result.is_err());
    assert_eq!(runtime.connections.len(), 1);
    assert!(runtime.connections[0].connection.is_connected());
    runtime.shutdown().await.unwrap();
    assert!(runtime.connections.is_empty());
    fixture.abort();
    let _ = fixture.await;
}

#[tokio::test]
async fn a_previous_request_snapshot_is_rejected_before_approval_or_traffic() {
    let (server, methods, fixture) = fixture::server_fixture("").await;
    let (_, _owner, port) = owner();
    let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
    runtime.ensure_catalog("fixture", pending()).await.unwrap();
    choose(&mut runtime, "fixture");
    let first = runtime.freeze().unwrap();
    assert!(runtime.freeze().unwrap().specs().is_empty());
    let call = ToolCall {
        id: "old-request-call".into(),
        name: first.specs()[0].name.clone(),
        arguments: json!({}),
    };
    let suffix = vec![Message::assistant_with_tool_calls("", vec![call.clone()])];
    let mut request = Box::pin(runtime.call(&first, &call, suffix, pending()));
    assert!(matches!(
        request.as_mut().now_or_never(),
        Some(Err(Error::Context(super::super::Error::SelectionStale)))
    ));
    drop(request);
    assert!(!methods.lock().unwrap().iter().any(|m| m == "tools/call"));
    runtime.shutdown().await.unwrap();
    fixture.abort();
    let _ = fixture.await;
}

fn store(
    target: &RunTarget,
) -> (
    crate::desktop_store::PrototypeRoot,
    crate::desktop_store::Writer,
) {
    store_with_prompt(target, None)
}
fn store_with_prompt(
    target: &RunTarget,
    mcp_prompt: Option<polaris_desktop_protocol::request::McpPromptChoice>,
) -> (
    crate::desktop_store::PrototypeRoot,
    crate::desktop_store::Writer,
) {
    use crate::desktop_store::{InitialState, PrototypeRoot};
    use polaris_desktop_protocol::{request::*, snapshot::*};
    let root = PrototypeRoot::new().unwrap();
    let session = SessionId::new("runtime-session").unwrap();
    let mut writer = root
        .create(
            ProjectId::new("project").unwrap(),
            session.clone(),
            InitialState {
                draft: Draft {
                    draft_revision: DecimalU64::new(0),
                    text: "synthetic user input".into(),
                    attachment_ids: vec![],
                },
                configuration: Configuration {
                    history_mode: Default::default(),
                    configuration_revision: DecimalU64::new(0),
                    provider: "fake".into(),
                    model: "fake".into(),
                    effort: "medium".into(),
                },
                policy_revision: DecimalU64::new(0),
            },
        )
        .unwrap();
    writer
        .apply(
            &Request {
                protocol_version: Default::default(),
                client_id: ClientId::new("runtime-test").unwrap(),
                request_id: RequestId::new("start").unwrap(),
                body: RequestBody::RunStart(
                    session,
                    RunStart {
                        mcp_prompt,
                        expected_draft_revision: DecimalU64::new(0),
                        expected_configuration_revision: DecimalU64::new(0),
                        expected_policy_revision: DecimalU64::new(0),
                    },
                ),
            },
            Some(target.clone()),
        )
        .unwrap();
    (root, writer)
}

#[tokio::test]
async fn runtime_dispatch_waits_for_approval_and_returns_the_exact_durable_result_once() {
    use polaris_desktop_protocol::{request::*, snapshot::ApprovalDecision};
    let (server, methods, fixture) = fixture::server_fixture("").await;
    let (target, mut owner, port) = owner();
    let (_root, mut writer) = store(&target);
    let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
    runtime.ensure_catalog("fixture", pending()).await.unwrap();
    choose(&mut runtime, "fixture");
    let first = runtime.freeze().unwrap();
    let call = ToolCall {
        id: "runtime-call".into(),
        name: first.specs()[0].name.clone(),
        arguments: json!({}),
    };
    let suffix = vec![Message::assistant_with_tool_calls("", vec![call.clone()])];
    let mut request = Box::pin(runtime.call(&first, &call, suffix, pending()));
    assert!(request.as_mut().now_or_never().is_none());
    owner.step(&mut writer, &target).unwrap();
    assert!(owner.awaiting_approval());
    assert!(request.as_mut().now_or_never().is_none());
    assert!(!methods.lock().unwrap().iter().any(|m| m == "tools/call"));
    let saved = writer.snapshot().unwrap();
    let approval = saved.state.unresolved_approvals[0].clone();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    writer
        .resolve_approval(
            &Request {
                protocol_version: Default::default(),
                client_id: ClientId::new("runtime-test").unwrap(),
                request_id: RequestId::new("allow").unwrap(),
                body: RequestBody::ApprovalResolve(
                    SessionId::new("runtime-session").unwrap(),
                    ApprovalResolve {
                        approval_id: approval.approval_id.clone(),
                        run_id: approval.run_id,
                        attempt_id: approval.attempt_id,
                        policy_revision: approval.policy_revision,
                        decision: ApprovalDecision::Allow,
                    },
                ),
            },
            now,
        )
        .unwrap();
    owner.approval_resolved(&approval.approval_id, ApprovalDecision::Allow);
    owner.step(&mut writer, &target).unwrap();
    assert_eq!(writer.snapshot().unwrap().state.runs[0].operations.len(), 1);
    let result = tokio::time::timeout(Duration::from_secs(4), async {
        loop {
            owner.step(&mut writer, &target).unwrap();
            if let Some(result) = request.as_mut().now_or_never() {
                break result.unwrap();
            }
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
    })
    .await
    .unwrap();
    drop(request);
    assert!(result.permits_continuation());
    assert!(owner.quiescent());
    let saved = writer.snapshot().unwrap();
    assert_eq!(saved.raw.len(), 3);
    assert_eq!(
        serde_json::to_value(&saved.raw[2].message).unwrap(),
        serde_json::to_value(&result.message).unwrap()
    );
    let value: serde_json::Value = serde_json::from_str(&result.message.content).unwrap();
    let handle = value["reply"]["parts"][0]["handle"].as_str().unwrap();
    let chunk = runtime.chunk(handle, 0, 1024).unwrap();
    assert!(chunk.text.unwrap().starts_with("known result "));
    runtime.shutdown().await.unwrap();
    assert!(runtime.chunk(handle, 0, 1024).is_err());
    owner.step(&mut writer, &target).unwrap();
    assert_eq!(
        methods
            .lock()
            .unwrap()
            .iter()
            .filter(|m| *m == "tools/call")
            .count(),
        1
    );
    fixture.abort();
    let _ = fixture.await;
}

#[tokio::test]
async fn resource_intent_ack_fault_never_grants_or_sends_the_read() {
    for point in ["raw_sync", "marker_rename", "directory_sync"] {
        let (server, methods, fixture) = fixture::server_fixture("").await;
        let (target, mut owner, port) =
            owner_with_policy(ExternalCeiling::Denied, ApprovalPolicy::Never);
        let (root, mut writer) = store(&target);
        let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
        runtime.ensure_catalog("fixture", pending()).await.unwrap();
        let request = runtime.freeze().unwrap();
        let call = read_call(&runtime, false);
        let suffix = vec![Message::assistant_with_tool_calls("", vec![call.clone()])];
        let mut read = Box::pin(runtime.read_resource(&request, &call, suffix, pending()));
        assert!(read.as_mut().now_or_never().is_none());
        owner.step(&mut writer, &target).unwrap();
        crate::desktop_store::inject_test_failure(Some(point));
        let result = owner.step(&mut writer, &target);
        crate::desktop_store::inject_test_failure(None);
        result.unwrap();
        assert!(matches!(
            read.await,
            Err(Error::Owner(McpCallError::Storage))
        ));
        assert!(owner.quiescent());
        assert!(
            !methods
                .lock()
                .unwrap()
                .iter()
                .any(|method| method == "resources/read")
        );
        let saved = writer.snapshot().unwrap();
        assert!(saved.state.approval_records.is_empty());
        if point == "directory_sync" {
            assert_eq!(saved.raw.len(), 3);
            assert_eq!(
                saved.state.runs[0].operations[0]
                    .mcp
                    .as_ref()
                    .unwrap()
                    .disposition(),
                Some(crate::desktop_store::McpDisposition::BeforeSend)
            );
        } else {
            assert_eq!(saved.raw.len(), 1);
            assert!(saved.state.runs[0].operations.is_empty());
        }
        runtime.shutdown().await.unwrap();
        drop(writer);
        let reopened = root
            .open(
                &ProjectId::new("project").unwrap(),
                &SessionId::new("runtime-session").unwrap(),
            )
            .unwrap();
        assert_eq!(reopened.snapshot().unwrap().raw.len(), saved.raw.len());
        assert!(
            !methods
                .lock()
                .unwrap()
                .iter()
                .any(|method| method == "resources/read")
        );
        fixture.abort();
        let _ = fixture.await;
    }
}

struct Answers {
    decision: crate::approval::Decision,
    asked: Vec<String>,
}

impl crate::approval::Approver for Answers {
    fn ask(&mut self, reason: &str) -> crate::approval::Decision {
        self.asked.push(reason.into());
        self.decision
    }
}

#[tokio::test]
async fn local_runtime_asks_the_gate_for_the_exact_operation_and_sends_once() {
    use crate::approval::{Decision, Gate};
    let (server, methods, fixture) = fixture::server_fixture("").await;
    let port = McpPort::local("local-session", "run-1", "1", ExternalCeiling::Allowed);
    let mut runtime = RunRuntime::new(config(vec![server]), port).unwrap();
    runtime.ensure_catalog("fixture", pending()).await.unwrap();
    choose(&mut runtime, "fixture");
    let first = runtime.freeze().unwrap();
    let call = ToolCall {
        id: "local-call".into(),
        name: first.specs()[0].name.clone(),
        arguments: json!({}),
    };
    let calls = || {
        methods
            .lock()
            .unwrap()
            .iter()
            .filter(|m| *m == "tools/call")
            .count()
    };
    let mut gate = Gate::new(ApprovalPolicy::Always);
    let mut deny = Answers {
        decision: Decision::Deny,
        asked: vec![],
    };
    let prepared = runtime.prepare_local(&first, &call).unwrap();
    assert!(matches!(
        prepared.approve(&mut gate, &mut deny).await,
        Err("mcp_call_denied")
    ));
    assert!(deny.asked[0].contains("fixture"));
    let mut never = Gate::new(ApprovalPolicy::Never);
    let prepared = runtime.prepare_local(&first, &call).unwrap();
    assert!(matches!(
        prepared.approve(&mut never, &mut deny).await,
        Err("mcp_call_denied")
    ));
    assert_eq!(deny.asked.len(), 1, "policy never refuses without asking");
    assert_eq!(calls(), 0);

    let mut allow = Answers {
        decision: Decision::Allow,
        asked: vec![],
    };
    let approved = runtime
        .prepare_local(&first, &call)
        .unwrap()
        .approve(&mut gate, &mut allow)
        .await
        .unwrap();
    let result = runtime.perform_local(approved, pending()).await.unwrap();
    assert!(result.permits_continuation() && result.succeeded());
    assert_eq!(result.message.tool_call_id.as_deref(), Some("local-call"));
    let value: serde_json::Value = serde_json::from_str(&result.message.content).unwrap();
    assert_eq!(value["intent"]["run_id"], "run-1");
    let handle = value["reply"]["parts"][0]["handle"].as_str().unwrap();
    assert!(
        runtime
            .chunk(handle, 0, 1024)
            .unwrap()
            .text
            .unwrap()
            .starts_with("known result ")
    );
    assert_eq!(calls(), 1);

    let (_target, _owner, desktop) = owner();
    let mut owned = RunRuntime::new(config(vec![]), desktop).unwrap();
    assert!(!owned.is_local());
    assert!(matches!(
        owned.prepare_local(&first, &call),
        Err(Error::Owner(McpCallError::Invalid))
    ));
    runtime.shutdown().await.unwrap();
    fixture.abort();
    let _ = fixture.await;
}

#[tokio::test]
async fn preselection_keeps_three_picks_and_room_for_one_explicit_selection() {
    let mut servers = Vec::new();
    let mut tasks = Vec::new();
    for name in ["fixture", "s2", "s3", "s4", "s5"] {
        let (mut server, _methods, task) = fixture::server_fixture("").await;
        server.server_id = name.into();
        servers.push(server);
        tasks.push(task);
    }
    let config = config(servers);
    let mut picks = Vec::new();
    for name in ["fixture", "s2", "s3", "s4"] {
        picks.push(pick_for(&config, name).await);
    }
    let (_target, _owner, port) = owner();
    let mut runtime = RunRuntime::new(config, port).unwrap();
    runtime.set_preselection(picks);
    runtime.refresh_preselected(pending()).await.unwrap();
    assert_eq!(runtime.freeze().unwrap().specs().len(), 3);
    runtime.ensure_catalog("s5", pending()).await.unwrap();
    choose(&mut runtime, "s5");
    let request = runtime.freeze().unwrap();
    assert_eq!(
        request.specs().len(),
        4,
        "three picks and the explicit selection"
    );
    runtime.shutdown().await.unwrap();
    for task in tasks {
        task.abort();
        let _ = task.await;
    }
}
