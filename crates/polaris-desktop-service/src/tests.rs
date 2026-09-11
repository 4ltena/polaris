//! 実OS匿名pipeで制御・本文・停止を往復し、論理engine再起動を別途検証する。

use super::*;
use crate::transport::read_frame;
use polaris_desktop_protocol::{codec, request::*};
use tokio::{io::AsyncWriteExt, net::unix::pipe, task::JoinHandle, time::sleep};

struct PendingUsageProvider;
#[async_trait::async_trait]
impl polaris_provider::Provider for PendingUsageProvider {
    async fn complete(
        &self,
        _: polaris_provider::CompletionRequest,
    ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
        std::future::pending().await
    }
}

#[tokio::test]
async fn cancelled_provider_usage_survives_the_waiting_future_and_reopen() {
    use polaris_provider::Provider;
    let (root, mut engine) = service();
    hello(&mut engine);
    begin(&mut engine);
    let provider = engine
        .active
        .as_ref()
        .unwrap()
        .usage
        .wrap(Arc::new(PendingUsageProvider));
    let mut request = Box::pin(provider.complete(polaris_provider::CompletionRequest {
        system: String::new(),
        messages: vec![],
        tools: vec![],
    }));
    assert!(
        timeout(Duration::from_millis(1), &mut request)
            .await
            .is_err()
    );
    drop(request);
    engine.checkpoint_usage().unwrap();
    let saved = engine.store.snapshot().unwrap();
    assert_eq!(saved.state.runs[0].usage.unwrap().failed_requests, 1);
    engine.checkpoint_usage().unwrap();
    assert_eq!(
        engine.store.snapshot().unwrap().state.runs[0].usage,
        saved.state.runs[0].usage
    );
    drop(provider);
    drop(engine);
    let reopened = Engine::reopen(&root, Options::default()).unwrap();
    assert_eq!(
        reopened.store.snapshot().unwrap().state.runs[0].usage,
        saved.state.runs[0].usage
    );
}

#[test]
#[ignore = "dummy child entered only by controlled service tests"]
fn execution_dummy_child() {
    use std::io::{Read, Write};
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input).unwrap();
    if input == "flood" {
        std::io::stdout()
            .write_all(&vec![b'x'; 512 * 1024])
            .unwrap();
        std::io::stderr()
            .write_all(&vec![b'y'; 512 * 1024])
            .unwrap();
    } else if let Some(path) = input.strip_prefix("wait|") {
        std::fs::write(path, "started").unwrap();
        loop {
            std::thread::sleep(Duration::from_millis(10));
        }
    } else {
        println!("SERVICE-CHILD:{input}");
    }
}

fn execution_command(mode: String) -> polaris_core::desktop_execution::ExecutionCommand {
    use polaris_core::desktop_execution::*;
    ExecutionCommand {
        policy: SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[std::env::temp_dir()]).unwrap(),
        program: std::env::current_exe().unwrap(),
        args: vec![
            "--exact".into(),
            "engine::tests::execution_dummy_child".into(),
            "--ignored".into(),
            "--nocapture".into(),
        ],
        stdin: Some(mode),
    }
}

async fn execution_settle(engine: &mut Engine) {
    let until = Instant::now() + Duration::from_secs(6);
    loop {
        engine.settle_without_output().unwrap();
        if engine.ready {
            break;
        }
        assert!(Instant::now() < until, "execution ownership did not settle");
        sleep(Duration::from_millis(10)).await;
    }
}

#[tokio::test]
async fn approval_request_is_durable_before_spawn() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    begin(&mut engine);
    let waiting = engine
        .active
        .as_ref()
        .unwrap()
        .execution
        .port
        .submit(execution_command("approval-fixture".into()))
        .unwrap();
    let (out, _, _workers) = transport::workers(
        tokio::io::empty(),
        tokio::io::sink(),
        Duration::from_secs(1),
    );
    engine.step(&out).unwrap();
    let published = engine.store.snapshot().unwrap();
    assert_eq!(published.state.unresolved_approvals.len(), 1);
    assert!(
        !published.state.runs[0]
            .operations
            .iter()
            .any(|op| op.operation_id.as_str().starts_with("exec-"))
    );
    drop(waiting);
    execution_settle(&mut engine).await;
}

#[tokio::test]
async fn approval_denial_and_policy_revocation_never_create_execution_intent() {
    for revoke in [false, true] {
        let (_root, mut engine) = service();
        hello(&mut engine);
        begin(&mut engine);
        let waiting = engine
            .active
            .as_ref()
            .unwrap()
            .execution
            .port
            .submit(execution_command("must-not-start".into()))
            .unwrap();
        let (out, _, _workers) = transport::workers(
            tokio::io::empty(),
            tokio::io::sink(),
            Duration::from_secs(1),
        );
        engine.step(&out).unwrap();
        let approval = engine.store.snapshot().unwrap().state.unresolved_approvals[0].clone();
        let answer = request(
            "answer",
            RequestBody::ApprovalResolve(
                session_id(),
                ApprovalResolve {
                    approval_id: approval.approval_id,
                    run_id: approval.run_id,
                    attempt_id: approval.attempt_id,
                    policy_revision: approval.policy_revision,
                    decision: if revoke {
                        polaris_desktop_protocol::snapshot::ApprovalDecision::Allow
                    } else {
                        polaris_desktop_protocol::snapshot::ApprovalDecision::Deny
                    },
                },
            ),
        );
        direct(&mut engine, &answer).unwrap();
        // ACK再送が二度目の起動許可を発行してはならない。
        direct(&mut engine, &answer).unwrap();
        if revoke {
            engine
                .store
                .set_policy_revision(DecimalU64::new(1))
                .unwrap();
        }
        engine.step(&out).unwrap();
        let result = timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap();
        assert!(result.problem.is_some());
        let published = engine.store.snapshot().unwrap();
        assert!(
            !published.state.runs[0]
                .operations
                .iter()
                .any(|op| op.operation_id.as_str().starts_with("exec-"))
        );
        assert!(published.state.unresolved_approvals.is_empty());
        execution_settle(&mut engine).await;
    }
}

#[tokio::test]
async fn approval_invalidated_after_intent_is_saved_without_starting_child() {
    for disconnect in [false, true] {
        let (_root, mut engine) = service();
        hello(&mut engine);
        begin(&mut engine);
        let execution = &mut engine.active.as_mut().unwrap().execution;
        execution.invalidate_after_intent = Some(disconnect);
        let waiting = execution
            .port
            .submit(execution_command("must-not-start".into()))
            .unwrap();
        let (out, _, _workers) = transport::workers(
            tokio::io::empty(),
            tokio::io::sink(),
            Duration::from_secs(1),
        );
        step_with_fixture_approval(&mut engine, &out);
        let result = timeout(Duration::from_secs(1), waiting)
            .await
            .unwrap()
            .unwrap();
        assert!(result.problem.as_ref().unwrap().contains("authorization"));
        assert!(result.stdout.is_empty());
        assert_eq!(
            result.end,
            polaris_core::desktop_execution::ControlledEnd::CancelledBeforeSpawn
        );
        execution_settle(&mut engine).await;
        let published = engine.store.snapshot().unwrap();
        let operations: Vec<_> = published.state.runs[0]
            .operations
            .iter()
            .filter(|op| op.operation_id.as_str().starts_with("exec-"))
            .collect();
        assert_eq!(operations.len(), 1);
        assert!(operations[0].result_id.is_some());
    }
}

#[tokio::test]
async fn execution_pre_registration_cancel_has_no_operation_or_child() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    begin(&mut engine);
    let port = engine.active.as_ref().unwrap().execution.port.clone();
    drop(port.submit(execution_command("echo".into())).unwrap());
    execution_settle(&mut engine).await;
    let p = engine.store.snapshot().unwrap();
    assert!(p.state.runs[0].operations.is_empty());
    assert_eq!(p.state.runs[0].run.state, RunState::Cancelled);
}

#[tokio::test]
async fn regression_old_cancel_replay_does_not_cancel_current_execution() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    let old = begin(&mut engine);
    let cancel = request("old-cancel", RequestBody::RunCancel(session_id(), old));
    direct(&mut engine, &cancel).unwrap();
    direct(&mut engine, &draft("next", 2, "next")).unwrap();
    direct(&mut engine, &start("next-start", 3, 0)).unwrap();
    let port = engine.active.as_ref().unwrap().execution.port.clone();
    direct(&mut engine, &cancel).unwrap();
    let waiting = port.submit(execution_command("current".into()));
    assert!(
        waiting.is_ok(),
        "old cancellation replay closed the current execution port"
    );
    drop(waiting);
    execution_settle(&mut engine).await;
}

#[tokio::test]
async fn regression_cleanup_continues_after_eof_deadline_and_storage_error_without_step() {
    for storage_error in [false, true] {
        let (_root, mut engine) = service();
        hello(&mut engine);
        begin(&mut engine);
        let wait = engine
            .active
            .as_ref()
            .unwrap()
            .execution
            .port
            .submit(execution_command("cleanup".into()))
            .unwrap();
        let (out, _, _workers) = transport::workers(
            tokio::io::empty(),
            tokio::io::sink(),
            Duration::from_secs(1),
        );
        step_with_fixture_approval(&mut engine, &out);
        timeout(Duration::from_secs(5), wait)
            .await
            .unwrap()
            .unwrap();
        sleep(Duration::from_millis(30)).await;
        let released = engine
            .active
            .as_mut()
            .unwrap()
            .execution
            .inject_unconfirmed_cleanup();
        engine.failed = storage_error;
        let (client, task) = connect(engine);
        drop(client.tx);
        let (mut engine, outcome) = timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.is_err(), storage_error);
        assert!(!engine.ready);
        released.store(true, Ordering::Release);
        let deadline = Instant::now() + Duration::from_secs(1);
        while !engine
            .active
            .as_ref()
            .unwrap()
            .execution
            .cleanup_complete_for_test()
        {
            assert!(
                Instant::now() < deadline,
                "join/reap stopped with the storage owner; no new Engine or Step is running"
            );
            sleep(Duration::from_millis(10)).await;
        }
        engine.failed = false;
        execution_settle(&mut engine).await;
    }
}

#[tokio::test]
async fn regression_storage_failure_live_child_is_joined_without_step() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    begin(&mut engine);
    let marker =
        std::env::temp_dir().join(format!("polaris-reclaim-child-{}", engine.epoch.as_str()));
    let wait = engine
        .active
        .as_ref()
        .unwrap()
        .execution
        .port
        .submit(execution_command(format!("wait|{}", marker.display())))
        .unwrap();
    let (out, _, _workers) = transport::workers(
        tokio::io::empty(),
        tokio::io::sink(),
        Duration::from_secs(1),
    );
    step_with_fixture_approval(&mut engine, &out);
    let deadline = Instant::now() + Duration::from_secs(3);
    while !marker.exists() {
        assert!(Instant::now() < deadline);
        sleep(Duration::from_millis(10)).await;
    }
    engine.failed = true;
    let (client, task) = connect(engine);
    drop(client.tx);
    let (mut engine, result) = timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert!(result.is_err());
    assert!(!engine.ready);
    let outcome = timeout(Duration::from_secs(5), wait)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        outcome.end,
        polaris_core::desktop_execution::ControlledEnd::Cancelled
    );
    let deadline = Instant::now() + Duration::from_secs(2);
    while !engine
        .active
        .as_ref()
        .unwrap()
        .execution
        .cleanup_complete_for_test()
    {
        assert!(
            Instant::now() < deadline,
            "native worker join did not continue independently"
        );
        sleep(Duration::from_millis(10)).await;
    }
    assert!(!engine.ready, "cleanup must not stand in for persistence");
    engine.failed = false;
    execution_settle(&mut engine).await;
    std::fs::remove_file(marker).unwrap();
}

#[tokio::test]
async fn execution_storage_failure_prevents_queued_worker_registration() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    begin(&mut engine);
    let wait = engine
        .active
        .as_ref()
        .unwrap()
        .execution
        .port
        .submit(execution_command("must-not-start".into()))
        .unwrap();
    let (out, _, _workers) = transport::workers(
        tokio::io::empty(),
        tokio::io::sink(),
        Duration::from_secs(1),
    );
    engine.failed = true;
    assert!(engine.step(&out).is_err());
    assert!(
        engine.store.snapshot().unwrap().state.runs[0]
            .operations
            .is_empty()
    );
    engine.failed = false;
    execution_settle(&mut engine).await;
    assert!(
        wait.await
            .unwrap()
            .problem
            .as_ref()
            .unwrap()
            .contains("before approval")
    );
}

#[tokio::test]
async fn execution_wait_drop_after_registration_retains_join_and_saved_result() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    let target = begin(&mut engine);
    let port = engine.active.as_ref().unwrap().execution.port.clone();
    let wait = port.submit(execution_command("echo".into())).unwrap();
    let (out, _, _workers) = transport::workers(
        tokio::io::empty(),
        tokio::io::sink(),
        Duration::from_secs(1),
    );
    step_with_fixture_approval(&mut engine, &out);
    assert!(!engine.active.as_ref().unwrap().execution.quiescent());
    assert_eq!(
        engine.store.snapshot().unwrap().state.runs[0]
            .operations
            .len(),
        2
    );
    drop(wait);
    engine.settle_without_output().unwrap();
    assert!(!engine.ready);
    execution_settle(&mut engine).await;
    let p = engine.store.snapshot().unwrap();
    assert!(
        p.state.runs[0]
            .operations
            .iter()
            .all(|op| op.result_id.is_some())
    );
    assert_eq!(p.state.runs[0].run.run_id, target.run_id);
    assert!(
        p.raw
            .last()
            .unwrap()
            .message
            .content
            .contains("execution end=")
    );
}

#[tokio::test]
async fn execution_unread_output_is_bounded_and_result_is_owned_before_notification() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    begin(&mut engine);
    let port = engine.active.as_ref().unwrap().execution.port.clone();
    let wait = port.submit(execution_command("flood".into())).unwrap();
    let (out, _, _workers) = transport::workers(
        tokio::io::empty(),
        tokio::io::sink(),
        Duration::from_secs(1),
    );
    step_with_fixture_approval(&mut engine, &out);
    let result = timeout(Duration::from_secs(5), wait)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.status, Some(0), "{result:?}");
    assert_eq!(result.stdout.len(), 256 * 1024);
    assert_eq!(result.stderr.len(), 256 * 1024);
    assert!(
        result.stdout_truncated
            && result.stderr_truncated
            && result.stdout_eof
            && result.stderr_eof
    );
    assert!(!engine.active.as_ref().unwrap().execution.quiescent());
    execution_settle(&mut engine).await;
    let published = engine.store.snapshot().unwrap();
    let text = &published.raw.last().unwrap().message.content;
    assert!(text.len() <= TEXT_BYTES);
    assert!(text.contains("transcript truncated"));
}

#[tokio::test]
async fn execution_worker_panic_is_saved_as_failure_and_cannot_report_success() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    begin(&mut engine);
    engine.active.as_mut().unwrap().execution.panic_worker = true;
    let wait = engine
        .active
        .as_ref()
        .unwrap()
        .execution
        .port
        .submit(execution_command("echo".into()))
        .unwrap();
    let (out, _, _workers) = transport::workers(
        tokio::io::empty(),
        tokio::io::sink(),
        Duration::from_secs(1),
    );
    step_with_fixture_approval(&mut engine, &out);
    let result = timeout(Duration::from_secs(2), wait)
        .await
        .unwrap()
        .unwrap();
    assert!(result.problem.as_ref().unwrap().contains("panicked"));
    let until = Instant::now() + Duration::from_secs(2);
    while engine.active.is_some() {
        engine.complete(Observation::Succeeded, None).unwrap();
        assert!(Instant::now() < until);
        sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        engine.store.snapshot().unwrap().state.runs[0].run.state,
        RunState::Failed
    );
}

#[tokio::test]
async fn execution_unconfirmed_cleanup_and_storage_failure_both_suppress_ready() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    begin(&mut engine);
    let wait = engine
        .active
        .as_ref()
        .unwrap()
        .execution
        .port
        .submit(execution_command("echo".into()))
        .unwrap();
    let (out, _, _workers) = transport::workers(
        tokio::io::empty(),
        tokio::io::sink(),
        Duration::from_secs(1),
    );
    step_with_fixture_approval(&mut engine, &out);
    timeout(Duration::from_secs(5), wait)
        .await
        .unwrap()
        .unwrap();
    // 通知後もjoinがまだ終了していない境界を待つ。
    sleep(Duration::from_millis(30)).await;
    let released = engine
        .active
        .as_mut()
        .unwrap()
        .execution
        .inject_unconfirmed_cleanup();
    for _ in 0..8 {
        engine.settle(&out).unwrap();
        assert!(!engine.ready && engine.active.is_some());
        assert!(
            !engine.store.snapshot().unwrap().state.runs[0]
                .run
                .state
                .is_terminal()
        );
    }
    released.store(true, Ordering::Release);
    // 保存失敗のsticky状態を注入。未保存結果を持ったままreadyを拒否する。
    engine.failed = true;
    assert!(engine.settle(&out).is_err());
    assert!(engine.settle_without_output().is_err());
    assert!(!engine.ready && engine.active.is_some());
    engine.failed = false;
    execution_settle(&mut engine).await;
    assert!(
        engine
            .store
            .snapshot()
            .unwrap()
            .raw
            .last()
            .unwrap()
            .message
            .content
            .contains("SERVICE-CHILD:echo")
    );
}

#[test]
fn execution_owner_loss_retains_unsaved_result_and_blocks_new_ready() {
    const MARKER: &str = "POLARIS_SERVICE_OWNER_LOSS_FIXTURE";
    if std::env::var_os(MARKER).is_none() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "engine::tests::execution_owner_loss_retains_unsaved_result_and_blocks_new_ready",
                "--nocapture",
            ])
            .env_clear()
            .env(MARKER, "1")
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        return;
    }
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let (_root, mut engine) = service();
        hello(&mut engine);
        begin(&mut engine);
        let wait = engine
            .active
            .as_ref()
            .unwrap()
            .execution
            .port
            .submit(execution_command("echo".into()))
            .unwrap();
        let (out, _, _workers) = transport::workers(
            tokio::io::empty(),
            tokio::io::sink(),
            Duration::from_secs(1),
        );
        step_with_fixture_approval(&mut engine, &out);
        let reclaimed = engine
            .active
            .as_ref()
            .unwrap()
            .execution
            .cleanup_probe_for_test();
        drop(engine); // 新Engineを作る前に、独立回収がjoinを終えることを確認する。
        timeout(Duration::from_secs(5), wait)
            .await
            .unwrap()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !reclaimed() {
            assert!(
                Instant::now() < deadline,
                "dropped owner left its worker unjoined"
            );
            sleep(Duration::from_millis(10)).await;
        }
        let (_root, mut next) = service();
        for _ in 0..8 {
            next.settle_without_output().unwrap();
            assert!(!next.ready);
            sleep(Duration::from_millis(10)).await;
        }
    });
}

#[tokio::test]
async fn execution_eof_cancels_child_and_preserves_result_with_unread_transport() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    begin(&mut engine);
    let marker = std::env::temp_dir().join(format!("polaris-eof-child-{}", engine.epoch.as_str()));
    let wait = engine
        .active
        .as_ref()
        .unwrap()
        .execution
        .port
        .submit(execution_command(format!("wait|{}", marker.display())))
        .unwrap();
    let (out, _, _workers) = transport::workers(
        tokio::io::empty(),
        tokio::io::sink(),
        Duration::from_secs(1),
    );
    step_with_fixture_approval(&mut engine, &out);
    let (client, task) = connect(engine);
    let until = Instant::now() + Duration::from_secs(3);
    while !marker.exists() {
        assert!(Instant::now() < until);
        sleep(Duration::from_millis(10)).await;
    }
    drop(client.tx);
    let (engine, result) = timeout(Duration::from_secs(5), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.unwrap(), Exit::Eof);
    assert!(engine.ready);
    assert_eq!(
        wait.await.unwrap().end,
        polaris_core::desktop_execution::ControlledEnd::Cancelled
    );
    assert!(
        engine.store.snapshot().unwrap().state.runs[0]
            .operations
            .iter()
            .all(|op| op.result_id.is_some())
    );
    std::fs::remove_file(marker).unwrap();
}

#[tokio::test]
async fn execution_serve_drop_continues_owner_cleanup_and_persistence() {
    let (root, mut engine) = service();
    hello(&mut engine);
    begin(&mut engine);
    let port = engine.active.as_ref().unwrap().execution.port.clone();
    let marker =
        std::env::temp_dir().join(format!("polaris-service-child-{}", engine.epoch.as_str()));
    let wait = port
        .submit(execution_command(format!("wait|{}", marker.display())))
        .unwrap();
    let (out, _, _workers) = transport::workers(
        tokio::io::empty(),
        tokio::io::sink(),
        Duration::from_secs(1),
    );
    step_with_fixture_approval(&mut engine, &out);
    let (mut client, task) = connect(engine);
    // 起動済み子のmarkerだけではserve Futureがpoll済みとは限らない。
    // ownerへの移譲を応答で確認してから、通信Futureの破棄を試す。
    client
        .call(&request(
            "owner-ready",
            RequestBody::SessionSnapshot(session_id()),
        ))
        .await
        .unwrap();
    let until = Instant::now() + Duration::from_secs(3);
    while !marker.exists() {
        assert!(Instant::now() < until);
        sleep(Duration::from_millis(10)).await;
    }
    task.abort();
    let _ = task.await;
    let result = timeout(Duration::from_secs(5), wait)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        result.end,
        polaris_core::desktop_execution::ControlledEnd::Cancelled
    );
    let until = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(reopened) = Engine::reopen(&root, Options::default()) {
            let p = reopened.store.snapshot().unwrap();
            assert_eq!(p.state.runs[0].run.state, RunState::Cancelled);
            assert!(
                p.state.runs[0]
                    .operations
                    .iter()
                    .all(|op| op.result_id.is_some())
            );
            assert!(
                p.raw
                    .last()
                    .unwrap()
                    .message
                    .content
                    .contains("execution end=Cancelled")
            );
            break;
        }
        assert!(Instant::now() < until);
        sleep(Duration::from_millis(10)).await;
    }
    std::fs::remove_file(marker).unwrap();
}

// 旧所有試験でも実際の承認保存経路を通し、取消・回収の対象まで起動する。
fn step_with_fixture_approval(engine: &mut Engine, out: &Outbox) {
    engine.step(out).unwrap();
    let approvals = engine.store.snapshot().unwrap().state.unresolved_approvals;
    let has_pending = !approvals.is_empty();
    for approval in approvals {
        let request = request(
            &format!("fixture-{}", approval.approval_id.as_str()),
            RequestBody::ApprovalResolve(
                session_id(),
                ApprovalResolve {
                    approval_id: approval.approval_id,
                    run_id: approval.run_id,
                    attempt_id: approval.attempt_id,
                    policy_revision: approval.policy_revision,
                    decision: polaris_desktop_protocol::snapshot::ApprovalDecision::Allow,
                },
            ),
        );
        direct(engine, &request).unwrap();
    }
    if has_pending {
        engine.step(out).unwrap();
    }
}

fn request(id: &str, body: RequestBody) -> Request {
    Request {
        protocol_version: ProtocolVersion,
        client_id: ClientId::new("client").unwrap(),
        request_id: RequestId::new(id).unwrap(),
        body,
    }
}
fn draft(id: &str, revision: u64, text: &str) -> Request {
    request(
        id,
        RequestBody::DraftUpdate(
            session_id(),
            DraftUpdate {
                expected_draft_revision: DecimalU64::new(revision),
                text: text.into(),
                attachment_ids: vec![],
            },
        ),
    )
}
fn start(id: &str, draft_revision: u64, config_revision: u64) -> Request {
    request(
        id,
        RequestBody::RunStart(
            session_id(),
            RunStart {
                expected_draft_revision: DecimalU64::new(draft_revision),
                expected_configuration_revision: DecimalU64::new(config_revision),
                expected_policy_revision: DecimalU64::new(0),
            },
        ),
    )
}
fn configure(id: &str, revision: u64, model: &str) -> Request {
    request(
        id,
        RequestBody::SessionConfigure(
            session_id(),
            SessionConfigure {
                history_mode: Default::default(),
                expected_configuration_revision: DecimalU64::new(revision),
                provider: "fake".into(),
                model: model.into(),
                effort: "none".into(),
            },
        ),
    )
}
fn service() -> (PrototypeRoot, Engine) {
    let root = PrototypeRoot::new().unwrap();
    let engine = Engine::create(&root, Options::default()).unwrap();
    (root, engine)
}
fn direct(engine: &mut Engine, request: &Request) -> Result<SuccessResult, ProtocolError> {
    engine.handle(request, &mut vec![])
}
fn hello(engine: &mut Engine) {
    direct(engine, &request("hello", RequestBody::Hello)).unwrap();
}
fn begin(engine: &mut Engine) -> RunTarget {
    direct(engine, &draft("draft", 0, "入力")).unwrap();
    let SuccessResult::RunStart(run) = direct(engine, &start("start", 1, 0)).unwrap() else {
        panic!()
    };
    RunTarget {
        run_id: run.run_id,
        attempt_id: run.attempt_id,
    }
}

struct Client {
    tx: pipe::Sender,
    rx: pipe::Receiver,
    events: Vec<Event>,
}
type ServiceTask = JoinHandle<(Engine, Result<Exit, ServiceError>)>;
fn connect(engine: Engine) -> (Client, ServiceTask) {
    let (tx, reader) = pipe::pipe().unwrap();
    let (writer, rx) = pipe::pipe().unwrap();
    let task = tokio::spawn(async move {
        let mut service = FakeService {
            engine: Some(engine),
        };
        let result = service.serve(reader, writer).await;
        (service.engine.unwrap(), result)
    });
    (
        Client {
            tx,
            rx,
            events: vec![],
        },
        task,
    )
}
impl Client {
    async fn send(&mut self, request: &Request) {
        self.tx
            .write_all(&codec::encode(request).unwrap())
            .await
            .unwrap();
    }
    async fn response(&mut self) -> Response {
        loop {
            let value: serde_json::Value =
                timeout(Duration::from_secs(3), read_frame(&mut self.rx))
                    .await
                    .unwrap()
                    .unwrap()
                    .unwrap();
            if value["kind"] == "event" {
                self.events.push(serde_json::from_value(value).unwrap());
            } else {
                return serde_json::from_value(value).unwrap();
            }
        }
    }
    async fn call(&mut self, request: &Request) -> Result<SuccessResult, ProtocolError> {
        self.send(request).await;
        let response = self.response().await;
        response.validate_for(request).unwrap();
        response.outcome
    }
    async fn event(&mut self) -> Event {
        let event = timeout(Duration::from_secs(3), read_frame(&mut self.rx))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        self.events.push(event);
        self.events.last().unwrap().clone()
    }
}

#[tokio::test]
async fn frames_partial_oversize_and_eof_do_not_publish() {
    for bytes in [
        vec![0, 0],
        vec![0, 0, 0, 9, b'{'],
        (1_048_577u32).to_be_bytes().to_vec(),
        vec![0, 0, 0, 1, 0xff],
        vec![0, 0, 0, 1, b'{'],
    ] {
        let (_root, engine) = service();
        let (mut client, task) = connect(engine);
        client.tx.write_all(&bytes).await.unwrap();
        drop(client.tx);
        let (engine, result) = timeout(Duration::from_secs(2), task)
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(result, Err(ServiceError::Codec(_))));
        assert_eq!(
            engine
                .store
                .snapshot()
                .unwrap()
                .marker
                .session_revision
                .get(),
            0
        );
    }
    let (_root, engine) = service();
    let (mut client, task) = connect(engine);
    let frame = codec::encode(&request("hello", RequestBody::Hello)).unwrap();
    for byte in frame {
        client.tx.write_all(&[byte]).await.unwrap();
        tokio::task::yield_now().await;
    }
    assert!(matches!(
        client.response().await.outcome,
        Ok(SuccessResult::Hello(_))
    ));
    drop(client.tx);
    assert_eq!(task.await.unwrap().1.unwrap(), Exit::Eof);
}

#[tokio::test]
async fn hello_gate_capabilities_authorization_and_unknown_version() {
    let (_root, engine) = service();
    let (mut client, task) = connect(engine);
    assert_eq!(
        client
            .call(&draft("bad", 0, "未受理"))
            .await
            .unwrap_err()
            .code,
        ErrorCode::InvalidRequest
    );
    let SuccessResult::Hello(hello) = client
        .call(&request("hello", RequestBody::Hello))
        .await
        .unwrap()
    else {
        panic!()
    };
    assert!(hello.capabilities.contains(&Capability::ApprovalResolve));
    let status = request(
        "status",
        RequestBody::RequestStatus(
            SessionId::new("other").unwrap(),
            RequestStatus {
                client_id: ClientId::new("client").unwrap(),
                request_id: RequestId::new("bad").unwrap(),
            },
        ),
    );
    assert_eq!(
        client.call(&status).await.unwrap_err().code,
        ErrorCode::PermissionDenied
    );
    let approval = request(
        "approval",
        RequestBody::ApprovalResolve(
            session_id(),
            ApprovalResolve {
                approval_id: ApprovalId::new("a").unwrap(),
                run_id: RunId::new("r").unwrap(),
                attempt_id: AttemptId::new("a").unwrap(),
                policy_revision: DecimalU64::new(0),
                decision: polaris_desktop_protocol::snapshot::ApprovalDecision::Allow,
            },
        ),
    );
    assert_eq!(
        client.call(&approval).await.unwrap_err().code,
        ErrorCode::NotFound
    );
    let mut value = serde_json::to_value(request("unknown", RequestBody::Hello)).unwrap();
    value["protocol_version"] = 2.into();
    client
        .tx
        .write_all(&codec::encode(&value).unwrap())
        .await
        .unwrap();
    let (engine, result) = task.await.unwrap();
    assert!(matches!(
        result,
        Err(ServiceError::Codec(codec::CodecError::UnsupportedVersion))
    ));
    assert_eq!(
        engine
            .store
            .snapshot()
            .unwrap()
            .marker
            .session_revision
            .get(),
        0
    );
}

#[tokio::test]
async fn send_dedup_cas_and_original_configuration_response() {
    let (_root, engine) = service();
    let (mut client, task) = connect(engine);
    client
        .call(&request("hello", RequestBody::Hello))
        .await
        .unwrap();
    let a = configure("a", 0, "scripted-alt");
    let a_result = client.call(&a).await.unwrap();
    client.call(&configure("b", 1, "scripted")).await.unwrap();
    assert_eq!(client.call(&a).await.unwrap(), a_result);
    assert_eq!(
        client
            .call(&configure("stale", 0, "scripted"))
            .await
            .unwrap_err()
            .code,
        ErrorCode::RevisionConflict
    );
    client.call(&draft("d", 0, "送信")).await.unwrap();
    let sent = start("s", 1, 2);
    let accepted = client.call(&sent).await.unwrap();
    assert_eq!(client.call(&sent).await.unwrap(), accepted);
    assert_eq!(
        client.call(&start("s", 0, 2)).await.unwrap_err().code,
        ErrorCode::RevisionConflict
    );
    client.call(&draft("new", 2, "次の下書き")).await.unwrap();
    assert_eq!(
        client
            .call(&draft("stale-d", 1, "古い"))
            .await
            .unwrap_err()
            .code,
        ErrorCode::RevisionConflict
    );
    drop(client.tx);
    let (engine, result) = task.await.unwrap();
    result.unwrap();
    let saved = engine.store.snapshot().unwrap();
    assert_eq!(saved.state.configuration.model, "scripted");
    assert_eq!(saved.state.runs.len(), 1);
    assert_eq!(
        saved.state.runs[0]
            .configuration
            .configuration_revision
            .get(),
        2
    );
    assert_eq!(saved.state.draft.text, "次の下書き");
    assert_eq!(saved.raw.iter().filter(|r| r.starts_turn).count(), 1);
}

#[tokio::test]
async fn stream_utf8_saved_history_and_resubscribe_do_not_rerun() {
    let (_root, engine) = service();
    let expected = engine.options.script.concat();
    let (mut client, task) = connect(engine);
    client
        .call(&request("hello", RequestBody::Hello))
        .await
        .unwrap();
    let subscribe = request(
        "sub",
        RequestBody::SessionSubscribe(session_id(), Subscribe::default()),
    );
    let SuccessResult::SessionSubscribe(snapshot) = client.call(&subscribe).await.unwrap() else {
        panic!()
    };
    client.call(&draft("d", 0, "質問")).await.unwrap();
    client.call(&start("s", 1, 0)).await.unwrap();
    loop {
        if matches!(client.event().await.body, EventBody::RunState(r) if r.state == RunState::Succeeded)
        {
            break;
        }
    }
    let mut text = String::new();
    let mut message = None;
    for (index, event) in client.events.iter().enumerate() {
        assert_eq!(event.event_seq.get(), index as u64 + 1);
        assert_eq!(event.subscription_id, snapshot.position.subscription_id);
        if let EventBody::MessageDelta(delta) = &event.body {
            message = Some(delta.message_id.clone());
            if delta.durability == Durability::Tentative {
                assert_eq!(delta.byte_offset.get() as usize, text.len());
                text.push_str(&delta.text);
            } else {
                assert_eq!(delta.text, expected);
            }
        }
    }
    assert_eq!(text, expected);
    let SuccessResult::SessionSubscribe(snapshot) = client.call(&subscribe).await.unwrap() else {
        panic!()
    };
    assert_eq!(snapshot.runs.len(), 1);
    assert_eq!(snapshot.runs[0].state, RunState::Succeeded);
    let history = request(
        "history",
        RequestBody::HistoryPage(
            session_id(),
            HistoryPage {
                snapshot_id: snapshot.snapshot_id,
                cursor: snapshot.history_start_cursor,
                limit: PageLimit::new(256).unwrap(),
            },
        ),
    );
    let SuccessResult::HistoryPage(page) = client.call(&history).await.unwrap() else {
        panic!()
    };
    assert_eq!(page.messages.len(), 2);
    assert_eq!(page.messages[1].text, expected);
    assert_eq!(page.messages[1].message_id, message.unwrap());
    assert_eq!(
        page.messages[1].saved_byte_offset.get() as usize,
        expected.len()
    );
    let status = request(
        "status",
        RequestBody::RequestStatus(
            session_id(),
            RequestStatus {
                client_id: ClientId::new("client").unwrap(),
                request_id: RequestId::new("s").unwrap(),
            },
        ),
    );
    assert!(matches!(
        client.call(&status).await.unwrap(),
        SuccessResult::RequestStatus(RequestStatusResult::Completed { .. })
    ));
    drop(client.tx);
    task.await.unwrap().1.unwrap();
}

#[tokio::test]
async fn cancellation_ack_precedes_terminal_over_pipe() {
    let (_root, engine) = service();
    let (mut client, task) = connect(engine);
    client
        .call(&request("h", RequestBody::Hello))
        .await
        .unwrap();
    client
        .call(&request(
            "sub",
            RequestBody::SessionSubscribe(session_id(), Subscribe::default()),
        ))
        .await
        .unwrap();
    client.call(&draft("d", 0, "x")).await.unwrap();
    let SuccessResult::RunStart(run) = client.call(&start("s", 1, 0)).await.unwrap() else {
        panic!()
    };
    let cancel = request(
        "c",
        RequestBody::RunCancel(
            session_id(),
            RunTarget {
                run_id: run.run_id,
                attempt_id: run.attempt_id,
            },
        ),
    );
    assert!(matches!(
        client.call(&cancel).await.unwrap(),
        SuccessResult::RunCancel(_)
    ));
    assert!(
        !client
            .events
            .iter()
            .any(|e| matches!(&e.body, EventBody::RunState(r) if r.state == RunState::Cancelled))
    );
    loop {
        if matches!(client.event().await.body, EventBody::RunState(r) if r.state == RunState::Cancelled)
        {
            break;
        }
    }
    assert!(matches!(
        client.call(&cancel).await.unwrap(),
        SuccessResult::RunCancel(_)
    ));
    drop(client.tx);
    task.await.unwrap().1.unwrap();
}

#[tokio::test]
async fn shutdown_repeated_queries_ready_rejects_new_work_and_exits_with_stdin_open() {
    let (_root, engine) = service();
    let (mut client, task) = connect(engine);
    let SuccessResult::Hello(h) = client
        .call(&request("h", RequestBody::Hello))
        .await
        .unwrap()
    else {
        panic!()
    };
    client.call(&draft("d", 0, "x")).await.unwrap();
    client.call(&start("s", 1, 0)).await.unwrap();
    let shutdown = request(
        "shutdown",
        RequestBody::ShutdownRequest(ShutdownRequest {
            engine_epoch: h.engine_epoch,
        }),
    );
    assert!(matches!(
        client.call(&shutdown).await.unwrap(),
        SuccessResult::ShutdownRequest(ShutdownResult {
            state: ShutdownState::Draining,
            ..
        })
    ));
    assert_eq!(
        client
            .call(&draft("rejected", 2, "x"))
            .await
            .unwrap_err()
            .code,
        ErrorCode::SessionBusy
    );
    sleep(Duration::from_millis(70)).await;
    assert!(matches!(
        client.call(&shutdown).await.unwrap(),
        SuccessResult::ShutdownRequest(ShutdownResult {
            state: ShutdownState::Ready,
            ..
        })
    ));
    let (engine, result) = timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.unwrap(), Exit::Ready);
    assert_eq!(
        engine.store.snapshot().unwrap().state.runs[0].run.state,
        RunState::Cancelled
    );
}

#[tokio::test]
async fn serialized_cancel_before_start_and_success_before_cancel() {
    for success_first in [false, true] {
        let (_root, mut engine) = service();
        hello(&mut engine);
        let run = begin(&mut engine);
        let (out, _, _workers) = transport::workers(
            tokio::io::empty(),
            tokio::io::sink(),
            Duration::from_secs(1),
        );
        if success_first {
            for _ in 0..4 {
                engine.step(&out).unwrap();
            }
        }
        direct(
            &mut engine,
            &request("cancel", RequestBody::RunCancel(session_id(), run)),
        )
        .unwrap();
        engine.step(&out).unwrap();
        let p = engine.store.snapshot().unwrap();
        assert_eq!(
            p.state.runs[0].run.state,
            if success_first {
                RunState::Succeeded
            } else {
                RunState::Cancelled
            }
        );
        assert_eq!(p.state.runs[0].operations.len(), usize::from(success_first));
    }
}

#[tokio::test]
async fn cancel_during_stream_saves_partial_without_success() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    let run = begin(&mut engine);
    let (out, _, _workers) = transport::workers(
        tokio::io::empty(),
        tokio::io::sink(),
        Duration::from_secs(1),
    );
    engine.step(&out).unwrap();
    engine.step(&out).unwrap();
    direct(
        &mut engine,
        &request("cancel", RequestBody::RunCancel(session_id(), run)),
    )
    .unwrap();
    assert_eq!(
        engine.store.snapshot().unwrap().state.runs[0].run.state,
        RunState::Cancelling
    );
    engine.step(&out).unwrap();
    let p = engine.store.snapshot().unwrap();
    assert_eq!(p.state.runs[0].run.state, RunState::Cancelled);
    assert_eq!(p.raw[1].message.content, engine.options.script[0]);
}

#[tokio::test(start_paused = true)]
async fn snapshot_ttl_old_cursor_and_epoch_are_rejected() {
    let (root, mut engine) = service();
    hello(&mut engine);
    let first = engine.snapshot(true).unwrap();
    direct(&mut engine, &draft("d", 0, "保存")).unwrap();
    let history = HistoryPage {
        snapshot_id: first.snapshot_id.clone(),
        cursor: first.history_start_cursor.clone(),
        limit: PageLimit::new(1).unwrap(),
    };
    assert_eq!(
        engine
            .history_page(&history)
            .unwrap()
            .session_revision
            .get(),
        0
    );
    let next = engine.snapshot(true).unwrap();
    assert_ne!(
        first.position.subscription_id,
        next.position.subscription_id
    );
    assert_eq!(
        engine.history_page(&history).unwrap_err().code,
        ErrorCode::RevisionConflict
    );
    let history = HistoryPage {
        snapshot_id: next.snapshot_id,
        cursor: next.history_start_cursor,
        limit: PageLimit::new(1).unwrap(),
    };
    tokio::time::advance(SNAPSHOT_TTL).await;
    assert_eq!(
        engine.history_page(&history).unwrap_err().code,
        ErrorCode::RevisionConflict
    );
    let epoch = engine.epoch.clone();
    drop(engine);
    let mut engine = Engine::reopen(&root, Options::default()).unwrap();
    hello(&mut engine);
    assert_ne!(epoch, engine.epoch);
    assert_eq!(
        direct(
            &mut engine,
            &request(
                "old-sub",
                RequestBody::SessionSubscribe(
                    session_id(),
                    Subscribe {
                        resume: Some(first.position)
                    }
                )
            )
        )
        .unwrap_err()
        .code,
        ErrorCode::RevisionConflict
    );
    assert_eq!(
        direct(
            &mut engine,
            &request(
                "old-shutdown",
                RequestBody::ShutdownRequest(ShutdownRequest {
                    engine_epoch: epoch
                })
            )
        )
        .unwrap_err()
        .code,
        ErrorCode::RevisionConflict
    );
}

#[test]
fn persisted_tool_results_keep_their_history_role() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    let run = begin(&mut engine);
    let operation = engine.active.as_ref().unwrap().operation.clone();
    engine.store.record_intent(&run, operation.clone()).unwrap();
    engine
        .store
        .finish_with_messages(
            &run,
            &operation,
            Observation::Succeeded,
            ResultId::new("full-result").unwrap(),
            vec![
                polaris_provider::Message::assistant_with_tool_calls(
                    "確認",
                    vec![polaris_provider::ToolCall {
                        id: "call".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path":"dummy"}),
                    }],
                ),
                polaris_provider::Message::tool_result("call", "tool body"),
                polaris_provider::Message::assistant("final body"),
            ],
        )
        .unwrap();
    let snapshot = engine.snapshot(false).unwrap();
    let page = engine
        .history_page(&HistoryPage {
            snapshot_id: snapshot.snapshot_id,
            cursor: snapshot.history_start_cursor,
            limit: PageLimit::new(4).unwrap(),
        })
        .unwrap();
    assert_eq!(
        page.messages
            .iter()
            .map(|message| message.role)
            .collect::<Vec<_>>(),
        vec![
            MessageRole::User,
            MessageRole::Assistant,
            MessageRole::Tool,
            MessageRole::Assistant
        ]
    );
    assert_eq!(page.messages[2].text, "tool body");
    assert_eq!(page.messages[3].text, "final body");
}

#[test]
fn bounded_saved_text_fits_history_frame_with_json_escaping() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    let run = begin(&mut engine);
    let operation = engine.active.as_ref().unwrap().operation.clone();
    engine.store.record_intent(&run, operation.clone()).unwrap();
    let body = "\0".repeat(21_800);
    engine
        .store
        .finish_with_messages(
            &run,
            &operation,
            Observation::Succeeded,
            ResultId::new("bounded-result").unwrap(),
            (0..4)
                .map(|_| polaris_provider::Message::assistant(body.clone()))
                .collect(),
        )
        .unwrap();
    let snapshot = engine.snapshot(false).unwrap();
    let page = engine
        .history_page(&HistoryPage {
            cursor: cursor(&snapshot.snapshot_id, 1),
            snapshot_id: snapshot.snapshot_id,
            limit: PageLimit::new(4).unwrap(),
        })
        .unwrap();
    assert_eq!(page.messages.len(), 4);
    assert!(page.next_cursor.is_none());
    assert!(page.messages.iter().all(|message| message.text == body));
    let frame = codec::encode(&Response {
        protocol_version: ProtocolVersion,
        client_id: ClientId::new("c".repeat(128)).unwrap(),
        request_id: RequestId::new("r".repeat(128)).unwrap(),
        outcome: Ok(SuccessResult::HistoryPage(page)),
    })
    .unwrap();
    assert!(frame.len() < codec::MAX_FRAME_BYTES / 2 + 4096);
}

#[tokio::test]
async fn logical_engine_restart_recovers_intent_no_replay_and_cancel_ack_loss() {
    for began in [false, true] {
        let (root, mut engine) = service();
        hello(&mut engine);
        let run = begin(&mut engine);
        let (out, _, _workers) = transport::workers(
            tokio::io::empty(),
            tokio::io::sink(),
            Duration::from_secs(1),
        );
        if began {
            engine.step(&out).unwrap();
            engine.step(&out).unwrap();
        }
        let cancel = request("cancel", RequestBody::RunCancel(session_id(), run));
        let ack = direct(&mut engine, &cancel).unwrap();
        let old_epoch = engine.epoch.clone();
        drop(engine); // 論理engine喪失。TempDirはrootが保持する。
        let mut engine = Engine::reopen(&root, Options::default()).unwrap();
        hello(&mut engine);
        assert_ne!(old_epoch, engine.epoch);
        let before = engine.store.snapshot().unwrap();
        assert_eq!(direct(&mut engine, &cancel).unwrap(), ack);
        assert_eq!(engine.store.snapshot().unwrap().marker, before.marker);
        assert!(engine.active.is_none());
        assert_eq!(before.raw.len(), 1);
        assert_eq!(
            before.state.runs[0].run.state,
            if began {
                RunState::OutcomeUnknown
            } else {
                RunState::Cancelled
            }
        );
        let replay = direct(&mut engine, &start("start", 1, 0)).unwrap();
        assert!(matches!(replay, SuccessResult::RunStart(_)));
        engine.step(&out).unwrap();
        assert!(engine.active.is_none());
    }
}

#[tokio::test]
async fn logical_restart_preserves_saved_answer_and_original_config() {
    let (root, mut engine) = service();
    hello(&mut engine);
    let a = configure("a", 0, "scripted-alt");
    let accepted = direct(&mut engine, &a).unwrap();
    direct(&mut engine, &configure("b", 1, "scripted")).unwrap();
    direct(&mut engine, &draft("d", 0, "x")).unwrap();
    direct(&mut engine, &start("s", 1, 2)).unwrap();
    let (out, _, _workers) = transport::workers(
        tokio::io::empty(),
        tokio::io::sink(),
        Duration::from_secs(1),
    );
    for _ in 0..4 {
        engine.step(&out).unwrap();
    }
    let before = engine.store.snapshot().unwrap();
    drop(engine);
    let mut engine = Engine::reopen(&root, Options::default()).unwrap();
    hello(&mut engine);
    assert_eq!(direct(&mut engine, &a).unwrap(), accepted);
    let after = engine.store.snapshot().unwrap();
    assert_eq!(before.marker, after.marker);
    assert_eq!(after.raw[1].message.content, engine.options.script.concat());
    assert!(engine.active.is_none());
}

#[tokio::test]
async fn unread_os_output_still_accepts_cancel_and_settles_by_deadline() {
    let (root, mut engine) = service();
    engine.options.output_deadline = Duration::from_millis(500);
    engine.options.script = vec!["x".into(); 200];
    // 開始/target取得だけを同じ所有者で行い、以後の制御は実pipeで送る。
    hello(&mut engine);
    let run = begin(&mut engine);
    let (mut client, task) = connect(engine);
    client.send(&draft("large", 2, &"あ".repeat(10_000))).await;
    for n in 0..4 {
        client
            .send(&request(
                &format!("snapshot-{n}"),
                RequestBody::SessionSnapshot(session_id()),
            ))
            .await;
    }
    let cancel = request("cancel", RequestBody::RunCancel(session_id(), run));
    client.send(&cancel).await;
    // stdoutを一切読まない。queueには余裕があるがOS pipeへのwriteは停止する。
    let (engine, result) = timeout(Duration::from_secs(3), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.unwrap(), Exit::OutputClosed);
    let p = engine.store.snapshot().unwrap();
    assert_eq!(p.state.runs[0].run.state, RunState::Cancelled);
    assert!(
        engine
            .store
            .request_status(&session_id(), &cancel.client_id, &cancel.request_id)
            .unwrap()
            .is_some()
    );
    drop(engine);
    let reopened = Engine::reopen(&root, Options::default()).unwrap();
    assert_eq!(
        reopened.store.snapshot().unwrap().state.runs[0].run.state,
        RunState::Cancelled
    );
}

#[tokio::test(start_paused = true)]
async fn queue_count_and_bytes_are_bounded_without_silent_drop() {
    let (out, _input, workers) = transport::workers(
        tokio::io::empty(),
        tokio::io::sink(),
        Duration::from_secs(1),
    );
    // workerをpollせず容量まで埋める。seq付与後の飽和は必ずErrになる。
    let (_root, mut engine) = service();
    hello(&mut engine);
    engine.snapshot(true).unwrap();
    let draft = engine.store.snapshot().unwrap().state.draft;
    for _ in 0..255 {
        engine
            .emit(EventBody::DraftUpdated(draft.clone()), &out)
            .unwrap();
    }
    assert!(matches!(
        engine.emit(EventBody::DraftUpdated(draft), &out),
        Err(ServiceError::OutputClosed)
    ));
    drop(workers);
    let (out, _input, _workers) = transport::workers(
        tokio::io::empty(),
        tokio::io::sink(),
        Duration::from_secs(1),
    );
    let big = "x".repeat(900_000);
    for _ in 0..4 {
        out.send(&big).unwrap();
    }
    assert!(matches!(out.send(&big), Err(ServiceError::OutputClosed)));
}

#[tokio::test(start_paused = true)]
async fn frame_deadline_is_not_reset_after_partial_writes() {
    use std::{
        pin::Pin,
        task::{Context, Poll},
    };
    use tokio::{io::AsyncWrite, time::Sleep};
    struct Dribble {
        delay: Pin<Box<Sleep>>,
        writes: std::sync::Arc<AtomicU64>,
    }
    impl AsyncWrite for Dribble {
        fn poll_write(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            bytes: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            use std::future::Future;
            if self.delay.as_mut().poll(cx).is_pending() {
                return Poll::Pending;
            }
            self.writes.fetch_add(1, Ordering::Relaxed);
            self.delay
                .as_mut()
                .reset(Instant::now() + Duration::from_millis(20));
            Poll::Ready(Ok(bytes.len().min(1)))
        }
        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    let writes = std::sync::Arc::new(AtomicU64::new(0));
    let writer = Dribble {
        delay: Box::pin(sleep(Duration::from_millis(20))),
        writes: writes.clone(),
    };
    let (out, _input, mut workers) =
        transport::workers(tokio::io::empty(), writer, Duration::from_millis(55));
    out.send(&"long-frame").unwrap();
    loop {
        if let Some(Ok(WorkerExit::Writer(result))) = workers.join_next().await {
            assert!(matches!(result, Err(ServiceError::OutputClosed)));
            break;
        }
    }
    assert_eq!(writes.load(Ordering::Relaxed), 2);
}

#[tokio::test]
async fn eof_saves_owned_partial_work() {
    let (_root, engine) = service();
    let (mut client, task) = connect(engine);
    client
        .call(&request("hello", RequestBody::Hello))
        .await
        .unwrap();
    client
        .call(&request(
            "sub",
            RequestBody::SessionSubscribe(session_id(), Subscribe::default()),
        ))
        .await
        .unwrap();
    client.call(&draft("d", 0, "x")).await.unwrap();
    client.call(&start("s", 1, 0)).await.unwrap();
    loop {
        if matches!(client.event().await.body, EventBody::MessageDelta(_)) {
            break;
        }
    }
    drop(client.tx);
    let (engine, result) = task.await.unwrap();
    assert_eq!(result.unwrap(), Exit::Eof);
    let p = engine.store.snapshot().unwrap();
    assert_eq!(p.state.runs[0].run.state, RunState::Cancelled);
    assert_eq!(p.raw[1].message.content, engine.options.script[0]);
}

#[tokio::test]
async fn unavailable_store_cannot_report_shutdown_ready() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    engine.store.tombstone().unwrap();
    let shutdown = request(
        "shutdown",
        RequestBody::ShutdownRequest(ShutdownRequest {
            engine_epoch: engine.epoch.clone(),
        }),
    );
    assert!(matches!(
        direct(&mut engine, &shutdown).unwrap(),
        SuccessResult::ShutdownRequest(ShutdownResult {
            state: ShutdownState::Draining,
            ..
        })
    ));
    let (out, _, _workers) = transport::workers(
        tokio::io::empty(),
        tokio::io::sink(),
        Duration::from_secs(1),
    );
    assert!(engine.settle(&out).is_err());
    assert!(!engine.ready);
}

pub(super) struct Pause {
    pub request_id: &'static str,
    pub entered: tokio::sync::oneshot::Sender<()>,
    pub release: tokio::sync::oneshot::Receiver<()>,
}
fn pause(
    engine: &mut Engine,
) -> (
    tokio::sync::oneshot::Receiver<()>,
    tokio::sync::oneshot::Sender<()>,
) {
    let (entered, wait) = tokio::sync::oneshot::channel();
    let (release, blocked) = tokio::sync::oneshot::channel();
    engine.pause = Some(Pause {
        request_id: "blocked",
        entered,
        release: blocked,
    });
    (wait, release)
}

#[tokio::test]
async fn controller_identity_and_status_target_are_checked_before_ledger() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    let mut foreign = draft("d", 0, "foreign");
    foreign.client_id = ClientId::new("other").unwrap();
    assert_eq!(
        direct(&mut engine, &foreign).unwrap_err().code,
        ErrorCode::PermissionDenied
    );
    let status = request(
        "status",
        RequestBody::RequestStatus(
            session_id(),
            RequestStatus {
                client_id: foreign.client_id,
                request_id: foreign.request_id,
            },
        ),
    );
    assert_eq!(
        direct(&mut engine, &status).unwrap_err().code,
        ErrorCode::PermissionDenied
    );
    assert_eq!(
        engine
            .store
            .snapshot()
            .unwrap()
            .marker
            .session_revision
            .get(),
        0
    );
}

#[tokio::test]
async fn queued_cancel_terminal_is_emitted_before_replacing_active() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    let old = begin(&mut engine);
    direct(
        &mut engine,
        &request("cancel", RequestBody::RunCancel(session_id(), old.clone())),
    )
    .unwrap();
    direct(&mut engine, &draft("next", 2, "next")).unwrap();
    let mut events = Vec::new();
    engine
        .handle(&start("next-start", 3, 0), &mut events)
        .unwrap();
    assert!(
        matches!(&events[0], EventBody::RunState(r) if r.run_id == old.run_id && r.state == RunState::Cancelled)
    );
    assert_ne!(engine.active.as_ref().unwrap().target.run_id, old.run_id);
}

#[tokio::test]
async fn cancellation_reserve_cannot_be_consumed_by_old_terminal_run() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    let old = begin(&mut engine);
    let (out, _, _workers) = transport::workers(
        tokio::io::empty(),
        tokio::io::sink(),
        Duration::from_secs(1),
    );
    for _ in 0..4 {
        engine.step(&out).unwrap();
    }
    direct(&mut engine, &draft("next", 2, "next")).unwrap();
    let SuccessResult::RunStart(current) = direct(&mut engine, &start("next-start", 3, 0)).unwrap()
    else {
        panic!()
    };
    while engine.store.snapshot().unwrap().state.requests.len() < REQUEST_RECORDS - 1 {
        let p = engine.store.snapshot().unwrap();
        direct(
            &mut engine,
            &draft(
                &format!("fill-{}", p.state.requests.len()),
                p.state.draft.draft_revision.get(),
                "x",
            ),
        )
        .unwrap();
    }
    assert_eq!(
        direct(
            &mut engine,
            &request("old-cancel", RequestBody::RunCancel(session_id(), old))
        )
        .unwrap_err()
        .code,
        ErrorCode::CapabilityUnavailable
    );
    let cancel = request(
        "current-cancel",
        RequestBody::RunCancel(
            session_id(),
            RunTarget {
                run_id: current.run_id,
                attempt_id: current.attempt_id,
            },
        ),
    );
    let ack = direct(&mut engine, &cancel).unwrap();
    assert_eq!(direct(&mut engine, &cancel).unwrap(), ack);
    assert_eq!(
        engine.store.snapshot().unwrap().state.requests.len(),
        REQUEST_RECORDS
    );
}

#[tokio::test]
async fn eof_during_blocked_persistence_suppresses_queued_mutation_and_start() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    let (entered, release) = pause(&mut engine);
    let (mut client, task) = connect(engine);
    client.send(&draft("blocked", 0, "must not save")).await;
    entered.await.unwrap();
    client.send(&start("queued", 1, 0)).await;
    drop(client.tx);
    // current-thread reactorがblocking所有者とは独立して動くことも確認する。
    sleep(Duration::from_millis(40)).await;
    assert!(!task.is_finished());
    release.send(()).unwrap();
    let (engine, result) = timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.unwrap(), Exit::Eof);
    let p = engine.store.snapshot().unwrap();
    assert_eq!(p.marker.session_revision.get(), 0);
    assert!(p.state.runs.is_empty());
}

#[tokio::test]
async fn queued_cancel_is_observed_while_persistence_is_blocked() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    let target = begin(&mut engine);
    let (entered, release) = pause(&mut engine);
    let (mut client, task) = connect(engine);
    client.send(&configure("blocked", 0, "scripted-alt")).await;
    entered.await.unwrap();
    client
        .send(&request(
            "cancel",
            RequestBody::RunCancel(session_id(), target),
        ))
        .await;
    sleep(Duration::from_millis(80)).await;
    release.send(()).unwrap();
    client.response().await.outcome.unwrap();
    assert!(matches!(
        client.response().await.outcome.unwrap(),
        SuccessResult::RunCancel(_)
    ));
    drop(client.tx);
    let (engine, result) = task.await.unwrap();
    result.unwrap();
    assert_eq!(
        engine.store.snapshot().unwrap().state.runs[0].run.state,
        RunState::Cancelled
    );
    assert!(
        engine.store.snapshot().unwrap().state.runs[0]
            .operations
            .is_empty()
    );
}

#[tokio::test]
async fn writer_deadline_remains_live_while_persistence_owner_is_blocked() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    engine.options.output_deadline = Duration::from_millis(80);
    direct(&mut engine, &draft("large", 0, &"界".repeat(10_000))).unwrap();
    let (entered, release) = pause(&mut engine);
    let (mut client, task) = connect(engine);
    for n in 0..4 {
        client
            .send(&request(
                &format!("snapshot-{n}"),
                RequestBody::SessionSnapshot(session_id()),
            ))
            .await;
    }
    client.send(&configure("blocked", 0, "scripted-alt")).await;
    entered.await.unwrap();
    sleep(Duration::from_millis(180)).await;
    release.send(()).unwrap();
    let (engine, result) = timeout(Duration::from_secs(2), task)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.unwrap(), Exit::OutputClosed);
    assert_eq!(
        engine
            .store
            .snapshot()
            .unwrap()
            .state
            .configuration
            .configuration_revision
            .get(),
        0
    );
}

#[tokio::test]
async fn aborting_serve_signals_stop_before_worker_channels_drop() {
    let (root, mut engine) = service();
    hello(&mut engine);
    let (entered, release) = pause(&mut engine);
    let (mut client, task) = connect(engine);
    client.send(&draft("blocked", 0, "discarded")).await;
    entered.await.unwrap();
    client.send(&start("queued", 1, 0)).await;
    task.abort();
    assert!(matches!(task.await, Err(error) if error.is_cancelled()));
    release.send(()).unwrap();
    let writer = timeout(Duration::from_secs(2), async {
        loop {
            match root.open(&project_id(), &session_id()) {
                Ok(writer) => break writer,
                Err(StoreError::Busy) => sleep(Duration::from_millis(5)).await,
                Err(e) => panic!("{e}"),
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(writer.snapshot().unwrap().marker.session_revision.get(), 0);
}

#[test]
fn slow_step_dispatch_serves_queued_normal_request_without_starving_steps() {
    let interval = Duration::from_millis(50);
    let start = Instant::now();
    let mut dispatch = Dispatch::new(start, interval);
    let mut queued = VecDeque::new();
    let mut now = start + interval;
    for index in 0..8 {
        let id = format!("normal-{index}");
        queued.push_back(draft(&id, 0, "queued"));
        // Even with a queued request, a due Step must make progress.
        assert!(matches!(
            dispatch.next(now, &mut queued),
            Some(Command::Step)
        ));
        // Complete that Step strictly after its next deadline. This is the
        // causal timing condition, not a sleep sensitive to host load.
        now += interval + Duration::from_millis(1);
        assert!(now > dispatch.next_step);
        assert!(
            matches!(dispatch.next(now, &mut queued), Some(Command::Request(r)) if r.request_id.as_str() == id),
            "a completed slow Step must yield to the already queued normal request"
        );
        assert!(queued.is_empty());
        // A quick request must not restart/postpone the overdue Step timer.
        now += Duration::from_millis(1);
    }
}

#[test]
fn dispatch_prioritizes_controls_and_preserves_their_settlement_step() {
    let interval = Duration::from_millis(50);
    let start = Instant::now();
    let mut dispatch = Dispatch::new(start, interval);
    let mut queued = VecDeque::from([draft("normal", 0, "queued")]);
    let mut now = start + interval;
    for index in 0..4 {
        let id = format!("control-{index}");
        let body = if index % 2 == 0 {
            RequestBody::RunCancel(
                session_id(),
                RunTarget {
                    run_id: RunId::new("run").unwrap(),
                    attempt_id: AttemptId::new("attempt").unwrap(),
                },
            )
        } else {
            RequestBody::ShutdownRequest(ShutdownRequest {
                engine_epoch: EngineEpoch::new("epoch").unwrap(),
            })
        };
        queued.push_front(request(&id, body));
        assert!(
            matches!(dispatch.next(now, &mut queued), Some(Command::Request(r)) if r.request_id.as_str() == id)
        );
        // The owner's successful control response requests immediate settling.
        dispatch.settle_soon = true;
        queued.push_front(request(
            "next-control",
            RequestBody::ShutdownRequest(ShutdownRequest {
                engine_epoch: EngineEpoch::new("epoch").unwrap(),
            }),
        ));
        assert!(matches!(
            dispatch.next(now, &mut queued),
            Some(Command::Step)
        ));
        now += interval + Duration::from_millis(1);
        assert!(
            matches!(dispatch.next(now, &mut queued), Some(Command::Request(r)) if r.request_id.as_str() == "next-control")
        );
        // Model a Ready/error control response, which does not request settling.
        assert!(!dispatch.settle_soon);
    }
    assert_eq!(queued.front().unwrap().request_id.as_str(), "normal");
}

#[tokio::test]
async fn continuous_status_polling_does_not_starve_cancel_or_shutdown_ready() {
    let (_root, engine) = service();
    let (mut client, task) = connect(engine);
    let SuccessResult::Hello(h) = client
        .call(&request("h", RequestBody::Hello))
        .await
        .unwrap()
    else {
        panic!()
    };
    client
        .call(&request(
            "sub",
            RequestBody::SessionSubscribe(session_id(), Subscribe::default()),
        ))
        .await
        .unwrap();
    client.call(&draft("d", 0, "x")).await.unwrap();
    let SuccessResult::RunStart(run) = client.call(&start("s", 1, 0)).await.unwrap() else {
        panic!()
    };
    client
        .call(&request(
            "cancel",
            RequestBody::RunCancel(
                session_id(),
                RunTarget {
                    run_id: run.run_id,
                    attempt_id: run.attempt_id,
                },
            ),
        ))
        .await
        .unwrap();
    let status = request(
        "status",
        RequestBody::RequestStatus(
            session_id(),
            RequestStatus {
                client_id: ClientId::new("client").unwrap(),
                request_id: RequestId::new("s").unwrap(),
            },
        ),
    );
    timeout(Duration::from_secs(2), async {
        loop {
            client.call(&status).await.unwrap();
            if client.events.iter().any(
                |e| matches!(&e.body, EventBody::RunState(r) if r.state == RunState::Cancelled),
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    let shutdown = request(
        "shutdown",
        RequestBody::ShutdownRequest(ShutdownRequest {
            engine_epoch: h.engine_epoch,
        }),
    );
    timeout(Duration::from_secs(2), async {
        loop {
            if matches!(
                client.call(&shutdown).await.unwrap(),
                SuccessResult::ShutdownRequest(ShutdownResult {
                    state: ShutdownState::Ready,
                    ..
                })
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(task.await.unwrap().1.unwrap(), Exit::Ready);
}

#[tokio::test]
async fn continuous_reads_do_not_starve_finite_fake_steps() {
    let (_root, engine) = service();
    let (mut client, task) = connect(engine);
    client
        .call(&request("h", RequestBody::Hello))
        .await
        .unwrap();
    client.call(&draft("d", 0, "x")).await.unwrap();
    client.call(&start("s", 1, 0)).await.unwrap();
    let status = request(
        "status",
        RequestBody::RequestStatus(
            session_id(),
            RequestStatus {
                client_id: ClientId::new("client").unwrap(),
                request_id: RequestId::new("s").unwrap(),
            },
        ),
    );
    timeout(Duration::from_secs(2), async {
        loop {
            if matches!(
                client.call(&status).await.unwrap(),
                SuccessResult::RequestStatus(RequestStatusResult::Completed { .. })
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();
    drop(client.tx);
    task.await.unwrap().1.unwrap();
}
