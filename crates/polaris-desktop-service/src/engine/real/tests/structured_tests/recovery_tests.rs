//! Separate-process continuation of real engine auxiliary failure states.
use super::*;
use std::path::Path;

fn memory_json(saved: &Published) -> serde_json::Value {
    serde_json::to_value(saved.state.conversation_memory.as_ref().unwrap()).unwrap()
}

fn assert_recovered(before: &Published, after: &Published, unknown: bool) {
    assert_eq!(
        serde_json::to_vec(&after.raw[..before.raw.len()]).unwrap(),
        serde_json::to_vec(&before.raw).unwrap(),
        "recovery must preserve the original raw prefix"
    );
    assert_eq!(
        after.raw.iter().filter(|event| event.starts_turn).count(),
        12
    );
    let old = memory_json(before);
    let new = memory_json(after);
    assert_eq!(new["attempts"][0], old["attempts"][0]);
    assert_eq!(
        new["attempts"].as_array().unwrap().len(),
        if unknown { 1 } else { 2 }
    );
    assert_eq!(
        new["sources"].as_array().unwrap().len(),
        if unknown { 0 } else { 2 }
    );
    let old_run = before.state.runs.last().unwrap();
    let restored = after
        .state
        .runs
        .iter()
        .find(|run| run.run.run_id == old_run.run.run_id)
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&restored.operations).unwrap(),
        serde_json::to_vec(&old_run.operations).unwrap()
    );
    let status = after.state.runs.last().unwrap().memory.as_ref().unwrap();
    if unknown {
        assert_eq!(restored.run.state, RunState::OutcomeUnknown);
        assert_ne!(status.phase, MemoryPhase::Ready);
        assert!(
            status
                .embedding_usage
                .as_ref()
                .is_none_or(|usage| usage.requests.get() == 0)
        );
    } else {
        assert_eq!(status.phase, MemoryPhase::Ready);
        assert!(status.embedding_usage.as_ref().unwrap().requests.get() >= 2);
    }
}

pub(super) fn resume_failure_in_child(
    config: &ServiceConfig,
    database: &Path,
    before: &Published,
    unknown: bool,
) {
    let old = memory_json(before);
    assert_eq!(old["attempts"].as_array().unwrap().len(), 1);
    assert!(old["sources"].as_array().unwrap().is_empty());
    let attempt = &old["attempts"][0];
    assert!(attempt["structured"]["intent"].is_object());
    assert_eq!(attempt["structured"]["response"].is_null(), unknown);
    let operation = before
        .state
        .runs
        .last()
        .unwrap()
        .operations
        .iter()
        .find(|operation| Some(operation.operation_id.as_str()) == attempt["operation_id"].as_str())
        .unwrap();
    assert_eq!(operation.result_id.is_none(), unknown);

    let metadata_dir = tempfile::tempdir().unwrap();
    let metadata = metadata_dir.path().join("recovery.json");
    std::fs::write(
        &metadata,
        serde_json::to_vec(&serde_json::json!({
            "store_root":config.store_root,"database":database,
            "parent_pid":std::process::id(),"unknown":unknown
        }))
        .unwrap(),
    )
    .unwrap();
    let output = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap().block_on(async {
        tokio::time::timeout(Duration::from_secs(120),
            tokio::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact", "engine::real::tests::structured_tests::recovery_tests::structured_failure_recovery_child", "--ignored", "--nocapture"])
                .env("POLARIS_STRUCTURED_RECOVERY_FIXTURE", &metadata)
                .kill_on_drop(true).output()
        ).await.expect("owned synthetic recovery child timeout").unwrap()
    });
    assert!(
        output.status.success(),
        "child stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("structured_failure_recovery "));
    // Forward the fixture receipt without nesting its test-harness totals.
    println!(
        "{}",
        stdout
            .lines()
            .find(|line| line.starts_with("structured_failure_recovery "))
            .unwrap()
    );
    let reopened = DesktopService::open(config.clone()).unwrap();
    let after = reopened.engine.as_ref().unwrap().store.snapshot().unwrap();
    assert_recovered(before, &after, unknown);
}

#[test]
#[ignore = "親テストが補助処理失敗後の一時保存先を指定するfixture"]
fn structured_failure_recovery_child() {
    let path =
        std::env::var_os("POLARIS_STRUCTURED_RECOVERY_FIXTURE").expect("parent-owned fixture path");
    let input: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let parent_pid = input["parent_pid"].as_u64().unwrap();
    let unknown = input["unknown"].as_bool().unwrap();
    assert_ne!(u64::from(std::process::id()), parent_pid);
    let database = std::path::PathBuf::from(input["database"].as_str().unwrap());
    let store_root = std::path::PathBuf::from(input["store_root"].as_str().unwrap());
    assert!(
        database.is_absolute()
            && database.is_file()
            && store_root.is_absolute()
            && store_root.is_dir()
    );
    // Both providers are healthy. A replay can succeed, so identity/count checks
    // below expose resending instead of hiding it behind another failure.
    let (mut f, main, summaries) =
        structured_fixture_with_index(false, EmbedderBehavior::Valid, Some(database), None);
    let engine = f.service.engine.as_ref().unwrap();
    let configuration = engine.store.snapshot().unwrap().state.configuration;
    let factory = engine.trusted_runs.as_ref().unwrap().factory.clone();
    drop(f.service);
    f.config.store_root = store_root;
    f.service =
        DesktopService::open_with_trusted_runs(f.config.clone(), configuration, factory).unwrap();
    let engine = f.service.engine.as_mut().unwrap();
    let before = engine.store.snapshot().unwrap();
    assert_eq!(
        before.raw.iter().filter(|event| event.starts_turn).count(),
        11
    );
    engine
        .handle(
            &request("child-recovery-hello", RequestBody::Hello),
            &mut vec![],
        )
        .unwrap();
    run_text(engine, 12);
    let main_count = main.lock().unwrap().len();
    let summaries = summaries.lock().unwrap();
    assert_eq!(main_count, usize::from(!unknown));
    assert_eq!(summaries.len(), usize::from(!unknown));
    if let Some(request) = summaries.first() {
        let actual: serde_json::Value =
            serde_json::from_str(&request.messages.last().unwrap().content).unwrap();
        let targets = actual["summary_target_source_ids"].as_array().unwrap();
        let users: Vec<_> = actual["sources"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|source| targets.contains(&source["source_id"]) && source["role"] == "user")
            .map(|source| source["content"].as_str().unwrap())
            .collect();
        assert_eq!(users, ["strict turn 2"], "saved turn 1 must not be resent");
    }
    let after = engine.store.snapshot().unwrap();
    assert_recovered(&before, &after, unknown);
    // libtest may leave its test-name prefix on the current stdout line.
    println!(
        "\nstructured_failure_recovery {}",
        serde_json::json!({
            "child_pid":std::process::id(),"parent_pid":parent_pid,"unknown":unknown,
            "main_calls":main_count,"summary_calls":summaries.len(),"raw_user_turns":12,"live_provider":false
        })
    );
}

/// Kills and reaps a fixture child even when the parent test panics.
pub(super) struct KillOnDrop(pub std::process::Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// SIGKILL runs no destructor. Killed while the summary request is pending,
/// its intent is saved without a response and nothing is resent. Killed while
/// the passage embedding is held, the saved response is reused.
#[test]
fn structured10_kill_during_auxiliary_work_never_resends_and_reuses_saved_responses() {
    for (hold, unknown) in [("summary", true), ("passage", false)] {
        let (f, _main, _summaries) = structured_fixture();
        let database = f
            .memory_temp
            .as_ref()
            .unwrap()
            .path()
            .join("memory.sqlite3");
        let marker = f._temp.path().join("held");
        let metadata = f._temp.path().join("kill.json");
        std::fs::write(
            &metadata,
            serde_json::to_vec(&serde_json::json!({
                "store_root":f.config.store_root,"database":database,"marker":marker,
                "hold":hold,"parent_pid":std::process::id()
            }))
            .unwrap(),
        )
        .unwrap();
        let config = f.config.clone();
        drop(f.service);
        let log = f._temp.path().join("child.log");
        let mut child = KillOnDrop(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "engine::real::tests::structured_tests::recovery_tests::structured_kill_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("POLARIS_STRUCTURED_KILL_FIXTURE", &metadata)
                .env("POLARIS_FIXTURE_HOLD_MARKER", &marker)
                .stdout(std::process::Stdio::null())
                .stderr(std::fs::File::create(&log).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + Duration::from_secs(120);
        while !marker.exists() {
            let early = child.0.try_wait().unwrap().is_some();
            assert!(
                !early && std::time::Instant::now() < deadline,
                "{hold}: {}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(20));
        }
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        let reopened = DesktopService::open(config.clone()).unwrap();
        let before = reopened.engine.as_ref().unwrap().store.snapshot().unwrap();
        drop(reopened);
        assert_eq!(
            before.raw.iter().filter(|event| event.starts_turn).count(),
            11
        );
        resume_failure_in_child(&config, &database, &before, unknown);
    }
}

#[test]
#[ignore = "親テストが補助処理の保留中にSIGKILLする子プロセス"]
fn structured_kill_child() {
    let path =
        std::env::var_os("POLARIS_STRUCTURED_KILL_FIXTURE").expect("parent-owned fixture path");
    let input: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    assert_ne!(
        u64::from(std::process::id()),
        input["parent_pid"].as_u64().unwrap()
    );
    let path = |key: &str| std::path::PathBuf::from(input[key].as_str().unwrap());
    let summary = input["hold"] == "summary";
    let (mut f, _main, _summaries) = structured_fixture_with_index(
        false,
        if summary {
            EmbedderBehavior::Valid
        } else {
            EmbedderBehavior::HoldPassage
        },
        Some(path("database")),
        summary.then(|| path("marker")),
    );
    let engine = f.service.engine.as_ref().unwrap();
    let configuration = engine.store.snapshot().unwrap().state.configuration;
    let factory = engine.trusted_runs.as_ref().unwrap().factory.clone();
    drop(f.service);
    f.config.store_root = path("store_root");
    f.service =
        DesktopService::open_with_trusted_runs(f.config.clone(), configuration, factory).unwrap();
    let engine = f.service.engine.as_mut().unwrap();
    engine
        .handle(
            &request("kill-child-hello", RequestBody::Hello),
            &mut vec![],
        )
        .unwrap();
    for turn in 1..=10 {
        run_text(engine, turn);
    }
    queue_text(engine, 11);
    // No deadline: the parent kills this process while the work is held.
    loop {
        engine.step_real(None, false).unwrap();
        std::thread::sleep(Duration::from_millis(5));
    }
}
