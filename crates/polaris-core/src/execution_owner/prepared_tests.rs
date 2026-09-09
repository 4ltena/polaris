//! Prepared workspace policy and lifetime checks through deferred process cleanup.
use super::*;
use crate::isolated_workspace::Limits;
use std::{path::PathBuf, time::Duration};

fn prepared_owner() -> (
    crate::desktop_store::PrototypeRoot,
    Writer,
    RunTarget,
    RunExecution,
    tempfile::TempDir,
) {
    let (root, writer, target, old, dir) = tests::read_owner();
    drop(old);
    let base = dir.path().canonicalize().unwrap();
    let source = base.join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("ordinary"), "synthetic source").unwrap();
    let workspace = Arc::new(
        PreparedWorkspace::prepare(
            &source,
            &base.join("runtime/helper"),
            SandboxMode::WorkspaceWrite,
            &[],
            Limits {
                max_entries: 16,
                max_files: 8,
                max_file_bytes: 1024,
                max_total_bytes: 4096,
                max_depth: 4,
            },
        )
        .unwrap(),
    );
    let owner =
        RunExecution::with_prepared_workspace(&target, Arc::new(AtomicBool::new(false)), workspace)
            .unwrap();
    (root, writer, target, owner, dir)
}

fn command(policy: SandboxPolicy) -> ExecutionCommand {
    ExecutionCommand {
        policy,
        program: PathBuf::from("/must-never-spawn"),
        args: vec![],
        stdin: None,
    }
}

fn register_slot(owner: &mut RunExecution, target: &RunTarget) -> Arc<Mutex<Slot>> {
    let slot = Arc::new(Mutex::new(Slot {
        workspace: owner.workspace.clone(),
        _reservation: Reservation::acquire().unwrap(),
        target: target.clone(),
        operation: OperationId::new("synthetic-operation").unwrap(),
        cancelled: Arc::new(AtomicBool::new(false)),
        worker: None,
        completion: Arc::new(Mutex::new(None)),
        result: None,
        pending: None,
        recorded: false,
        orphaned: false,
    }));
    RECOVERY.lock().unwrap().slots.push(slot.clone());
    owner.slots.push(slot.clone());
    slot
}

fn lifecycle() {
    // No native executable is run. A synthetic worker waits until the test
    // confirms that dropping/cancelling its owner retained the private copy.
    for saved in [false, true] {
        let (_root, _writer, target, mut owner, _dir) = prepared_owner();
        let weak = Arc::downgrade(owner.workspace.as_ref().unwrap());
        let path = owner
            .workspace
            .as_ref()
            .unwrap()
            .snapshot()
            .path()
            .to_owned();
        let slot = register_slot(&mut owner, &target);
        let (release, wait) = std::sync::mpsc::channel();
        {
            let mut slot = slot.lock().unwrap();
            let workspace = slot.workspace.clone();
            let completion = slot.completion.clone();
            slot.worker = Some(std::thread::spawn(move || {
                wait.recv_timeout(Duration::from_secs(5)).unwrap();
                assert!(workspace.as_ref().unwrap().snapshot().path().exists());
                *completion.lock().unwrap() = Some((
                    Arc::new(ExecutionResult::not_started("synthetic completion".into())),
                    None,
                ));
                drop(workspace);
            }));
        }
        owner.saved = saved;
        owner.cancel();
        assert!(slot.lock().unwrap().cancelled.load(Ordering::Acquire));
        assert!(path.exists());
        drop(owner);
        assert!(weak.upgrade().is_some());
        assert!(path.exists());
        assert!(
            RECOVERY
                .lock()
                .unwrap()
                .slots
                .iter()
                .any(|s| Arc::ptr_eq(s, &slot))
        );
        release.send(()).unwrap();
        for _ in 0..500 {
            CleanupOwner::default().step();
            if weak.upgrade().is_none() {
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
        assert!(weak.upgrade().is_none());
        assert!(!path.exists());
    }
    for saved in [false, true] {
        let (_root, _writer, target, mut owner, _dir) = prepared_owner();
        let weak = Arc::downgrade(owner.workspace.as_ref().unwrap());
        let path = owner
            .workspace
            .as_ref()
            .unwrap()
            .snapshot()
            .path()
            .to_owned();
        let slot = register_slot(&mut owner, &target);
        let ready = Arc::new(AtomicBool::new(false));
        {
            let mut slot = slot.lock().unwrap();
            slot.result = Some(Arc::new(ExecutionResult::failed(
                "synthetic unconfirmed cleanup".into(),
            )));
            slot.pending = Some(Cleanup::Deferred(ready.clone()));
        }
        owner.saved = saved;
        assert!(!owner.quiescent());
        drop(owner);
        CleanupOwner::default().step();
        assert!(weak.upgrade().is_some());
        assert!(path.exists());
        ready.store(true, Ordering::Release);
        for _ in 0..100 {
            CleanupOwner::default().step();
            if weak.upgrade().is_none() {
                break;
            }
        }
        assert!(weak.upgrade().is_none());
        assert!(!path.exists());
    }
    // A worker panic/lost result cannot be attributed to a native handle by PID.
    // Its lease survives until all registered workers and the registry settle.
    let (_root, _writer, target, mut owner, _dir) = prepared_owner();
    let weak = Arc::downgrade(owner.workspace.as_ref().unwrap());
    let slot = register_slot(&mut owner, &target);
    {
        let mut slot = slot.lock().unwrap();
        slot.worker = Some(std::thread::spawn(|| {})); // deliberately loses completion
    }
    let (_other_root, _other_writer, other_target, mut other, _other_dir) = prepared_owner();
    let blocker = register_slot(&mut other, &other_target);
    drop(owner);
    for _ in 0..100 {
        CleanupOwner::default().step();
        if matches!(slot.lock().unwrap().pending, Some(Cleanup::Registry)) {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(matches!(
        slot.lock().unwrap().pending,
        Some(Cleanup::Registry)
    ));
    assert!(weak.upgrade().is_some());
    blocker.lock().unwrap().result = Some(Arc::new(ExecutionResult::not_started(
        "synthetic blocker finished".into(),
    )));
    for _ in 0..100 {
        CleanupOwner::default().step();
        if weak.upgrade().is_none() {
            break;
        }
    }
    assert!(weak.upgrade().is_none());
    other.saved = true;
}

fn registry_drain_after_concurrent_join_retains_lease() {
    let (_root, _writer, target, mut owner, _dir) = prepared_owner();
    let weak = Arc::downgrade(owner.workspace.as_ref().unwrap());
    let path = owner
        .workspace
        .as_ref()
        .unwrap()
        .snapshot()
        .path()
        .to_owned();
    let slot = register_slot(&mut owner, &target);
    // Use an isolated Recovery so the background reclaimer cannot consume the
    // synthetic registry. Move the registered slot before its worker starts.
    RECOVERY
        .lock()
        .unwrap()
        .slots
        .retain(|s| !Arc::ptr_eq(s, &slot));
    let mut recovery = Recovery {
        slots: vec![slot.clone()],
        pending: vec![],
        cursor: 0,
        pending_cursor: 0,
    };
    let registry = Arc::new(Mutex::new(Vec::<Cleanup>::new()));
    let confirmed = Arc::new(AtomicBool::new(false));
    let (release, wait) = std::sync::mpsc::channel();
    {
        let mut owned = slot.lock().unwrap();
        let registry = registry.clone();
        let confirmed = confirmed.clone();
        owned.worker = Some(std::thread::spawn(move || {
            wait.recv_timeout(Duration::from_secs(5)).unwrap();
            // Model a native handle parked by unwinding, with no completion.
            registry.lock().unwrap().push(Cleanup::Deferred(confirmed));
        }));
    }
    drop(owner);
    assert!(weak.upgrade().is_some());
    let mut drains = 0;
    recovery.step_with_registry(|| {
        drains += 1;
        let drained = std::mem::take(&mut *registry.lock().unwrap());
        if drains == 1 {
            assert!(drained.is_empty());
            release.send(()).unwrap();
            let deadline = std::time::Instant::now() + Duration::from_secs(5);
            loop {
                let finished = slot.lock().unwrap().worker.as_ref().unwrap().is_finished();
                if finished {
                    break;
                }
                assert!(std::time::Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(1));
            }
            // This is service-side poll, which needs only the slot lock and
            // may join after Recovery's first drain snapshot was taken.
            let mut owned = slot.lock().unwrap();
            owned.poll();
            assert!(owned.worker.is_none());
            assert!(matches!(owned.pending, Some(Cleanup::Registry)));
        }
        drained
    });
    // Without the post-join drain the old barrier clears this lease here.
    assert!(weak.upgrade().is_some());
    assert!(path.exists());
    assert_eq!(drains, 2);
    assert_eq!(recovery.pending.len(), 1);
    assert!(registry.lock().unwrap().is_empty());
    assert!(matches!(
        slot.lock().unwrap().pending,
        Some(Cleanup::Registry)
    ));

    recovery.step_with_registry(|| std::mem::take(&mut *registry.lock().unwrap()));
    assert!(weak.upgrade().is_some());
    assert!(path.exists());
    assert_eq!(recovery.pending.len(), 1);

    confirmed.store(true, Ordering::Release);
    recovery.step_with_registry(|| std::mem::take(&mut *registry.lock().unwrap()));
    assert!(recovery.pending.is_empty());
    assert!(weak.upgrade().is_none());
    assert!(!path.exists());
    let owned = slot.lock().unwrap();
    assert!(owned.pending.is_none());
    assert_eq!(
        owned.result.as_ref().unwrap().end,
        ControlledEnd::StopUnconfirmed
    );
    assert!(!owned.recorded);
    assert!(owned.orphaned);
    drop(owned);
    // Resource cleanup does not manufacture a saved result/shutdown readiness.
    assert!(!recovery.is_empty());
}

async fn policies_and_read_grant() {
    let (_root, mut writer, target, mut owner, dir) = prepared_owner();
    let workspace = owner.workspace.as_ref().unwrap().clone();
    let policy = workspace.policy();
    let boundary = policy.isolated_boundary().unwrap();
    let env = boundary.environment.as_ref().unwrap();
    let readonly = policy.restrict(SandboxMode::ReadOnly, &[]).unwrap();
    assert!(
        owner
            .port
            .read_helper_matches(workspace.helper_path(), &readonly)
    );
    let child = workspace.snapshot().path().join("child");
    std::fs::create_dir(&child).unwrap();
    let narrowed = policy
        .restrict(SandboxMode::WorkspaceWrite, &[child])
        .unwrap();
    assert!(owner.policy_matches(&narrowed));
    assert!(owner.policy_matches(&readonly));

    let base = dir.path().canonicalize().unwrap();
    let isolated = |mode, root: &std::path::Path, reads: &[PathBuf]| {
        SandboxPolicy::isolated(mode, root, reads).unwrap()
    };
    let mut extra_reads = boundary.readable_roots.clone();
    extra_reads.push(base.join("source"));
    let mismatched = [
        SandboxPolicy::new(SandboxMode::ReadOnly, &[]).unwrap(),
        SandboxPolicy::new(SandboxMode::FullAccess, &[]).unwrap(),
        isolated(SandboxMode::ReadOnly, &base.join("source"), &[]),
        isolated(SandboxMode::ReadOnly, &boundary.workspace, &extra_reads)
            .with_isolated_sibling_environment(&env.home, &env.tmpdir)
            .unwrap(),
        isolated(
            SandboxMode::ReadOnly,
            &boundary.workspace,
            &boundary.readable_roots,
        )
        .with_isolated_sibling_environment(&env.tmpdir, &env.home)
        .unwrap(),
        isolated(
            SandboxMode::FullAccess,
            &boundary.workspace,
            &boundary.readable_roots,
        )
        .with_isolated_sibling_environment(&env.home, &env.tmpdir)
        .unwrap(),
        isolated(SandboxMode::WorkspaceWrite, &base.join("runtime"), &[]),
    ];
    for bad in mismatched {
        let waiting = owner.port.submit(command(bad)).unwrap();
        owner.step(&mut writer, &target).unwrap();
        let result = waiting.await.unwrap();
        assert_eq!(result.end, ControlledEnd::CancelledBeforeSpawn);
        assert_eq!(
            result.problem.as_deref(),
            Some("execution policy escapes prepared workspace")
        );
        assert!(owner.slots.is_empty());
        assert!(owner.awaiting.is_empty());
        assert!(owner.approval_events.is_empty());
        assert!(
            writer.snapshot().unwrap().state.runs[0]
                .operations
                .is_empty()
        );
    }
    let cancelled_read = owner
        .port
        .submit_confined_read(&ConfinedReadRequest::DiffBefore {
            path: "ordinary".into(),
        })
        .unwrap();
    drop(cancelled_read);
    owner.step(&mut writer, &target).unwrap();
    assert!(owner.slots.is_empty());
    assert!(
        writer.snapshot().unwrap().state.runs[0]
            .operations
            .is_empty()
    );

    // The same owner admits a narrowed ordinary command to approval, then
    // rejects a changed boundary before consuming that approval's intent.
    let waiting = owner.port.submit(command(narrowed)).unwrap();
    owner.step(&mut writer, &target).unwrap();
    assert_eq!(owner.awaiting.len(), 1);
    owner.awaiting[0].decision = Some(ApprovalDecision::Allow);
    owner.awaiting[0].request.command.policy =
        SandboxPolicy::new(SandboxMode::FullAccess, &[]).unwrap();
    owner.step(&mut writer, &target).unwrap();
    assert_eq!(
        waiting.await.unwrap().end,
        ControlledEnd::CancelledBeforeSpawn
    );
    assert!(owner.slots.is_empty());
    assert!(
        writer.snapshot().unwrap().state.runs[0]
            .operations
            .is_empty()
    );

    // Read grant is real; native execution is cancelled after durable intent.
    owner.invalidate_after_intent = Some(false);
    let waiting = owner
        .port
        .submit_confined_read(&ConfinedReadRequest::DiffBefore {
            path: "ordinary".into(),
        })
        .unwrap();
    owner.step(&mut writer, &target).unwrap();
    assert_eq!(
        waiting.await.unwrap().end,
        ControlledEnd::CancelledBeforeSpawn
    );
    assert_eq!(writer.snapshot().unwrap().state.runs[0].operations.len(), 1);
    assert!(owner.slots[0].lock().unwrap().worker.is_none());
    owner.saved = true;
}

#[tokio::test]
async fn prepared_integration_synthetic() {
    const MARKER: &str = "POLARIS_PREPARED_EXECUTION_FIXTURE";
    if std::env::var_os(MARKER).is_none() {
        // prepare registers default auth *metadata*. Isolate HOME and the
        // registry in a fresh process so not even real auth metadata is used.
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "execution_owner::prepared_tests::prepared_integration_synthetic",
                "--nocapture",
            ])
            .env_clear()
            .env("HOME", home.path().canonicalize().unwrap())
            .env(MARKER, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    registry_drain_after_concurrent_join_retains_lease();
    lifecycle();
    policies_and_read_grant().await;
}
