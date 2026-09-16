//! Internal engine admission; the public resource gate remains closed.
use super::*;
use polaris_desktop_protocol::snapshot::{HistoryMode, MemoryPhase, MemoryRetrievalStatus};

fn structured_fixture() -> (Fixture, RecordedRequests, RecordedRequests) {
    structured_fixture_with(false, EmbedderBehavior::Valid)
}

fn structured_fixture_with(
    fail: bool,
    embedder: EmbedderBehavior,
) -> (Fixture, RecordedRequests, RecordedRequests) {
    structured_fixture_with_index(fail, embedder, None, None)
}

/// `hang` names a file written when the summary request arrives; that
/// request then never answers, so a parent can kill the process mid-request.
fn structured_fixture_with_index(
    fail: bool,
    embedder: EmbedderBehavior,
    index: Option<std::path::PathBuf>,
    hang: Option<std::path::PathBuf>,
) -> (Fixture, RecordedRequests, RecordedRequests) {
    let main = Arc::new(std::sync::Mutex::new(Vec::new()));
    let summaries = Arc::new(std::sync::Mutex::new(Vec::new()));
    let memory_temp = tempfile::tempdir().unwrap();
    let history = HistoryFixture {
        mode: HistoryMode::Structured10,
        summary_provider: Arc::new(StructuredSummary {
            calls: summaries.clone(),
            fail,
            hang,
        }),
        database: index.unwrap_or_else(|| memory_temp.path().join("memory.sqlite3")),
        identity: polaris_core::desktop_store::MemoryResources {
            embedding_model: "fixture-embedder".into(),
            embedding_revision: "v1".into(),
            embedding_dimension: 4,
            fingerprint: "a".repeat(64),
        },
        embedder,
    };
    let mut f = fixture_with_history(
        Behavior::Complete,
        false,
        Some(history),
        Some(Arc::new(RecordingProvider {
            calls: main.clone(),
            summary: false,
            fail: false,
            hold: false,
            read_source: false,
        })),
    );
    f.memory_temp = Some(memory_temp);
    (f, main, summaries)
}

#[test]
fn real_engine_structured10_initializes_absent_index_before_first_request() {
    let (mut f, main, summaries) = structured_fixture();
    let database = f
        .memory_temp
        .as_ref()
        .unwrap()
        .path()
        .join("memory.sqlite3");
    assert!(
        !database.exists(),
        "test must not initialize the product index"
    );
    let engine = f.service.engine.as_mut().unwrap();
    engine
        .handle(
            &request("structured-hello", RequestBody::Hello),
            &mut vec![],
        )
        .unwrap();
    run_text(engine, 1);
    let saved = engine.store.snapshot().unwrap();
    assert_eq!(
        main.lock().unwrap().len(),
        1,
        "cold Structured10 must reach the main provider"
    );
    assert!(
        summaries.lock().unwrap().is_empty(),
        "one direct turn requires no summary"
    );
    let memory = saved.state.runs.last().unwrap().memory.as_ref().unwrap();
    assert_eq!(memory.phase, MemoryPhase::Ready);
    assert_eq!(
        memory.retrieval_status,
        Some(MemoryRetrievalStatus::NotFound)
    );
    assert_eq!(memory.recent_raw_turns.get(), 1);
    assert!(memory.retrieval_sources.is_empty());
    assert!(memory.reference_tokens.get() > 0 && memory.reference_tokens.get() <= 768);
    assert_eq!(
        memory.main_usage.as_ref().unwrap().reported_responses.get(),
        1
    );
    assert!(database.is_file());
    drop(f.service);
    let reopened = DesktopService::open(f.config.clone()).unwrap();
    let restored = reopened.engine.as_ref().unwrap().store.snapshot().unwrap();
    assert_eq!(
        restored.state.runs.last().unwrap().memory.as_ref(),
        Some(memory)
    );
    assert_eq!(
        serde_json::to_vec(&restored.raw).unwrap(),
        serde_json::to_vec(&saved.raw).unwrap()
    );
}

struct StructuredSummary {
    calls: RecordedRequests,
    fail: bool,
    hang: Option<std::path::PathBuf>,
}
#[async_trait::async_trait]
impl Provider for StructuredSummary {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        // Coverage derives solely from the actual provider input, not an authored expected state.
        let input: serde_json::Value =
            serde_json::from_str(&request.messages.last().unwrap().content).unwrap();
        let ids = input["extraction_source_ids"].clone();
        let targets = input["summary_target_source_ids"].as_array().unwrap();
        let source = input["sources"]
            .as_array()
            .unwrap()
            .iter()
            .find(|s| targets.contains(&s["source_id"]) && s["role"] == "user")
            .unwrap();
        let turn: u64 = source["content"]
            .as_str()
            .unwrap()
            .strip_prefix("strict turn ")
            .unwrap()
            .parse()
            .unwrap();
        self.calls.lock().unwrap().push(request);
        if let Some(marker) = &self.hang {
            std::fs::write(marker, turn.to_string()).unwrap();
            std::future::pending::<()>().await;
        }
        if self.fail {
            return Err(ProviderError::Http("structured fixture failure".into()));
        }
        Ok(CompletionResponse {
            text: serde_json::json!({"summary":{"facts":[format!("expired fixture turn {turn}")],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[turn]},"processed_source_ids":ids,"entity_candidates":[],"fact_deltas":[],"entity_relations":[]}).to_string(),
            usage: Some(Usage { input_tokens: 5, output_tokens: 2, total_tokens: 7, cached_tokens: 0 }),
            ..Default::default()
        })
    }
}

#[test]
fn real_engine_structured10_compresses_metered_envelope_and_continues_after_reopen() {
    let (mut f, main, summaries) = structured_fixture();
    let engine = f.service.engine.as_mut().unwrap();
    let factory = engine.trusted_runs.as_ref().unwrap().factory.clone();
    let config = engine.store.snapshot().unwrap().state.configuration;
    engine
        .handle(
            &request("structured-history-hello", RequestBody::Hello),
            &mut vec![],
        )
        .unwrap();
    for turn in 1..=11 {
        run_text(engine, turn);
    }
    let before = engine.store.snapshot().unwrap();
    assert_eq!(main.lock().unwrap().len(), 11);
    assert_eq!(summaries.lock().unwrap().len(), 1);
    {
        let requests = main.lock().unwrap();
        let last = requests.last().unwrap();
        let users: Vec<_> = last
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| m.content.as_str())
            .collect();
        assert!(!users.contains(&"strict turn 1"));
        for turn in 2..=11 {
            assert!(users.contains(&format!("strict turn {turn}").as_str()));
        }
    }
    let status = before.state.runs.last().unwrap().memory.as_ref().unwrap();
    assert_eq!(status.phase, MemoryPhase::Ready);
    assert_eq!(status.recent_raw_turns.get(), 10);
    assert_eq!(
        status
            .summary_usage
            .as_ref()
            .unwrap()
            .reported_responses
            .get(),
        1
    );
    assert_eq!(
        status.main_usage.as_ref().unwrap().reported_responses.get(),
        1
    );
    assert_eq!(
        status
            .total_usage
            .as_ref()
            .unwrap()
            .reported_responses
            .get(),
        2
    );
    assert!(status.embedding_usage.as_ref().unwrap().completed.get() >= 1);
    drop(f.service);
    f.service = DesktopService::open_with_trusted_runs(f.config.clone(), config, factory).unwrap();
    let engine = f.service.engine.as_mut().unwrap();
    assert_eq!(
        engine
            .store
            .snapshot()
            .unwrap()
            .state
            .runs
            .last()
            .unwrap()
            .memory
            .as_ref(),
        Some(status)
    );
    engine
        .handle(
            &request("structured-reopen-hello", RequestBody::Hello),
            &mut vec![],
        )
        .unwrap();
    run_text(engine, 12);
    assert_eq!(main.lock().unwrap().len(), 12);
    assert_eq!(
        summaries.lock().unwrap().len(),
        2,
        "restart must not resummarize published turn 1"
    );
    let after = engine.store.snapshot().unwrap();
    assert_eq!(
        after
            .raw
            .iter()
            .filter(|r| r.message.role == Role::User)
            .count(),
        12
    );
    assert_eq!(
        after
            .state
            .runs
            .last()
            .unwrap()
            .memory
            .as_ref()
            .unwrap()
            .phase,
        MemoryPhase::Ready
    );
    let metadata = f._temp.path().join("process-reopen.json");
    let database = f
        .memory_temp
        .as_ref()
        .unwrap()
        .path()
        .join("memory.sqlite3");
    std::fs::write(
        &metadata,
        serde_json::to_vec(&serde_json::json!({
            "store_root":f.config.store_root,"database":database,"parent_pid":std::process::id()
        }))
        .unwrap(),
    )
    .unwrap();
    drop(f.service);
    let output = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            tokio::time::timeout(
                Duration::from_secs(120),
                tokio::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "engine::real::tests::structured_tests::structured_process_reopen_child",
                        "--ignored",
                        "--nocapture",
                    ])
                    .env("POLARIS_STRUCTURED_REOPEN_FIXTURE", &metadata)
                    .kill_on_drop(true)
                    .output(),
            )
            .await
            .expect("owned synthetic restart child timeout")
            .unwrap()
        });
    assert!(
        output.status.success(),
        "child stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("structured_process_resume "));
    print!("{stdout}");
    let reopened = DesktopService::open(f.config.clone()).unwrap();
    let restored = reopened.engine.as_ref().unwrap().store.snapshot().unwrap();
    assert_eq!(
        restored
            .raw
            .iter()
            .filter(|r| r.message.role == Role::User)
            .count(),
        13
    );
    assert_eq!(
        restored
            .state
            .runs
            .last()
            .unwrap()
            .memory
            .as_ref()
            .unwrap()
            .phase,
        MemoryPhase::Ready
    );
}

#[test]
fn real_engine_structured10_auxiliary_failure_preserves_raw_and_stops_main() {
    for (fail, embedder) in [
        (true, EmbedderBehavior::Valid),
        (false, EmbedderBehavior::FailPassage),
    ] {
        let (mut f, main, summaries) = structured_fixture_with(fail, embedder);
        let engine = f.service.engine.as_mut().unwrap();
        engine
            .handle(
                &request("structured-failure-hello", RequestBody::Hello),
                &mut vec![],
            )
            .unwrap();
        for turn in 1..=11 {
            run_text(engine, turn);
        }
        assert_eq!(main.lock().unwrap().len(), 10);
        assert_eq!(summaries.lock().unwrap().len(), 1);
        assert!(engine.active.is_none());
        let saved = engine.store.snapshot().unwrap();
        assert_eq!(
            saved
                .raw
                .iter()
                .filter(|r| r.message.role == Role::User)
                .count(),
            11
        );
        assert_ne!(
            saved
                .state
                .runs
                .last()
                .unwrap()
                .memory
                .as_ref()
                .unwrap()
                .phase,
            MemoryPhase::Ready
        );
        drop(f.service);
        let reopened = DesktopService::open(f.config.clone()).unwrap();
        let restored = reopened.engine.as_ref().unwrap().store.snapshot().unwrap();
        assert_eq!(
            serde_json::to_vec(&restored.raw).unwrap(),
            serde_json::to_vec(&saved.raw).unwrap()
        );
        drop(reopened);
        let database = f
            .memory_temp
            .as_ref()
            .unwrap()
            .path()
            .join("memory.sqlite3");
        recovery_tests::resume_failure_in_child(&f.config, &database, &saved, fail);
    }
}

#[test]
fn real_engine_structured10_initialization_failure_releases_run_and_can_retry() {
    let (mut f, main, summaries) = structured_fixture();
    let database = f
        .memory_temp
        .as_ref()
        .unwrap()
        .path()
        .join("memory.sqlite3");
    std::fs::write(&database, b"deliberately invalid sqlite fixture").unwrap();
    let engine = f.service.engine.as_mut().unwrap();
    engine
        .handle(
            &request("structured-bad-index-hello", RequestBody::Hello),
            &mut vec![],
        )
        .unwrap();
    run_text(engine, 1);
    assert!(engine.active.is_none());
    assert!(main.lock().unwrap().is_empty());
    assert!(summaries.lock().unwrap().is_empty());
    let saved = engine.store.snapshot().unwrap();
    assert!(saved.state.conversation_memory.is_none());
    assert!(saved.state.runs.last().unwrap().memory.is_none());
    assert_eq!(
        std::fs::read(&database).unwrap(),
        b"deliberately invalid sqlite fixture"
    );
    assert_eq!(
        saved
            .raw
            .iter()
            .filter(|r| r.message.role == Role::User)
            .count(),
        1
    );
    // Remove only this deliberately malformed, unbound temporary fixture.
    std::fs::remove_file(&database).unwrap();
    run_text(engine, 2);
    assert_eq!(main.lock().unwrap().len(), 1);
    assert!(summaries.lock().unwrap().is_empty());
    assert_eq!(
        engine
            .store
            .snapshot()
            .unwrap()
            .state
            .runs
            .last()
            .unwrap()
            .memory
            .as_ref()
            .unwrap()
            .phase,
        MemoryPhase::Ready
    );
}

#[test]
#[ignore = "親テストが一時保存先を指定して別プロセスで実行するfixture"]
fn structured_process_reopen_child() {
    let path =
        std::env::var_os("POLARIS_STRUCTURED_REOPEN_FIXTURE").expect("parent-owned fixture path");
    let input: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
    let parent_pid = input["parent_pid"].as_u64().unwrap();
    assert_ne!(u64::from(std::process::id()), parent_pid);
    let database = std::path::PathBuf::from(input["database"].as_str().unwrap());
    let store_root = std::path::PathBuf::from(input["store_root"].as_str().unwrap());
    assert!(
        database.is_absolute()
            && database.is_file()
            && store_root.is_absolute()
            && store_root.is_dir()
    );
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
    assert_eq!(
        engine
            .store
            .snapshot()
            .unwrap()
            .raw
            .iter()
            .filter(|r| r.message.role == Role::User)
            .count(),
        12
    );
    engine
        .handle(
            &request("child-structured-hello", RequestBody::Hello),
            &mut vec![],
        )
        .unwrap();
    run_text(engine, 13);
    assert_eq!(main.lock().unwrap().len(), 1);
    assert_eq!(summaries.lock().unwrap().len(), 1);
    let saved = engine.store.snapshot().unwrap();
    assert_eq!(
        saved
            .raw
            .iter()
            .filter(|r| r.message.role == Role::User)
            .count(),
        13
    );
    let status = saved.state.runs.last().unwrap().memory.as_ref().unwrap();
    assert_eq!(status.phase, MemoryPhase::Ready);
    assert_eq!(
        status.main_usage.as_ref().unwrap().reported_responses.get(),
        1
    );
    assert_eq!(
        status
            .summary_usage
            .as_ref()
            .unwrap()
            .reported_responses
            .get(),
        1
    );
    println!(
        "structured_process_resume {}",
        serde_json::json!({"child_pid":std::process::id(),"parent_pid":parent_pid,"main_calls":1,"summary_calls":1,"raw_user_turns":13,"phase":"ready","live_provider":false})
    );
}

mod recovery_tests;
