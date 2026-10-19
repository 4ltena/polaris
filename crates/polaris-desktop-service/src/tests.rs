//! 実OS匿名pipeで制御・本文・停止を往復し、論理engine再起動を別途検証する。

use super::*;
use crate::transport::read_frame;
use polaris_desktop_protocol::{codec, request::*};
use tokio::{io::AsyncWriteExt, net::unix::pipe, task::JoinHandle, time::sleep};

// A failed assertion must not leave the process-wide reclaimer waiting on a
// synthetic cleanup forever and prevent unrelated production-owner tests exiting.
struct DeferredCleanupRelease(Arc<AtomicBool>);
impl DeferredCleanupRelease {
    fn release(&self) {
        self.0.store(true, Ordering::Release);
    }
}
impl Drop for DeferredCleanupRelease {
    fn drop(&mut self) {
        self.release();
    }
}

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
        let released = DeferredCleanupRelease(
            engine
                .active
                .as_mut()
                .unwrap()
                .execution
                .inject_unconfirmed_cleanup(),
        );
        engine.failed = storage_error;
        let (client, task) = connect(engine);
        drop(client.tx);
        let (mut engine, outcome) = timeout(Duration::from_secs(5), task)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(outcome.is_err(), storage_error);
        assert!(!engine.ready);
        released.release();
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
    let released = DeferredCleanupRelease(
        engine
            .active
            .as_mut()
            .unwrap()
            .execution
            .inject_unconfirmed_cleanup(),
    );
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
    released.release();
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
                mcp_prompt: None,
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

#[tokio::test]
async fn fake_engine_steps_legacy_run_without_an_admission_owner() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    let target = begin(&mut engine);
    assert!(engine.admission.is_none());
    let (out, _input, _workers) = transport::workers(
        tokio::io::empty(),
        tokio::io::sink(),
        Duration::from_secs(1),
    );

    engine.step(&out).unwrap();

    let run = engine
        .store
        .snapshot()
        .unwrap()
        .state
        .runs
        .into_iter()
        .find(|run| run.run.run_id == target.run_id)
        .unwrap();
    assert_eq!(run.run.state, RunState::Running);
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
    let mut mixed_version = request("mixed-version", RequestBody::SessionSnapshot(session_id()));
    mixed_version.protocol_version = ProtocolVersion::V1;
    assert_eq!(
        client.call(&mixed_version).await.unwrap_err().code,
        ErrorCode::UnsupportedVersion
    );
    let mut value = serde_json::to_value(request("unknown", RequestBody::Hello)).unwrap();
    value["protocol_version"] = 3.into();
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

#[test]
fn run_details_pages_rebuild_every_byte_and_refuse_foreign_or_expired_cursors() {
    use polaris_desktop_protocol::run_details::RunDetailsRequest;
    let (_root, mut engine) = service();
    hello(&mut engine);
    let run = begin(&mut engine);
    let operation = engine.active.as_ref().unwrap().operation.clone();
    engine.store.record_intent(&run, operation.clone()).unwrap();
    let body = format!("a{}", "あ".repeat(40_000));
    engine
        .store
        .finish_with_messages(
            &run,
            &operation,
            Observation::Succeeded,
            ResultId::new("details-result").unwrap(),
            vec![
                polaris_provider::Message::assistant_with_tool_calls(
                    "",
                    vec![polaris_provider::ToolCall {
                        id: "call".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path":"dummy"}),
                    }],
                ),
                polaris_provider::Message::tool_result("call", body),
                polaris_provider::Message::assistant("完了"),
            ],
        )
        .unwrap();
    let before = engine.store.snapshot().unwrap().marker;
    let oracle = engine.store.project_run_details(&run).unwrap();
    let ask = |engine: &mut Engine,
               snapshot_id: Option<String>,
               cursor: Option<String>,
               target: &RunTarget| {
        direct(
            engine,
            &request(
                "details",
                RequestBody::RunDetails(
                    session_id(),
                    RunDetailsRequest {
                        run_id: target.run_id.clone(),
                        attempt_id: target.attempt_id.clone(),
                        snapshot_id,
                        cursor,
                    },
                ),
            ),
        )
    };
    let mut rebuilt: Vec<(String, String)> = Vec::new();
    let mut next = None;
    let mut snapshot = None;
    let mut pages = 0;
    loop {
        let Ok(SuccessResult::RunDetails(page)) =
            ask(&mut engine, snapshot.clone(), next.clone(), &run)
        else {
            panic!("page");
        };
        pages += 1;
        assert!(page.fragments.len() <= 4);
        assert!(page.fragments.iter().map(|f| f.text.len()).sum::<usize>() <= 64 * 1024);
        for fragment in page.fragments {
            match rebuilt.last_mut() {
                Some((id, text)) if *id == fragment.entry_id => {
                    assert_eq!(fragment.offset.get() as usize, text.len());
                    text.push_str(&fragment.text);
                }
                _ => {
                    assert_eq!(fragment.offset.get(), 0);
                    rebuilt.push((fragment.entry_id, fragment.text));
                }
            }
        }
        snapshot = Some(page.snapshot_id);
        next = page.next_cursor;
        if next.is_none() {
            break;
        }
    }
    assert!(pages >= 2);
    let expected: Vec<_> = oracle
        .entries
        .iter()
        .map(|e| (e.id.clone(), e.text.clone()))
        .collect();
    assert_eq!(rebuilt, expected);

    let Ok(SuccessResult::RunDetails(first)) = ask(&mut engine, None, None, &run) else {
        panic!("first");
    };
    let cursor = first.next_cursor.clone().unwrap();
    let id = Some(first.snapshot_id.clone());
    let other = RunTarget {
        run_id: run.run_id.clone(),
        attempt_id: AttemptId::new("other-attempt").unwrap(),
    };
    let (entry, offset) = cursor.rsplit_once(':').unwrap();
    let mid_char = format!("{entry}:{}", offset.parse::<usize>().unwrap() + 1);
    for (snapshot, cursor, target) in [
        (id.clone(), Some(cursor.clone()), &other),
        (id.clone(), Some(mid_char), &run),
        (Some("details-stale".into()), Some(cursor.clone()), &run),
    ] {
        assert_eq!(
            ask(&mut engine, snapshot, cursor, target).unwrap_err().code,
            ErrorCode::RevisionConflict
        );
    }
    engine.details.as_mut().unwrap().expires = Instant::now();
    assert_eq!(
        ask(&mut engine, id, Some(cursor), &run).unwrap_err().code,
        ErrorCode::RevisionConflict
    );
    assert_eq!(
        ask(&mut engine, None, None, &other).unwrap_err().code,
        ErrorCode::NotFound
    );
    assert_eq!(engine.store.snapshot().unwrap().marker, before);
}

fn production_engine() -> (tempfile::TempDir, ServiceConfig, Engine) {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let root = tempfile::tempdir().unwrap();
    let store_root = root.path().canonicalize().unwrap();
    std::fs::set_permissions(&store_root, std::fs::Permissions::from_mode(0o700)).unwrap();
    let project = store_root.join("source");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(project.join(".polaris")).unwrap();
    let config = ServiceConfig {
        store_root,
        project_id: project_id(),
        session_id: session_id(),
    };
    let identity = std::fs::metadata(&project).unwrap();
    let mut service = DesktopService::open(config.clone())
        .unwrap()
        .with_management_project(project, (identity.dev(), identity.ino()))
        .unwrap();
    (root, config, service.engine.take().unwrap())
}

#[cfg(target_os = "linux")]
#[test]
fn linux_production_workflow_configure_refuses_before_lane_or_store_mutation() {
    let (_root, _config, mut engine) = production_engine();
    let SuccessResult::Hello(hello) =
        direct(&mut engine, &request("hello", RequestBody::Hello)).unwrap()
    else {
        panic!("hello result")
    };
    assert!(!hello.capabilities.contains(&Capability::WorkflowConfigure));
    direct(
        &mut engine,
        &request(
            "subscribe",
            RequestBody::SessionSubscribe(session_id(), Subscribe::default()),
        ),
    )
    .unwrap();
    assert!(engine.subscription.is_some());

    let before = engine.store.snapshot().unwrap();
    let current_session = engine.session.clone();
    let current_subscription = engine.subscription.clone();
    let other_session = engine.other_lane.as_ref().map(|lane| lane.session.clone());
    let mut events = Vec::new();
    let denied = engine
        .handle(
            &request(
                "workflow-unavailable",
                RequestBody::WorkflowConfigure(
                    SessionId::new("missing-session").unwrap(),
                    WorkflowConfigure {
                        expected_session_revision: DecimalU64::new(0),
                        enabled: true,
                        mode: WorkflowMode::Manual,
                        initial_phase: WorkflowPhase::General,
                    },
                ),
            ),
            &mut events,
        )
        .unwrap_err();
    assert_eq!(denied.code, ErrorCode::CapabilityUnavailable);
    assert!(events.is_empty());
    assert_eq!(engine.session, current_session);
    assert_eq!(engine.subscription, current_subscription);
    assert_eq!(
        engine.other_lane.as_ref().map(|lane| lane.session.clone()),
        other_session
    );
    let after = engine.store.snapshot().unwrap();
    assert_eq!(after.marker, before.marker);
    assert_eq!(after.state, before.state);
    assert_eq!(
        serde_json::to_vec(&after.raw).unwrap(),
        serde_json::to_vec(&before.raw).unwrap()
    );
}

#[test]
fn production_input_refuses_project_management_blocker_before_admission() {
    use polaris_core::{
        management_effect::{
            ManagementEffectHashV1, ManagementEffectScopeV1, ManagementEffectStoreV1,
            ManagementInventoryIdV1, ManagementOperationIdV1, ManagementPermissionScopeV1,
            ManagementRewindDispositionV1, PrepareManagementEffectV1,
        },
        project_mutation::ProjectMutationOwnerV1,
    };

    let (root, _config, mut engine) = production_engine();
    hello(&mut engine);
    let project = root.path().join("source");
    let owner = ProjectMutationOwnerV1::for_project(&project).unwrap();
    let mutation = owner.try_mutation().unwrap();
    let effects = ManagementEffectStoreV1::open(&project, &mutation).unwrap();
    effects
        .prepare(
            &mutation,
            effects.cursor().unwrap(),
            PrepareManagementEffectV1 {
                operation_id: ManagementOperationIdV1::random().unwrap(),
                inventory: ManagementInventoryIdV1::parse("C12").unwrap(),
                scope: ManagementEffectScopeV1 {
                    project: effects.project_scope_hash(),
                    session: None,
                    generation: 0,
                    revision: 0,
                    permission: ManagementPermissionScopeV1::Project,
                    account_binding: None,
                },
                request_hash: ManagementEffectHashV1::digest(b"blocked desktop input"),
                rewind: ManagementRewindDispositionV1::IrreversibleLocal,
            },
        )
        .unwrap();
    drop(mutation);

    let before = engine.store.snapshot().unwrap();
    let manager_before = engine
        .admission
        .as_ref()
        .unwrap()
        .persisted_state()
        .unwrap();
    let result = direct(
        &mut engine,
        &submit_input(
            "blocked-input",
            session_id(),
            0,
            0,
            InputRoute::Current,
            "blocked-admission",
            "must remain unsaved",
        ),
    );
    assert_eq!(result.unwrap_err().code, ErrorCode::RecoveryRequired);
    let after = engine.store.snapshot().unwrap();
    assert_eq!(after.marker, before.marker);
    assert_eq!(
        serde_json::to_vec(&after.raw).unwrap(),
        serde_json::to_vec(&before.raw).unwrap()
    );
    let manager_after = engine
        .admission
        .as_ref()
        .unwrap()
        .persisted_state()
        .unwrap();
    assert_eq!(manager_after, manager_before);
}

#[test]
fn production_input_refuses_torn_c10_before_admission_without_writes() {
    use std::os::unix::fs::PermissionsExt;

    let (root, _config, mut engine) = production_engine();
    hello(&mut engine);
    assert!(engine.input_management_guard().unwrap().is_some());
    let state = polaris_core::project::state_dir(&root.path().join("source")).unwrap();
    let sync = state.join("sync");
    let intents = sync.join("remote-intents-v2");
    for directory in [&sync, &intents, &intents.join("archive")] {
        std::fs::create_dir(directory).unwrap();
        std::fs::set_permissions(directory, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    let torn = intents.join(format!("{}.json", "a".repeat(64)));
    for (path, bytes) in [
        (intents.join("owner.lock"), b"".as_slice()),
        (torn.clone(), b"{torn".as_slice()),
    ] {
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    let before = engine.store.snapshot().unwrap();
    let manager_before = engine
        .admission
        .as_ref()
        .unwrap()
        .persisted_state()
        .unwrap();
    let cursor = engine
        .management_effects
        .as_ref()
        .unwrap()
        .cursor()
        .unwrap();
    let result = direct(
        &mut engine,
        &submit_input(
            "torn-c10-input",
            session_id(),
            0,
            0,
            InputRoute::Current,
            "torn-c10-admission",
            "must remain unsaved",
        ),
    );
    assert_eq!(result.unwrap_err().code, ErrorCode::RecoveryRequired);
    let after = engine.store.snapshot().unwrap();
    assert_eq!(after.marker, before.marker);
    assert_eq!(
        serde_json::to_vec(&after.raw).unwrap(),
        serde_json::to_vec(&before.raw).unwrap()
    );
    assert_eq!(
        engine
            .admission
            .as_ref()
            .unwrap()
            .persisted_state()
            .unwrap(),
        manager_before
    );
    assert_eq!(
        engine
            .management_effects
            .as_ref()
            .unwrap()
            .cursor()
            .unwrap(),
        cursor
    );
    assert_eq!(std::fs::read(torn).unwrap(), b"{torn");
}

#[test]
fn production_input_guard_allows_an_existing_project_activity() {
    use polaris_core::project_mutation::ProjectMutationOwnerV1;

    let (root, _config, engine) = production_engine();
    let owner = ProjectMutationOwnerV1::for_project(&root.path().join("source")).unwrap();
    let _active_run = owner.try_activity().unwrap();
    assert!(engine.input_management_guard().unwrap().is_some());
}

#[test]
fn production_input_refuses_replaced_source_identity_before_admission() {
    use std::os::unix::fs::symlink;

    let (root, _config, mut engine) = production_engine();
    hello(&mut engine);
    let source = root.path().join("source");
    let original = root.path().join("source-original");
    let replacement = root.path().join("replacement");
    std::fs::rename(&source, &original).unwrap();
    std::fs::create_dir(&replacement).unwrap();
    symlink(&replacement, &source).unwrap();

    let before = engine.store.snapshot().unwrap();
    let manager_before = engine
        .admission
        .as_ref()
        .unwrap()
        .persisted_state()
        .unwrap();
    let result = direct(
        &mut engine,
        &submit_input(
            "replaced-source-input",
            session_id(),
            0,
            0,
            InputRoute::Current,
            "replaced-source-admission",
            "must remain unsaved",
        ),
    );
    assert_eq!(result.unwrap_err().code, ErrorCode::RecoveryRequired);
    let after = engine.store.snapshot().unwrap();
    assert_eq!(after.marker, before.marker);
    assert_eq!(
        serde_json::to_vec(&after.raw).unwrap(),
        serde_json::to_vec(&before.raw).unwrap()
    );
    let manager_after = engine
        .admission
        .as_ref()
        .unwrap()
        .persisted_state()
        .unwrap();
    assert_eq!(manager_after, manager_before);

    std::fs::remove_file(&source).unwrap();
    std::fs::rename(&original, &source).unwrap();
    assert!(engine.input_management_guard().unwrap().is_some());
}

fn submit_input(
    id: &str,
    session: SessionId,
    manager_revision: u64,
    session_revision: u64,
    route: InputRoute,
    admission: &str,
    text: &str,
) -> Request {
    request(
        id,
        RequestBody::InputSubmit(
            session,
            InputSubmit::Text {
                expected_manager_revision: DecimalU64::new(manager_revision),
                expected_session_revision: DecimalU64::new(session_revision),
                expected_goal_revision: DecimalU64::new(0),
                route,
                attachment_ids: Vec::new(),
                admission_id: Some(AdmissionId::new(admission).unwrap()),
                text: text.into(),
            },
        ),
    )
}

#[test]
fn input_submit_is_idempotent_and_restart_never_resends_a_running_admission() {
    let (_root, config, mut engine) = production_engine();
    hello(&mut engine);
    let submit = submit_input(
        "input-current",
        session_id(),
        0,
        0,
        InputRoute::Current,
        "adm-client-current",
        "implement this",
    );
    let mut events = Vec::new();
    let SuccessResult::InputSubmit(first) = engine.handle(&submit, &mut events).unwrap() else {
        panic!("input.submit response");
    };
    assert_eq!(first.state, InputAdmissionState::Running);
    assert_eq!(first.route_reason, Some(InputRouteReason::ExplicitCurrent));
    assert_eq!(events.len(), 1);
    assert!(matches!(events[0], EventBody::OrchestrationUpdated(_)));
    assert_eq!(engine.store.snapshot().unwrap().raw.len(), 1);

    let mut replay = submit.clone();
    replay.request_id = RequestId::new("input-current-replay").unwrap();
    let SuccessResult::InputSubmit(replayed) = direct(&mut engine, &replay).unwrap() else {
        panic!("replayed input.submit response");
    };
    assert_eq!(replayed.admission_id, first.admission_id);
    assert_eq!(replayed.input_hash, first.input_hash);
    assert_eq!(engine.store.snapshot().unwrap().raw.len(), 1);
    drop(engine);

    let mut reopened_service = DesktopService::open(config).unwrap();
    let mut reopened = reopened_service.engine.take().unwrap();
    hello(&mut reopened);
    let SuccessResult::InputStatus(status) = direct(
        &mut reopened,
        &request(
            "input-current-status",
            RequestBody::InputStatus(
                session_id(),
                InputStatus {
                    admission_id: first.admission_id,
                },
            ),
        ),
    )
    .unwrap() else {
        panic!("input.status response");
    };
    assert_eq!(status.state, InputAdmissionState::OutcomeUnknown);
    assert!(reopened.active.is_none());
}

#[test]
fn admitted_input_consumes_only_the_matching_destination_draft() {
    let (_root, _config, mut engine) = production_engine();
    hello(&mut engine);
    direct(&mut engine, &draft("draft-first", 0, "first input")).unwrap();
    let before = engine.store.snapshot().unwrap();
    let unavailable = direct(
        &mut engine,
        &request(
            "input-reference-unavailable",
            RequestBody::InputSubmit(
                session_id(),
                InputSubmit::Reference {
                    expected_manager_revision: DecimalU64::new(0),
                    expected_session_revision: before.marker.session_revision,
                    expected_goal_revision: DecimalU64::new(0),
                    route: InputRoute::Current,
                    admission_id: Some(AdmissionId::new("adm-reference-unavailable").unwrap()),
                    input_ref: ResultId::new("result-unavailable").unwrap(),
                },
            ),
        ),
    )
    .unwrap_err();
    assert_eq!(unavailable.code, ErrorCode::ReferenceUnavailable);
    assert_eq!(
        engine.store.snapshot().unwrap().state.draft.text,
        "first input"
    );
    let SuccessResult::InputSubmit(first) = direct(
        &mut engine,
        &submit_input(
            "input-first",
            session_id(),
            0,
            before.marker.session_revision.get(),
            InputRoute::Current,
            "adm-draft-first",
            "first input",
        ),
    )
    .unwrap() else {
        panic!("input.submit response");
    };
    assert!(first.run.is_some());
    let consumed = engine.store.snapshot().unwrap();
    assert_eq!(consumed.state.draft.text, "");
    assert!(consumed.state.draft.attachment_ids.is_empty());
    assert_eq!(consumed.state.draft.draft_revision, DecimalU64::new(2));
    assert_eq!(consumed.state.runs[0].input.text, "first input");

    direct(&mut engine, &draft("draft-next", 2, "next input")).unwrap();
    assert_eq!(
        engine.store.snapshot().unwrap().state.draft.text,
        "next input"
    );
    direct(
        &mut engine,
        &submit_input(
            "input-first-replay",
            session_id(),
            0,
            before.marker.session_revision.get(),
            InputRoute::Current,
            "adm-draft-first",
            "first input",
        ),
    )
    .unwrap();
    let replayed = engine.store.snapshot().unwrap();
    assert_eq!(replayed.state.draft.text, "next input");
    assert_eq!(replayed.raw.len(), 1);

    let (_other_root, _other_config, mut other) = production_engine();
    hello(&mut other);
    direct(&mut other, &draft("draft-edited", 0, "newer edit")).unwrap();
    let revision = other
        .store
        .snapshot()
        .unwrap()
        .marker
        .session_revision
        .get();
    direct(
        &mut other,
        &submit_input(
            "input-other",
            session_id(),
            0,
            revision,
            InputRoute::Current,
            "adm-draft-other",
            "admitted input",
        ),
    )
    .unwrap();
    assert_eq!(
        other.store.snapshot().unwrap().state.draft.text,
        "newer edit"
    );
}

#[test]
fn sidechat_admission_consumes_only_the_unchanged_source_lane_draft() {
    let (_root, _config, mut engine) = production_engine();
    hello(&mut engine);
    direct(&mut engine, &draft("draft-side", 0, "side input")).unwrap();
    let revision = engine
        .store
        .snapshot()
        .unwrap()
        .marker
        .session_revision
        .get();
    let SuccessResult::InputSubmit(sidechat) = direct(
        &mut engine,
        &submit_input(
            "input-side",
            session_id(),
            0,
            revision,
            InputRoute::New,
            "adm-draft-side",
            "side input",
        ),
    )
    .unwrap() else {
        panic!("sidechat input.submit response");
    };
    let sidechat_id = sidechat.destination_session_id.unwrap();
    // The sidechat owns the admitted run, but the source copy is consumed
    // after that append commits.  It cannot become a second submission after
    // the sidechat completes or is cancelled.
    assert_eq!(engine.store.snapshot().unwrap().state.draft.text, "");
    let source_revision = engine
        .store
        .snapshot()
        .unwrap()
        .state
        .draft
        .draft_revision
        .get();
    direct(
        &mut engine,
        &draft("draft-after-sidechat", source_revision, "newer edit"),
    )
    .unwrap();
    let SuccessResult::InputCancel(_) = direct(
        &mut engine,
        &request(
            "cancel-sidechat",
            RequestBody::InputCancel(
                session_id(),
                InputCancel {
                    expected_manager_revision: sidechat.manager_revision,
                    admission_id: sidechat.admission_id.clone(),
                },
            ),
        ),
    )
    .unwrap() else {
        panic!("sidechat input.cancel response");
    };
    // A distinct source draft, including one entered while the sidechat was
    // running, is not overwritten by terminal admission processing.
    assert_eq!(
        engine.store.snapshot().unwrap().state.draft.text,
        "newer edit"
    );
    engine.select_lane(&sidechat_id).unwrap();
    let side = engine.store.snapshot().unwrap();
    assert_eq!(side.state.draft.text, "");
    assert_eq!(side.state.runs[0].input.text, "side input");
}

#[test]
fn explicit_new_uses_an_empty_second_lane_and_both_sessions_can_run() {
    let (_root, _config, mut engine) = production_engine();
    hello(&mut engine);
    let SuccessResult::InputSubmit(current) = direct(
        &mut engine,
        &submit_input(
            "input-current",
            session_id(),
            0,
            0,
            InputRoute::Current,
            "adm-client-current",
            "current work",
        ),
    )
    .unwrap() else {
        panic!("current input.submit response");
    };
    let SuccessResult::InputSubmit(sidechat) = direct(
        &mut engine,
        &submit_input(
            "input-sidechat",
            session_id(),
            current.manager_revision.get(),
            current.session_revision.get(),
            InputRoute::New,
            "adm-client-sidechat",
            "independent work",
        ),
    )
    .unwrap() else {
        panic!("sidechat input.submit response");
    };
    assert_eq!(sidechat.state, InputAdmissionState::Running);
    assert_eq!(sidechat.route_reason, Some(InputRouteReason::ExplicitNew));
    let destination = sidechat.destination_session_id.clone().unwrap();
    assert_ne!(destination, session_id());
    engine.select_lane(&destination).unwrap();
    let saved = engine.store.snapshot().unwrap();
    assert_eq!(saved.raw.len(), 1);
    assert_eq!(saved.state.inherited_turn_count.get(), 0);
    assert!(saved.state.goal.is_none());
    assert!(saved.state.unresolved_approvals.is_empty());

    let SuccessResult::SessionList(sessions) = direct(
        &mut engine,
        &request("session-list", RequestBody::SessionList),
    )
    .unwrap() else {
        panic!("session.list response");
    };
    assert_eq!(sessions.session_total.get(), 2);
    assert_eq!(sessions.sessions.len(), 2);
    assert!(
        sessions
            .sessions
            .iter()
            .all(|session| session.active_run.is_some())
    );
}

#[tokio::test]
async fn transport_selection_invalidates_previous_lane_subscription() {
    let (_root, _config, mut engine) = production_engine();
    hello(&mut engine);
    let SuccessResult::InputSubmit(sidechat) = direct(
        &mut engine,
        &submit_input(
            "input-sidechat",
            session_id(),
            0,
            0,
            InputRoute::New,
            "adm-transport-sidechat",
            "independent work",
        ),
    )
    .unwrap() else {
        panic!("sidechat input.submit response");
    };
    let sidechat_session = sidechat.destination_session_id.unwrap();
    // The setup uses the direct owner API only to create the managed lane.
    // The correlation under test is the pipe transport's resume handling.
    engine.connection = ConnectionState::AwaitingHello;
    engine.client = None;
    let (mut client, task) = connect(engine);
    client
        .call(&request("hello", RequestBody::Hello))
        .await
        .unwrap();
    let SuccessResult::SessionSubscribe(main) = client
        .call(&request(
            "subscribe-main",
            RequestBody::SessionSubscribe(session_id(), Subscribe::default()),
        ))
        .await
        .unwrap()
    else {
        panic!("main subscribe response");
    };
    let SuccessResult::SessionSubscribe(sidechat) = client
        .call(&request(
            "subscribe-sidechat",
            RequestBody::SessionSubscribe(sidechat_session.clone(), Subscribe::default()),
        ))
        .await
        .unwrap()
    else {
        panic!("sidechat subscribe response");
    };

    assert_eq!(
        client
            .call(&request(
                "resume-main",
                RequestBody::SessionSubscribe(
                    session_id(),
                    Subscribe {
                        resume: Some(main.position.clone()),
                    },
                ),
            ))
            .await
            .unwrap_err()
            .code,
        ErrorCode::RevisionConflict
    );
    let SuccessResult::SessionSubscribe(resubscribed) = client
        .call(&request(
            "resubscribe-main",
            RequestBody::SessionSubscribe(session_id(), Subscribe::default()),
        ))
        .await
        .unwrap()
    else {
        panic!("main resubscribe response");
    };
    assert_eq!(resubscribed.session_id, session_id());
    assert_ne!(
        main.position.subscription_id,
        sidechat.position.subscription_id
    );
    assert_ne!(
        main.position.subscription_id,
        resubscribed.position.subscription_id
    );
    assert_ne!(
        sidechat.position.subscription_id,
        resubscribed.position.subscription_id
    );
    drop(client.tx);
    let (_engine, result) = task.await.unwrap();
    result.unwrap();
}

#[tokio::test]
async fn transport_selected_sidechat_never_receives_background_main_events() {
    let (_root, _config, mut engine) = production_engine();
    // Keep durable SessionManager behavior while using the finite local fake
    // runner. Enough chunks remain after selection for both lanes to step.
    engine.production = false;
    engine.options.script = (0..16).map(|index| format!("chunk-{index}")).collect();
    hello(&mut engine);
    let SuccessResult::InputSubmit(main) = direct(
        &mut engine,
        &submit_input(
            "start-running-main",
            session_id(),
            0,
            0,
            InputRoute::Current,
            "adm-running-main",
            "main work",
        ),
    )
    .unwrap() else {
        panic!("main admission response");
    };
    let SuccessResult::InputSubmit(side) = direct(
        &mut engine,
        &submit_input(
            "start-running-side",
            session_id(),
            main.manager_revision.get(),
            main.session_revision.get(),
            InputRoute::New,
            "adm-running-side",
            "side work",
        ),
    )
    .unwrap() else {
        panic!("side admission response");
    };
    let side_session = side.destination_session_id.clone().unwrap();
    let side_run = side.run.clone().unwrap();
    engine.connection = ConnectionState::AwaitingHello;
    engine.client = None;
    engine.wire_version = None;
    let (mut client, task) = connect(engine);
    client
        .call(&request("hello-lane-stream", RequestBody::Hello))
        .await
        .unwrap();
    client
        .call(&request(
            "subscribe-running-main",
            RequestBody::SessionSubscribe(session_id(), Subscribe::default()),
        ))
        .await
        .unwrap();
    let SuccessResult::SessionSelect(side_snapshot) = client
        .call(&request(
            "select-running-side",
            RequestBody::SessionSelect(side_session.clone()),
        ))
        .await
        .unwrap()
    else {
        panic!("side selection response");
    };
    // `session.select` atomically establishes the returned sidechat stream;
    // no second subscribe request is required. `call` drains every event
    // ordered before its response, so observe only later events here.
    client.events.clear();

    timeout(Duration::from_secs(3), async {
        loop {
            let event = client.event().await;
            assert_eq!(event.session_id, side_session);
            assert_eq!(
                event.subscription_id,
                side_snapshot.position.subscription_id
            );
            if matches!(
                event.body,
                EventBody::RunState(ref run)
                    if run.run_id == side_run.run_id && run.state.is_terminal()
            ) {
                break;
            }
        }
    })
    .await
    .unwrap();

    drop(client.tx);
    let (mut engine, result) = task.await.unwrap();
    result.unwrap();
    engine.select_lane(&session_id()).unwrap();
    assert!(
        engine
            .store
            .snapshot()
            .unwrap()
            .state
            .runs
            .iter()
            .any(|run| {
                run.admission
                    .as_ref()
                    .is_some_and(|binding| binding.admission_id == "adm-running-main")
            })
    );
}

#[tokio::test]
async fn transport_cancels_consecutive_input_preserves_attachments_and_selects_sidechat() {
    let (_root, _config, mut engine) = production_engine();
    // Keep the durable production SessionManager while using the local fake
    // runner. This transport test must not contact a provider.
    engine.production = false;
    let (mut client, task) = connect(engine);
    client
        .call(&request("hello", RequestBody::Hello))
        .await
        .unwrap();

    let attachment = AttachmentId::new("attachment-transport-1").unwrap();
    let first_request = request(
        "input-first",
        RequestBody::InputSubmit(
            session_id(),
            InputSubmit::Text {
                expected_manager_revision: DecimalU64::new(0),
                expected_session_revision: DecimalU64::new(0),
                expected_goal_revision: DecimalU64::new(0),
                route: InputRoute::Current,
                attachment_ids: vec![attachment.clone()],
                admission_id: Some(AdmissionId::new("adm-transport-first").unwrap()),
                text: "main work".into(),
            },
        ),
    );
    let SuccessResult::InputSubmit(first) = client.call(&first_request).await.unwrap() else {
        panic!("first input.submit response");
    };
    assert_eq!(first.destination_session_id, Some(session_id()));
    assert!(first.run.is_some());

    let first_status_request = request(
        "input-first-status",
        RequestBody::InputStatus(
            session_id(),
            InputStatus {
                admission_id: first.admission_id.clone(),
            },
        ),
    );
    let first_terminal = timeout(Duration::from_secs(3), async {
        loop {
            let SuccessResult::InputStatus(status) =
                client.call(&first_status_request).await.unwrap()
            else {
                panic!("first input.status response");
            };
            if matches!(
                status.state,
                InputAdmissionState::Completed
                    | InputAdmissionState::Failed
                    | InputAdmissionState::OutcomeUnknown
            ) {
                break status;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();

    let second_request = submit_input(
        "input-second",
        session_id(),
        first_terminal.manager_revision.get(),
        first_terminal.session_revision.get(),
        InputRoute::Current,
        "adm-transport-second",
        "follow-up work",
    );
    let SuccessResult::InputSubmit(second) = client.call(&second_request).await.unwrap() else {
        panic!("second input.submit response");
    };
    let cancel = request(
        "cancel-second",
        RequestBody::InputCancel(
            session_id(),
            InputCancel {
                expected_manager_revision: second.manager_revision,
                admission_id: second.admission_id.clone(),
            },
        ),
    );
    let SuccessResult::InputCancel(cancelled) = client.call(&cancel).await.unwrap() else {
        panic!("input.cancel response");
    };
    assert_eq!(cancelled.state, InputAdmissionState::Cancelled);
    assert_eq!(cancelled.route, InputRoute::Current);
    assert_eq!(
        cancelled.route_reason,
        Some(InputRouteReason::ExplicitCurrent)
    );
    assert_eq!(cancelled.destination_session_id, Some(session_id()));
    assert!(cancelled.run.is_some());

    // The transport settles the cancelled runtime before accepting the next
    // control request. Refresh the writer revision after that local terminal
    // commit instead of reusing the cancellation response's earlier marker.
    let SuccessResult::InputStatus(cancelled) = client
        .call(&request(
            "cancel-second-status",
            RequestBody::InputStatus(
                session_id(),
                InputStatus {
                    admission_id: cancelled.admission_id.clone(),
                },
            ),
        ))
        .await
        .unwrap()
    else {
        panic!("cancelled input.status response");
    };
    assert_eq!(cancelled.state, InputAdmissionState::Cancelled);

    let sidechat_request = submit_input(
        "input-sidechat-select",
        session_id(),
        cancelled.manager_revision.get(),
        cancelled.session_revision.get(),
        InputRoute::New,
        "adm-transport-select",
        "independent work",
    );
    let SuccessResult::InputSubmit(sidechat) = client.call(&sidechat_request).await.unwrap() else {
        panic!("sidechat input.submit response");
    };
    let sidechat_id = sidechat.destination_session_id.clone().unwrap();
    let SuccessResult::SessionSelect(selected) = client
        .call(&request(
            "select-sidechat",
            RequestBody::SessionSelect(sidechat_id.clone()),
        ))
        .await
        .unwrap()
    else {
        panic!("session.select response");
    };
    assert_eq!(selected.session_id, sidechat_id);

    drop(client.tx);
    let (mut engine, result) = task.await.unwrap();
    result.unwrap();
    engine.select_lane(&session_id()).unwrap();
    let saved = engine.store.snapshot().unwrap();
    assert!(saved.state.runs.iter().any(|run| {
        run.admission
            .as_ref()
            .is_some_and(|binding| binding.admission_id == "adm-transport-first")
            && run.input.attachment_ids == vec![attachment.clone()]
    }));
}

#[tokio::test]
async fn transport_input_submit_resolves_lossless_projected_clarification() {
    let (_root, _config, mut engine) = production_engine();
    engine.production = false;
    hello(&mut engine);
    let run = begin(&mut engine);
    let active = engine.active.as_ref().unwrap();
    let operation = active.operation.clone();
    engine.store.record_intent(&run, operation.clone()).unwrap();
    let question = "JSON/TOMLのどちらにしますか？  \"既存値\"も保持します。";
    engine
        .store
        .finish_with_messages(
            &run,
            &operation,
            Observation::Succeeded,
            ResultId::new("clarification-result").unwrap(),
            vec![
                polaris_provider::Message::assistant_with_tool_calls(
                    "",
                    vec![polaris_provider::ToolCall {
                        id: "clarify-call".into(),
                        name: polaris_core::turn_intent::CLARIFY.into(),
                        arguments: serde_json::json!({"questions":[question]}),
                    }],
                ),
                polaris_provider::Message::tool_result("clarify-call", "waiting for user response"),
                polaris_provider::Message::assistant("回答待ち"),
            ],
        )
        .unwrap();
    engine.active = None;

    let pending_snapshot = engine.snapshot(false).unwrap();
    let pending = &pending_snapshot.orchestration.unwrap().clarification;
    assert_eq!(pending.state, ClarificationStateV2::Pending);
    assert_eq!(pending.unanswered_count.get(), 1);
    assert_eq!(pending.questions.len(), 1);
    assert_eq!(pending.questions[0].text, question);
    assert!(pending.questions[0].options.is_empty());
    assert!(pending.questions[0].free_text.allowed);
    assert!(pending.questions[0].free_text.required);
    let question_id = pending.questions[0].question_id.clone();

    let SuccessResult::SessionList(sessions) = direct(
        &mut engine,
        &request("clarify-sessions", RequestBody::SessionList),
    )
    .unwrap() else {
        panic!("session.list response");
    };
    engine.connection = ConnectionState::AwaitingHello;
    engine.client = None;
    engine.wire_version = None;
    let (mut client, task) = connect(engine);
    client
        .call(&request("hello-clarify", RequestBody::Hello))
        .await
        .unwrap();
    let answer = submit_input(
        "clarify-answer",
        session_id(),
        sessions.manager_revision.get(),
        pending_snapshot.session_revision.get(),
        InputRoute::Current,
        "adm-clarify-answer",
        "JSONでお願いします。",
    );
    let SuccessResult::InputSubmit(_) = client.call(&answer).await.unwrap() else {
        panic!("clarification answer input.submit response");
    };
    let SuccessResult::SessionSnapshot(answered) = client
        .call(&request(
            "clarify-snapshot",
            RequestBody::SessionSnapshot(session_id()),
        ))
        .await
        .unwrap()
    else {
        panic!("clarification snapshot response");
    };
    let resolved = &answered.orchestration.unwrap().clarification;
    assert_eq!(resolved.state, ClarificationStateV2::Resolved);
    assert_eq!(resolved.unanswered_count.get(), 0);
    assert_eq!(resolved.questions.len(), 1);
    assert_eq!(resolved.questions[0].question_id, question_id);
    assert_eq!(resolved.questions[0].text, question);
    assert!(resolved.questions[0].options.is_empty());
    assert!(resolved.questions[0].answered);

    drop(client.tx);
    task.await.unwrap().1.unwrap();
}

#[cfg(target_os = "macos")]
fn attach_delegation(engine: &mut Engine, target: &RunTarget, child_suffix: &str, terminal: bool) {
    use polaris_core::{
        session::{
            ChildControlProjectionV1, RootDelegationConfigV1, RootDelegationControlV1,
            RootDelegationProjectionV1,
        },
        subagent_control::{
            ApprovalCapabilityV1, CapabilitySnapshotV1, ChildAttemptId as CoreChildAttemptId,
            ChildAttemptV1, ChildBindingV1, ChildControlPlaneV1, ChildStateV1,
            LogicalChildId as CoreLogicalChildId, LogicalChildV1, McpCapabilityV1, ParentBindingV1,
            RootAttemptId, RootRunId, SandboxCapabilityV1, TaskId, UsageAttributionV1,
        },
    };

    let root_run_id = RootRunId::new(target.run_id.as_str()).unwrap();
    let root_attempt_id = RootAttemptId::new(target.attempt_id.as_str()).unwrap();
    let active_task_id = TaskId::new(format!("task-{child_suffix}")).unwrap();
    let logical_child_id = CoreLogicalChildId::new(format!("child-{child_suffix}")).unwrap();
    let attempt_id = CoreChildAttemptId::new(format!("child-attempt-{child_suffix}")).unwrap();
    let state = if terminal {
        ChildStateV1::Succeeded
    } else {
        ChildStateV1::Running
    };
    let child = LogicalChildV1 {
        logical_child_id: logical_child_id.clone(),
        display_name: "Sirius".into(),
        binding: ChildBindingV1 {
            root_run_id: root_run_id.clone(),
            root_attempt_id: root_attempt_id.clone(),
            active_task_id: active_task_id.clone(),
            parent: ParentBindingV1::Root,
        },
        role: "file-reader".into(),
        tier: "dwarf".into(),
        model: "gpt-6-astra".into(),
        effort: "xhigh".into(),
        capabilities: CapabilitySnapshotV1 {
            sandbox: SandboxCapabilityV1::ReadOnly,
            write_roots: Vec::new(),
            tool_names: vec!["read".into()],
            approval: ApprovalCapabilityV1::Never,
            mcp: McpCapabilityV1::None,
        },
        attempts: vec![ChildAttemptV1 {
            attempt_id: attempt_id.clone(),
            attempt_number: 0,
            restart_of: None,
            retry_capabilities: None,
            state: state.clone(),
            activity: None,
            usage: UsageAttributionV1::Missing,
            terminal_reason: terminal.then(|| "done".into()),
            result_refs: Vec::new(),
            artifact_refs: Vec::new(),
        }],
    };
    let plane = ChildControlPlaneV1 {
        schema_version: 1,
        children: vec![child.clone()],
        cursor: 7,
    };
    let config = RootDelegationConfigV1 {
        session_id: engine.session.as_str().into(),
        root_run_id: root_run_id.clone(),
        root_attempt_id: root_attempt_id.clone(),
        active_task_id: active_task_id.clone(),
        parent_capabilities: child.capabilities.clone(),
        model: "gpt-6-astra".into(),
        effort: "xhigh".into(),
    };
    let hook = Arc::new(|_| Ok(()));
    let control = if terminal {
        RootDelegationControlV1::restore_with_persistence_hook(config, plane, 0, hook)
    } else {
        RootDelegationControlV1::new_live_with_persistence_hook_for_test(config, plane, 0, hook)
    }
    .unwrap();
    let initial = RootDelegationProjectionV1 {
        revision: 0,
        session_id: engine.session.as_str().into(),
        root_run_id,
        root_attempt_id,
        active_task_id,
        children: vec![ChildControlProjectionV1 {
            logical_child_id,
            attempt_id,
            display_name: child.display_name,
            role: child.role,
            tier: child.tier,
            model: child.model,
            effort: child.effort,
            state,
            activity: None,
        }],
        mcp_leases: Vec::new(),
    };
    let projection = Arc::new(std::sync::RwLock::new(initial));
    let projected = projection.clone();
    let (updates, receiver) = std::sync::mpsc::sync_channel(1);
    control.set_projection_hook(Some(Arc::new(move |next| {
        *projected.write().unwrap() = next;
        let _ = updates.try_send(());
    })));
    engine.active.as_mut().unwrap().delegation = Some(DelegationRuntime {
        control,
        projection,
        updates: receiver,
    });
}

#[cfg(target_os = "macos")]
fn attach_terminal_delegation(engine: &mut Engine, target: &RunTarget, child_suffix: &str) {
    attach_delegation(engine, target, child_suffix, true);
}

#[cfg(target_os = "macos")]
fn attach_running_delegation(engine: &mut Engine, target: &RunTarget, child_suffix: &str) {
    attach_delegation(engine, target, child_suffix, false);
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn run_cancel_records_durable_child_plan_before_exact_runtime_effect() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    let target = begin(&mut engine);
    attach_running_delegation(&mut engine, &target, "root-cancel");
    let control = engine
        .active
        .as_ref()
        .unwrap()
        .delegation
        .as_ref()
        .unwrap()
        .control
        .clone();
    let runtime_abort = control
        .install_runtime_attempt_for_test(&polaris_core::subagent_control::ChildAttemptRefV1 {
            logical_child_id: polaris_core::subagent_control::LogicalChildId::new(
                "child-root-cancel",
            )
            .unwrap(),
            attempt_id: polaris_core::subagent_control::ChildAttemptId::new(
                "child-attempt-root-cancel",
            )
            .unwrap(),
        })
        .await
        .unwrap();

    let SuccessResult::RunCancel(cancelled) = direct(
        &mut engine,
        &request(
            "root-cancel-with-child",
            RequestBody::RunCancel(session_id(), target),
        ),
    )
    .unwrap() else {
        panic!("run.cancel response");
    };
    assert_eq!(cancelled.status, CancelStatus::CancelRequested);
    let projection = engine.delegation_projection().unwrap();
    assert_eq!(projection.revision, 1);
    assert_eq!(projection.children.len(), 1);
    assert_eq!(
        projection.children[0].state,
        polaris_core::subagent_control::ChildStateV1::Cancelling
    );
    assert!(runtime_abort.is_aborted());
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn transport_subagent_cancel_persists_before_exact_runtime_effect() {
    let (_root, mut engine) = service();
    hello(&mut engine);
    let target = begin(&mut engine);
    attach_running_delegation(&mut engine, &target, "running");
    let control = engine
        .active
        .as_ref()
        .unwrap()
        .delegation
        .as_ref()
        .unwrap()
        .control
        .clone();
    let runtime_reference = polaris_core::subagent_control::ChildAttemptRefV1 {
        logical_child_id: polaris_core::subagent_control::LogicalChildId::new("child-running")
            .unwrap(),
        attempt_id: polaris_core::subagent_control::ChildAttemptId::new("child-attempt-running")
            .unwrap(),
    };
    let runtime_abort = control
        .install_runtime_attempt_for_test(&runtime_reference)
        .await
        .unwrap();
    engine.connection = ConnectionState::AwaitingHello;
    engine.client = None;
    engine.wire_version = None;

    let (mut client, task) = connect(engine);
    client
        .call(&request("subagent-running-hello", RequestBody::Hello))
        .await
        .unwrap();
    let cancel_request = |request_id: &str, revision: u64| {
        request(
            request_id,
            RequestBody::SubagentCancel(
                session_id(),
                SubagentCancel {
                    root_run_id: target.run_id.clone(),
                    root_attempt_id: target.attempt_id.clone(),
                    logical_child_id: LogicalChildId::new("child-running").unwrap(),
                    child_attempt_id: ChildAttemptId::new("child-attempt-running").unwrap(),
                    expected_revision: DecimalU64::new(revision),
                },
            ),
        )
    };
    let SuccessResult::SubagentCancel(first) = client
        .call(&cancel_request("subagent-running-cancel", 0))
        .await
        .unwrap()
    else {
        panic!("running subagent.cancel response");
    };
    assert_eq!(first.disposition, SubagentCancelDispositionV2::Requested);
    assert_eq!(first.revision, DecimalU64::new(1));
    assert_eq!(
        first.state,
        polaris_desktop_protocol::orchestration::ChildControlStateV2::Cancelling
    );
    assert_eq!(first.runtime_effect, SubagentRuntimeEffectV2::Requested);
    assert!(runtime_abort.is_aborted());

    let SuccessResult::SubagentWait(saved) = client
        .call(&request(
            "subagent-running-wait",
            RequestBody::SubagentWait(
                session_id(),
                SubagentWait {
                    root_run_id: target.run_id.clone(),
                    root_attempt_id: target.attempt_id.clone(),
                    expected_revision: DecimalU64::new(1),
                    after_cursor: None,
                },
            ),
        ))
        .await
        .unwrap()
    else {
        panic!("saved subagent.wait response");
    };
    assert_eq!(saved.children[0].control_revision, DecimalU64::new(1));
    assert_eq!(
        saved.children[0].state,
        polaris_desktop_protocol::orchestration::ChildControlStateV2::Cancelling
    );

    let SuccessResult::SubagentCancel(replayed) = client
        .call(&cancel_request("subagent-running-cancel-replay", 1))
        .await
        .unwrap()
    else {
        panic!("replayed subagent.cancel response");
    };
    assert_eq!(
        replayed.disposition,
        SubagentCancelDispositionV2::AlreadyRequested
    );
    assert_eq!(replayed.revision, DecimalU64::new(1));
    assert_eq!(
        replayed.runtime_effect,
        SubagentRuntimeEffectV2::Unavailable
    );

    drop(client.tx);
    let (_engine, result) = task.await.unwrap();
    result.unwrap();
}

#[cfg(target_os = "macos")]
#[tokio::test]
async fn transport_subagent_control_is_cursor_bound_idempotent_and_lane_exact() {
    let (root, mut engine) = service();
    hello(&mut engine);
    let main_target = begin(&mut engine);
    attach_terminal_delegation(&mut engine, &main_target, "main");

    let (_side_root, mut side_engine) = service();
    hello(&mut side_engine);
    let side_target = begin(&mut side_engine);
    let side_session = SessionId::new("side-session").unwrap();
    side_engine.session = side_session.clone();
    attach_terminal_delegation(&mut side_engine, &side_target, "side");
    let mut side_store = root
        .create(
            project_id(),
            side_session.clone(),
            InitialState {
                draft: Draft {
                    draft_revision: DecimalU64::new(0),
                    text: "side root".into(),
                    attachment_ids: Vec::new(),
                },
                configuration: Configuration {
                    history_mode: Default::default(),
                    configuration_revision: DecimalU64::new(0),
                    provider: "fake".into(),
                    model: "scripted".into(),
                    effort: "none".into(),
                },
                policy_revision: DecimalU64::new(0),
            },
        )
        .unwrap();
    side_store
        .apply_bound(
            &Request {
                protocol_version: ProtocolVersion,
                client_id: ClientId::new("side-fixture").unwrap(),
                request_id: RequestId::new("side-run-start").unwrap(),
                body: RequestBody::RunStart(
                    side_session.clone(),
                    RunStart {
                        mcp_prompt: None,
                        expected_draft_revision: DecimalU64::new(0),
                        expected_configuration_revision: DecimalU64::new(0),
                        expected_policy_revision: DecimalU64::new(0),
                    },
                ),
            },
            Some(side_target.clone()),
            None,
        )
        .unwrap();
    engine.other_lane = Some(SessionLane {
        session: side_session.clone(),
        store: side_store,
        subscription: None,
        active: side_engine.active.take(),
        history: None,
        details: None,
        cleanups: Default::default(),
    });
    drop(root);
    engine.connection = ConnectionState::AwaitingHello;
    engine.client = None;
    engine.wire_version = None;

    let (mut client, task) = connect(engine);
    client
        .call(&request("subagent-hello", RequestBody::Hello))
        .await
        .unwrap();
    let SuccessResult::SubagentWait(updated) = client
        .call(&request(
            "subagent-wait-updated",
            RequestBody::SubagentWait(
                session_id(),
                SubagentWait {
                    root_run_id: main_target.run_id.clone(),
                    root_attempt_id: main_target.attempt_id.clone(),
                    expected_revision: DecimalU64::new(0),
                    after_cursor: None,
                },
            ),
        ))
        .await
        .unwrap()
    else {
        panic!("subagent.wait updated response");
    };
    assert_eq!(updated.state, SubagentWaitStateV2::Updated);
    assert_eq!(updated.cursor, DecimalU64::new(7));
    assert_eq!(updated.children.len(), 1);
    assert_eq!(updated.children[0].root_run_id, main_target.run_id);
    assert_eq!(updated.children[0].root_attempt_id, main_target.attempt_id);
    assert_eq!(updated.children[0].control_revision, DecimalU64::new(0));

    let SuccessResult::SubagentWait(unchanged) = client
        .call(&request(
            "subagent-wait-unchanged",
            RequestBody::SubagentWait(
                session_id(),
                SubagentWait {
                    root_run_id: main_target.run_id.clone(),
                    root_attempt_id: main_target.attempt_id.clone(),
                    expected_revision: DecimalU64::new(0),
                    after_cursor: Some(updated.cursor),
                },
            ),
        ))
        .await
        .unwrap()
    else {
        panic!("subagent.wait unchanged response");
    };
    assert_eq!(unchanged.state, SubagentWaitStateV2::Unchanged);
    assert!(unchanged.children.is_empty());

    let wrong_lane = client
        .call(&request(
            "subagent-wrong-lane",
            RequestBody::SubagentWait(
                side_session.clone(),
                SubagentWait {
                    root_run_id: main_target.run_id.clone(),
                    root_attempt_id: main_target.attempt_id.clone(),
                    expected_revision: DecimalU64::new(0),
                    after_cursor: None,
                },
            ),
        ))
        .await
        .unwrap_err();
    assert_eq!(wrong_lane.code, ErrorCode::PermissionDenied);

    let stale = client
        .call(&request(
            "subagent-stale",
            RequestBody::SubagentCancel(
                session_id(),
                SubagentCancel {
                    root_run_id: main_target.run_id.clone(),
                    root_attempt_id: main_target.attempt_id.clone(),
                    logical_child_id: LogicalChildId::new("child-main").unwrap(),
                    child_attempt_id: ChildAttemptId::new("child-attempt-main").unwrap(),
                    expected_revision: DecimalU64::new(1),
                },
            ),
        ))
        .await
        .unwrap_err();
    assert_eq!(stale.code, ErrorCode::RevisionConflict);

    let foreign = client
        .call(&request(
            "subagent-foreign",
            RequestBody::SubagentCancel(
                session_id(),
                SubagentCancel {
                    root_run_id: main_target.run_id.clone(),
                    root_attempt_id: main_target.attempt_id.clone(),
                    logical_child_id: LogicalChildId::new("child-main").unwrap(),
                    child_attempt_id: ChildAttemptId::new("child-attempt-foreign").unwrap(),
                    expected_revision: DecimalU64::new(0),
                },
            ),
        ))
        .await
        .unwrap_err();
    assert_eq!(foreign.code, ErrorCode::NotFound);

    for request_id in ["subagent-terminal", "subagent-terminal-replay"] {
        let SuccessResult::SubagentCancel(cancelled) = client
            .call(&request(
                request_id,
                RequestBody::SubagentCancel(
                    session_id(),
                    SubagentCancel {
                        root_run_id: main_target.run_id.clone(),
                        root_attempt_id: main_target.attempt_id.clone(),
                        logical_child_id: LogicalChildId::new("child-main").unwrap(),
                        child_attempt_id: ChildAttemptId::new("child-attempt-main").unwrap(),
                        expected_revision: DecimalU64::new(0),
                    },
                ),
            ))
            .await
            .unwrap()
        else {
            panic!("subagent.cancel terminal response");
        };
        assert_eq!(
            cancelled.disposition,
            SubagentCancelDispositionV2::AlreadyTerminal
        );
        assert_eq!(
            cancelled.state,
            polaris_desktop_protocol::orchestration::ChildControlStateV2::Succeeded
        );
        assert_eq!(
            cancelled.runtime_effect,
            SubagentRuntimeEffectV2::Unavailable
        );
    }

    let SuccessResult::SubagentWait(side) = client
        .call(&request(
            "subagent-side-exact",
            RequestBody::SubagentWait(
                side_session,
                SubagentWait {
                    root_run_id: side_target.run_id,
                    root_attempt_id: side_target.attempt_id,
                    expected_revision: DecimalU64::new(0),
                    after_cursor: None,
                },
            ),
        ))
        .await
        .unwrap()
    else {
        panic!("side lane subagent.wait response");
    };
    assert_eq!(side.children[0].logical_child_id, "child-side");

    let SuccessResult::SubagentWait(main_after_side_wait) = client
        .call(&request(
            "subagent-main-after-side-wait",
            RequestBody::SubagentWait(
                session_id(),
                SubagentWait {
                    root_run_id: main_target.run_id.clone(),
                    root_attempt_id: main_target.attempt_id.clone(),
                    expected_revision: DecimalU64::new(0),
                    after_cursor: None,
                },
            ),
        ))
        .await
        .unwrap()
    else {
        panic!("main lane wait after side wait");
    };
    assert_eq!(
        main_after_side_wait.children[0].logical_child_id,
        "child-main"
    );

    let SuccessResult::RunCancel(_) = client
        .call(&request(
            "subagent-side-root-cancel",
            RequestBody::RunCancel(
                SessionId::new("side-session").unwrap(),
                RunTarget {
                    run_id: side.root_run_id,
                    attempt_id: side.root_attempt_id,
                },
            ),
        ))
        .await
        .unwrap()
    else {
        panic!("side lane run.cancel response");
    };

    drop(client.tx);
    let (_engine, result) = task.await.unwrap();
    result.unwrap();
}

#[test]
fn orchestration_snapshot_projects_safe_child_activity_and_typed_unavailability() {
    use polaris_core::{
        session::{ChildControlProjectionV1, RootDelegationProjectionV1},
        subagent_control::{
            ChildAttemptId, ChildStateV1, LogicalChildId, RootAttemptId, RootRunId, SafeActivityV1,
            TaskId,
        },
    };

    let (_root, engine) = service();
    let saved = engine.store.snapshot().unwrap();
    let projection = RootDelegationProjectionV1 {
        revision: 4,
        session_id: session_id().as_str().into(),
        root_run_id: RootRunId::new("root-run").unwrap(),
        root_attempt_id: RootAttemptId::new("root-attempt").unwrap(),
        active_task_id: TaskId::new("active-task").unwrap(),
        children: vec![ChildControlProjectionV1 {
            logical_child_id: LogicalChildId::new("child-1").unwrap(),
            attempt_id: ChildAttemptId::new("child-attempt-1").unwrap(),
            display_name: "Sirius".into(),
            role: "file-reader".into(),
            tier: "dwarf".into(),
            model: "gpt-5.6-luna".into(),
            effort: "low".into(),
            state: ChildStateV1::Running,
            activity: Some(SafeActivityV1 {
                summary: "read(CURRENT.md)".into(),
                sequence: 2,
            }),
        }],
        mcp_leases: Vec::new(),
    };
    let summary = orchestration_summary(&saved, None, Some(&projection));
    assert_eq!(summary.subagent_total.get(), 1);
    assert_eq!(summary.subagents[0].root_run_id.as_str(), "root-run");
    assert_eq!(
        summary.subagents[0].root_attempt_id.as_str(),
        "root-attempt"
    );
    assert_eq!(summary.subagents[0].control_revision, DecimalU64::new(4));
    assert_eq!(
        summary.subagents[0].activity.as_deref(),
        Some("read(CURRENT.md)")
    );
    assert_eq!(
        summary.common_runner.state,
        CommonRunnerStateV2::Unavailable
    );
    assert_eq!(summary.clarification.state, ClarificationStateV2::None);
}
