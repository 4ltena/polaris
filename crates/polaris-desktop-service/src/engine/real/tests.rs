//! Offline real-engine ownership, durable transcript, and cancellation fixtures.
use super::*;
use polaris_desktop_protocol::request::{DraftUpdate, RunStart};
use polaris_provider::{CompletionRequest, CompletionResponse, ProviderError, ToolCall, Usage};
use std::{future::Future, io, os::unix::fs::PermissionsExt, pin::Pin, sync::atomic::AtomicUsize};

type FixtureFuture<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;

#[derive(Clone, Copy)]
enum Behavior {
    Spawn,
    SpawnPending,
    Complete,
    Held,
    Blocking,
    Rejected,
    Pending,
    Approval,
    Error,
    Panic,
}
struct FakeProvider {
    calls: Arc<AtomicUsize>,
    release: Arc<AtomicBool>,
    behavior: Behavior,
}
#[async_trait::async_trait]
impl Provider for FakeProvider {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if matches!(self.behavior, Behavior::Spawn | Behavior::SpawnPending) {
            let last = request.messages.last().unwrap();
            if last.content == "child fixture" {
                if matches!(self.behavior, Behavior::SpawnPending) {
                    return std::future::pending().await;
                }
                return Ok(CompletionResponse {
                    text: "{}".into(),
                    ..Default::default()
                });
            }
            return Ok(if last.role == Role::Tool {
                CompletionResponse {
                    text: "parent complete".into(),
                    ..Default::default()
                }
            } else {
                CompletionResponse {
                    tool_calls: vec![ToolCall {
                        id: "spawn-call".into(),
                        name: "spawn".into(),
                        arguments: serde_json::json!({"tasks":[{"type":"child-fixture", "task":"child fixture"}]}),
                    }],
                    ..Default::default()
                }
            });
        }
        if request.messages.last().unwrap().content == "continue explicitly" {
            assert!(matches!(self.behavior, Behavior::Approval));
            let marker = &request.messages[request.messages.len() - 2];
            assert_eq!(marker.role, Role::Tool);
            assert_eq!(marker.tool_call_id.as_deref(), Some("fixture-call"));
            assert!(marker.content.contains("結果は不明"));
            return Ok(CompletionResponse {
                text: "continued without replay".into(),
                ..Default::default()
            });
        }
        assert_eq!(request.messages.last().unwrap().content, "fixture input");
        match self.behavior {
            Behavior::Spawn | Behavior::SpawnPending => unreachable!(),
            Behavior::Complete | Behavior::Held | Behavior::Blocking | Behavior::Rejected => {
                let deadline = std::time::Instant::now() + Duration::from_secs(8);
                while !self.release.load(Ordering::Acquire) {
                    if matches!(self.behavior, Behavior::Blocking) {
                        // Deliberately occupy the worker's current poll. This
                        // fixture cannot observe cancellation until released.
                        if std::time::Instant::now() >= deadline {
                            return Err(ProviderError::Http("fixture release deadline".into()));
                        }
                        std::thread::sleep(Duration::from_millis(5));
                    } else {
                        tokio::time::sleep(Duration::from_millis(1)).await;
                    }
                }
                Ok(CompletionResponse {
                    text: if matches!(self.behavior, Behavior::Rejected) {
                        "x".repeat(128 * 1024 + 1)
                    } else {
                        "raw completion".into()
                    },
                    usage: Some(Usage {
                        input_tokens: 7,
                        output_tokens: 3,
                        total_tokens: 10,
                        cached_tokens: 0,
                    }),
                    ..Default::default()
                })
            }
            Behavior::Pending => std::future::pending().await,
            Behavior::Approval => Ok(CompletionResponse {
                tool_calls: vec![ToolCall {
                    id: "fixture-call".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command":"printf never-started"}),
                }],
                ..Default::default()
            }),
            Behavior::Error => Err(ProviderError::Http("fixture error".into())),
            Behavior::Panic => panic!("fixture panic"),
        }
    }
}
struct Factory {
    prepared: Arc<PreparedWorkspace>,
    provider: Arc<dyn Provider>,
    audit: Arc<tokio::sync::Mutex<AuditLog>>,
    preparations: Arc<AtomicUsize>,
    invalid_limits: bool,
    agent_types: Vec<polaris_skills::AgentType>,
    completions: Arc<std::sync::Mutex<Vec<TrustedRunCompletion>>>,
    history: Option<HistoryFixture>,
}
impl TrustedRunFactory for Factory {
    fn completed(&self, completion: TrustedRunCompletion) {
        self.completions.lock().unwrap().push(completion);
    }
    fn prepare(
        &self,
        target: &RunTarget,
        published: &Published,
    ) -> Result<TrustedRunInputs, ServiceError> {
        self.preparations.fetch_add(1, Ordering::SeqCst);
        let run = published
            .state
            .runs
            .iter()
            .find(|r| r.run.run_id == target.run_id)
            .unwrap();
        assert_eq!(
            run.operations.len(),
            1,
            "intent must precede factory/provider"
        );
        if self.history.is_none() {
            assert!(matches!(
                published.raw.last().unwrap().message.content.as_str(),
                "fixture input" | "continue explicitly"
            ));
        }
        Ok(TrustedRunInputs {
            prepared: self.prepared.clone(),
            mutation_policy: None,
            execution_capability: None,
            provider: self.provider.clone(),
            provider_pool: self.provider.clone(),
            always_on: polaris_core::prompt::assemble_always_on("", "", &[]),
            audit: self.audit.clone(),
            max_turns: if self.invalid_limits { 0 } else { 4 },
            skills: vec![],
            agent_types: self.agent_types.clone(),
            spawn_concurrency: 1,
            spawn_write_concurrency: 1,
            tool_memory: None,
            history: self.history.as_ref().map(HistoryFixture::resources),
        })
    }
}

#[derive(Clone)]
struct HistoryFixture {
    summary_provider: Arc<dyn Provider>,
    database: std::path::PathBuf,
    identity: polaris_core::desktop_store::MemoryResources,
    embedder: EmbedderBehavior,
}
impl HistoryFixture {
    fn resources(&self) -> TrustedHistoryResources {
        let ledger = polaris_provider::attempts::AttemptLedger::default();
        TrustedHistoryResources {
            summary_provider: self.summary_provider.clone(),
            embedder: Arc::new(FixtureEmbedder {
                model: polaris_core::conversation_memory::EmbeddingModel {
                    model: self.identity.embedding_model.clone(),
                    revision: self.identity.embedding_revision.clone(),
                    dimension: i64::from(self.identity.embedding_dimension),
                },
                behavior: self.embedder,
                ledger: ledger.clone(),
            }),
            database: self.database.clone(),
            identity: self.identity.clone(),
            embedding_usage: ledger,
        }
    }
}

#[derive(Clone, Copy)]
enum EmbedderBehavior {
    Valid,
    FailPassage,
    FailQuery,
    HoldPassage,
    HoldQuery,
}
struct FixtureEmbedder {
    model: polaris_core::conversation_memory::EmbeddingModel,
    behavior: EmbedderBehavior,
    ledger: polaris_provider::attempts::AttemptLedger,
}
impl FixtureEmbedder {
    async fn result(&self, passage: bool) -> io::Result<Vec<f32>> {
        let mut attempt = self
            .ledger
            .begin_embedding(&self.model.model)
            .map_err(io::Error::other)?;
        if matches!(self.behavior, EmbedderBehavior::HoldPassage) && passage
            || matches!(self.behavior, EmbedderBehavior::HoldQuery) && !passage
        {
            return std::future::pending().await;
        }
        let ok = matches!(self.behavior, EmbedderBehavior::Valid)
            || matches!(self.behavior, EmbedderBehavior::FailPassage) && !passage
            || matches!(self.behavior, EmbedderBehavior::FailQuery) && passage
            || matches!(self.behavior, EmbedderBehavior::HoldPassage) && !passage
            || matches!(self.behavior, EmbedderBehavior::HoldQuery) && passage;
        attempt.finish(Some(4), ok);
        if ok {
            Ok(vec![1.0; self.model.dimension as usize])
        } else {
            Err(io::Error::other("fixture embedding failure"))
        }
    }
}
impl polaris_core::conversation_memory::StrictEmbedder for FixtureEmbedder {
    fn metadata(&self) -> &polaris_core::conversation_memory::EmbeddingModel {
        &self.model
    }
    fn embed_passage<'a>(&'a self, _: &'a str) -> FixtureFuture<'a, Vec<f32>> {
        Box::pin(async move { self.result(true).await })
    }
    fn embed_query<'a>(&'a self, _: &'a str) -> FixtureFuture<'a, Vec<Vec<f32>>> {
        Box::pin(async move { self.result(false).await.map(|v| vec![v]) })
    }
}

struct RecordingProvider {
    calls: Arc<std::sync::Mutex<Vec<CompletionRequest>>>,
    summary: bool,
    fail: bool,
    hold: bool,
    read_source: bool,
}
#[async_trait::async_trait]
impl Provider for RecordingProvider {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let uri = request
            .messages
            .iter()
            .flat_map(|m| m.content.lines())
            .find(|line| line.starts_with("conversation://"))
            .map(str::to_owned);
        let original_read = request
            .messages
            .last()
            .filter(|m| m.role == Role::Tool)
            .map(|m| m.content.clone());
        self.calls.lock().unwrap().push(request);
        if self.hold {
            return std::future::pending().await;
        }
        if self.fail {
            return Err(ProviderError::Http("fixture failure".into()));
        }
        if self.summary {
            return Ok(CompletionResponse { text: r#"{"facts":["old evidence"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[1]}"#.into(), usage: Some(Usage { input_tokens: 5, output_tokens: 2, total_tokens: 7, cached_tokens: 0 }), ..Default::default() });
        }
        let mut response = CompletionResponse {
            text: "strict raw completion".into(),
            usage: Some(Usage {
                input_tokens: 11,
                output_tokens: 4,
                total_tokens: 15,
                cached_tokens: 0,
            }),
            ..Default::default()
        };
        if self.read_source {
            if let Some(original) = original_read {
                assert!(
                    original.contains("strict turn 1"),
                    "verified original source must reach the second request"
                );
                assert!(
                    original.len() <= 4096 && polaris_core::budget::count_tokens(&original) <= 1024
                );
            } else if let Some(uri) = uri {
                response.text.clear();
                response.tool_calls = vec![ToolCall {
                    id: "source-read".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path":uri}),
                }];
            }
        }
        Ok(response)
    }
}
struct Fixture {
    _temp: tempfile::TempDir,
    memory_temp: Option<tempfile::TempDir>,
    service: DesktopService,
    calls: Arc<AtomicUsize>,
    preparations: Arc<AtomicUsize>,
    release: Arc<AtomicBool>,
    config: ServiceConfig,
    completions: Arc<std::sync::Mutex<Vec<TrustedRunCompletion>>>,
}
fn fixture(behavior: Behavior, invalid_limits: bool) -> Fixture {
    fixture_with_history(behavior, invalid_limits, None, None)
}
fn fixture_with_history(
    behavior: Behavior,
    invalid_limits: bool,
    history: Option<HistoryFixture>,
    provider_override: Option<Arc<dyn Provider>>,
) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let source = base.join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("ordinary"), "fixture").unwrap();
    let helper = base.join("helper");
    std::fs::write(&helper, "dummy executable, never launched").unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    let prepared = Arc::new(
        PreparedWorkspace::prepare(
            &source,
            &helper,
            polaris_core::desktop_execution::SandboxMode::WorkspaceWrite,
            &[],
            polaris_core::isolated_workspace::Limits {
                max_entries: 16,
                max_files: 8,
                max_file_bytes: 1024,
                max_total_bytes: 4096,
                max_depth: 4,
            },
        )
        .unwrap(),
    );
    let calls = Arc::new(AtomicUsize::new(0));
    let release = Arc::new(AtomicBool::new(!matches!(
        behavior,
        Behavior::Held | Behavior::Blocking
    )));
    let preparations = Arc::new(AtomicUsize::new(0));
    let completions = Arc::new(std::sync::Mutex::new(Vec::new()));
    let schema = base.join("child-schema.json");
    std::fs::write(&schema, "{}").unwrap();
    let factory = Arc::new(Factory {
        completions: completions.clone(),
        prepared,
        provider: provider_override.unwrap_or_else(|| {
            Arc::new(FakeProvider {
                calls: calls.clone(),
                release: release.clone(),
                behavior,
            })
        }),
        audit: Arc::new(tokio::sync::Mutex::new(
            AuditLog::open(&base.join("audit.jsonl")).unwrap(),
        )),
        preparations: preparations.clone(),
        invalid_limits,
        agent_types: vec![polaris_skills::AgentType {
            name: "child-fixture".into(),
            description: "offline child".into(),
            body: "return JSON".into(),
            path: base.clone(),
            allowed_tools: vec![],
            access: polaris_skills::agent_type::AgentAccess::Read,
            tier: "medium".into(),
            wall_seconds: 5,
            max_turns: 2,
            workflow_phase: None,
            continuation: false,
            output_schema: schema,
        }],
        history: history.clone(),
    });
    std::fs::create_dir(base.join("store")).unwrap();
    std::fs::set_permissions(base.join("store"), std::fs::Permissions::from_mode(0o700)).unwrap();
    let config = ServiceConfig {
        store_root: base.join("store"),
        project_id: project_id(),
        session_id: session_id(),
    };
    let service = DesktopService::open_with_trusted_runs(
        config.clone(),
        Configuration {
            history_mode: if history.is_some() {
                polaris_desktop_protocol::snapshot::HistoryMode::Strict10
            } else {
                Default::default()
            },
            configuration_revision: DecimalU64::new(0),
            provider: "fixture".into(),
            model: "offline".into(),
            effort: "none".into(),
        },
        factory,
    )
    .unwrap();
    Fixture {
        _temp: temp,
        memory_temp: None,
        service,
        calls,
        preparations,
        release,
        config,
        completions,
    }
}

type RecordedRequests = Arc<std::sync::Mutex<Vec<CompletionRequest>>>;

fn strict_fixture(
    summary_fails: bool,
    embedder: EmbedderBehavior,
) -> (Fixture, RecordedRequests, RecordedRequests) {
    strict_fixture_with(summary_fails, embedder, false, false)
}
fn strict_fixture_with(
    summary_fails: bool,
    embedder: EmbedderBehavior,
    hold_summary: bool,
    read_source: bool,
) -> (Fixture, RecordedRequests, RecordedRequests) {
    let main_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let summary_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let memory_temp = tempfile::tempdir().unwrap();
    let history = HistoryFixture {
        summary_provider: Arc::new(RecordingProvider {
            calls: summary_calls.clone(),
            summary: true,
            fail: summary_fails,
            hold: hold_summary,
            read_source: false,
        }),
        database: memory_temp.path().join("memory.sqlite3"),
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
            calls: main_calls.clone(),
            summary: false,
            fail: false,
            hold: false,
            read_source,
        })),
    );
    f.memory_temp = Some(memory_temp);
    (f, main_calls, summary_calls)
}
fn request(id: &str, body: RequestBody) -> Request {
    Request {
        protocol_version: ProtocolVersion,
        client_id: ClientId::new("real-test").unwrap(),
        request_id: RequestId::new(id).unwrap(),
        body,
    }
}
fn start(engine: &mut Engine) -> Request {
    engine
        .handle(&request("hello", RequestBody::Hello), &mut vec![])
        .unwrap();
    engine
        .handle(
            &request(
                "draft",
                RequestBody::DraftUpdate(
                    session_id(),
                    DraftUpdate {
                        expected_draft_revision: DecimalU64::new(0),
                        text: "fixture input".into(),
                        attachment_ids: vec![],
                    },
                ),
            ),
            &mut vec![],
        )
        .unwrap();
    let start = request(
        "start",
        RequestBody::RunStart(
            session_id(),
            RunStart {
                expected_draft_revision: DecimalU64::new(1),
                expected_configuration_revision: DecimalU64::new(0),
                expected_policy_revision: DecimalU64::new(0),
            },
        ),
    );
    engine.handle(&start, &mut vec![]).unwrap();
    start
}
fn until(engine: &mut Engine, predicate: impl Fn(&Engine) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        engine.step_real(None, false).unwrap();
        if predicate(engine) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "fixture completion deadline"
        );
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn queue_text(engine: &mut Engine, ordinal: u64) {
    let saved = engine.store.snapshot().unwrap();
    let text = format!("strict turn {ordinal}");
    engine
        .handle(
            &request(
                &format!("strict-draft-{ordinal}"),
                RequestBody::DraftUpdate(
                    session_id(),
                    DraftUpdate {
                        expected_draft_revision: saved.state.draft.draft_revision,
                        text,
                        attachment_ids: vec![],
                    },
                ),
            ),
            &mut vec![],
        )
        .unwrap();
    let saved = engine.store.snapshot().unwrap();
    engine
        .handle(
            &request(
                &format!("strict-start-{ordinal}"),
                RequestBody::RunStart(
                    session_id(),
                    RunStart {
                        expected_draft_revision: saved.state.draft.draft_revision,
                        expected_configuration_revision: saved
                            .state
                            .configuration
                            .configuration_revision,
                        expected_policy_revision: saved.state.policy_revision,
                    },
                ),
            ),
            &mut vec![],
        )
        .unwrap();
}
fn run_text(engine: &mut Engine, ordinal: u64) {
    queue_text(engine, ordinal);
    until(engine, |e| e.active.is_none());
}

#[test]
fn real_engine_strict10_uses_v3_raw_owner_and_keeps_only_recent_request_turns() {
    let (mut f, main_requests, summary_requests) = strict_fixture(false, EmbedderBehavior::Valid);
    let engine = f.service.engine.as_mut().unwrap();
    engine
        .handle(&request("strict-hello", RequestBody::Hello), &mut vec![])
        .unwrap();
    for turn in 1..=11 {
        run_text(engine, turn);
    }

    let saved = engine.store.snapshot().unwrap();
    assert_eq!(
        saved
            .raw
            .iter()
            .filter(|raw| raw.message.role == Role::User)
            .count(),
        11
    );
    assert!(
        saved
            .raw
            .iter()
            .any(|raw| raw.message.content == "strict turn 1")
    );
    assert_eq!(
        summary_requests.lock().unwrap().len(),
        1,
        "expired turn is summarized once"
    );
    let requests = main_requests.lock().unwrap();
    assert_eq!(requests.len(), 11);
    let users: Vec<_> = requests
        .last()
        .unwrap()
        .messages
        .iter()
        .filter(|message| message.role == Role::User)
        .map(|message| message.content.as_str())
        .collect();
    assert!(!users.contains(&"strict turn 1"));
    for turn in 2..=11 {
        assert!(
            users
                .iter()
                .any(|text| *text == format!("strict turn {turn}"))
        );
    }
    let run = saved.state.runs.last().unwrap();
    let memory = run.memory.as_ref().unwrap();
    assert_eq!(
        memory.phase,
        polaris_desktop_protocol::snapshot::MemoryPhase::Ready
    );
    assert_eq!(memory.recent_raw_turns, DecimalU64::new(10));
    assert!(memory.reference_tokens.get() <= 768);
    assert!(memory.retrieval_sources.len() <= 3);
    assert!(
        memory
            .summary_usage
            .as_ref()
            .is_some_and(|usage| usage.reported_responses == DecimalU64::new(1))
    );
    assert!(
        memory
            .main_usage
            .as_ref()
            .is_some_and(|usage| usage.reported_responses == DecimalU64::new(1))
    );
    assert!(
        memory
            .total_usage
            .as_ref()
            .is_some_and(|usage| usage.reported_responses == DecimalU64::new(2))
    );
    assert!(
        memory
            .embedding_usage
            .as_ref()
            .is_some_and(|usage| usage.completed.get() >= 1)
    );
    drop(requests);
    drop(f.service);
    let reopened = DesktopService::open(f.config.clone()).unwrap();
    let restored = reopened.engine.as_ref().unwrap().store.snapshot().unwrap();
    assert_eq!(
        serde_json::to_vec(&restored.raw).unwrap(),
        serde_json::to_vec(&saved.raw).unwrap(),
        "restart retains the full v3 raw transcript"
    );
    assert_eq!(restored.state.runs.last().unwrap().memory, run.memory);
}

#[test]
fn real_engine_strict10_summary_or_embedding_failure_keeps_raw_and_starts_no_eleventh_main_request()
{
    for (summary_fails, embedder) in [
        (true, EmbedderBehavior::Valid),
        (false, EmbedderBehavior::FailPassage),
        (false, EmbedderBehavior::FailQuery),
    ] {
        let (mut f, main_requests, summary_requests) = strict_fixture(summary_fails, embedder);
        let engine = f.service.engine.as_mut().unwrap();
        engine
            .handle(
                &request("strict-negative-hello", RequestBody::Hello),
                &mut vec![],
            )
            .unwrap();
        let turns = if matches!(embedder, EmbedderBehavior::FailQuery) {
            1
        } else {
            11
        };
        for turn in 1..=turns {
            run_text(engine, turn);
        }
        let saved = engine.store.snapshot().unwrap();
        assert_eq!(
            saved
                .raw
                .iter()
                .filter(|raw| raw.message.role == Role::User)
                .count(),
            turns as usize
        );
        assert!(
            saved
                .raw
                .iter()
                .any(|raw| raw.message.content == "strict turn 1")
        );
        assert_eq!(
            summary_requests.lock().unwrap().len(),
            if matches!(embedder, EmbedderBehavior::FailQuery) {
                0
            } else {
                1
            }
        );
        assert_eq!(
            main_requests.lock().unwrap().len(),
            if summary_fails || matches!(embedder, EmbedderBehavior::FailPassage) {
                10
            } else {
                0
            },
            "failed preparation must not start the main request"
        );
        let run = saved.state.runs.last().unwrap();
        if summary_fails {
            assert_eq!(run.run.state, RunState::OutcomeUnknown);
            assert_eq!(
                run.memory.as_ref().unwrap().phase,
                polaris_desktop_protocol::snapshot::MemoryPhase::OutcomeUnknown
            );
        } else {
            assert_eq!(run.run.state, RunState::Failed);
            assert_eq!(
                run.memory.as_ref().unwrap().phase,
                polaris_desktop_protocol::snapshot::MemoryPhase::Failed
            );
        }
    }
}
#[test]
fn real_engine_strict10_oversized_provenance_stops_before_embedding_and_main_without_replay() {
    struct OverBudgetSummary {
        calls: Arc<AtomicUsize>,
        text: String,
    }
    #[async_trait::async_trait]
    impl Provider for OverBudgetSummary {
        async fn complete(
            &self,
            _: CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(CompletionResponse {
                text: self.text.clone(),
                usage: Some(Usage {
                    input_tokens: 5,
                    output_tokens: 2,
                    total_tokens: 7,
                    cached_tokens: 0,
                }),
                ..Default::default()
            })
        }
    }
    let summaries = Arc::new(AtomicUsize::new(0));
    let text = serde_json::json!({"facts":["word ".repeat(210)],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[1]}).to_string();
    assert!(polaris_core::budget::count_tokens(&text) <= 256);
    let database = tempfile::tempdir().unwrap();
    let history = HistoryFixture {
        summary_provider: Arc::new(OverBudgetSummary {
            calls: summaries.clone(),
            text,
        }),
        database: database.path().join("memory.sqlite3"),
        identity: polaris_core::desktop_store::MemoryResources {
            embedding_model: "fixture-embedder".into(),
            embedding_revision: "v1".into(),
            embedding_dimension: 4,
            fingerprint: "a".repeat(64),
        },
        embedder: EmbedderBehavior::Valid,
    };
    let main_calls = Arc::new(std::sync::Mutex::new(Vec::new()));
    let mut fixture = fixture_with_history(
        Behavior::Complete,
        false,
        Some(history),
        Some(Arc::new(RecordingProvider {
            calls: main_calls.clone(),
            summary: false,
            fail: false,
            hold: false,
            read_source: false,
        })),
    );
    let engine = fixture.service.engine.as_mut().unwrap();
    engine
        .handle(
            &request("over-budget-hello", RequestBody::Hello),
            &mut vec![],
        )
        .unwrap();
    for turn in 1..=10 {
        run_text(engine, turn);
    }
    assert_eq!(main_calls.lock().unwrap().len(), 10);
    run_text(engine, 11);
    let saved = engine.store.snapshot().unwrap();
    let run = saved.state.runs.last().unwrap();
    assert_eq!(run.run.state, RunState::OutcomeUnknown);
    let memory = run.memory.as_ref().unwrap();
    assert_eq!(
        memory.detail,
        "出典情報を含む記憶が256トークンを超えたため主要求を開始していません。要約は自動再送していません"
    );
    assert_eq!(
        memory
            .summary_usage
            .as_ref()
            .unwrap()
            .reported_responses
            .get(),
        1
    );
    assert!(memory.main_usage.is_none());
    assert!(
        memory
            .embedding_usage
            .as_ref()
            .is_none_or(|usage| usage.requests.get() == 0)
    );
    assert_eq!(saved.raw.iter().filter(|raw| raw.starts_turn).count(), 11);
    assert!(
        saved
            .raw
            .iter()
            .any(|raw| raw.message.content == "strict turn 1")
    );
    assert_eq!(main_calls.lock().unwrap().len(), 10);
    assert_eq!(summaries.load(Ordering::SeqCst), 1);
    run_text(engine, 12);
    assert_eq!(main_calls.lock().unwrap().len(), 10);
    assert_eq!(
        summaries.load(Ordering::SeqCst),
        1,
        "recovery must not repeat the summary call"
    );
}

#[test]
fn real_engine_strict10_cancellation_stops_summary_and_embeddings_without_replay() {
    for (held_summary, embedding, turns) in [
        (true, EmbedderBehavior::Valid, 11),
        (false, EmbedderBehavior::HoldPassage, 11),
        (false, EmbedderBehavior::HoldQuery, 1),
    ] {
        let (mut f, main, summaries) = strict_fixture_with(false, embedding, held_summary, false);
        let engine = f.service.engine.as_mut().unwrap();
        engine
            .handle(
                &request("strict-cancel-hello", RequestBody::Hello),
                &mut vec![],
            )
            .unwrap();
        for turn in 1..turns {
            run_text(engine, turn);
        }
        queue_text(engine, turns);
        until(engine, |e| {
            if held_summary {
                !summaries.lock().unwrap().is_empty()
            } else {
                e.active
                    .as_ref()
                    .and_then(|a| a.real.as_ref())
                    .and_then(|r| r.embedding_usage.as_ref())
                    .is_some_and(|ledger| {
                        ledger
                            .snapshot()
                            .iter()
                            .any(|r| r.status == polaris_provider::attempts::AttemptStatus::Running)
                    })
            }
        });
        let target = engine.active.as_ref().unwrap().target.clone();
        engine
            .handle(
                &request(
                    "strict-cancel",
                    RequestBody::RunCancel(session_id(), target),
                ),
                &mut vec![],
            )
            .unwrap();
        until(engine, |e| e.active.is_none());
        let saved = engine.store.snapshot().unwrap();
        let run = saved.state.runs.last().unwrap();
        assert_eq!(main.lock().unwrap().len(), (turns - 1) as usize);
        assert_eq!(
            saved.raw.iter().filter(|r| r.starts_turn).count(),
            turns as usize
        );
        assert_eq!(
            run.run.state,
            if held_summary {
                RunState::OutcomeUnknown
            } else {
                RunState::Cancelled
            }
        );
        let status = run.memory.as_ref().unwrap();
        assert!(status.main_usage.is_none());
        if held_summary {
            assert_eq!(
                status.summary_usage.as_ref().unwrap().failed_requests.get(),
                1
            );
            assert!(
                status.embedding_usage.is_none(),
                "cancelled summary must not start embedding"
            );
            run_text(engine, 12);
            assert_eq!(
                summaries.lock().unwrap().len(),
                1,
                "unknown summary is not resent on an explicit continuation"
            );
            assert_eq!(main.lock().unwrap().len(), 10);
        } else {
            let usage = status.embedding_usage.as_ref().unwrap();
            assert_eq!(usage.requests.get(), 1);
            assert_eq!(usage.unknown.get(), 1);
            assert_eq!(usage.completed.get(), 0);
        }
    }
}

#[test]
fn real_engine_strict10_original_uri_tool_roundtrip_keeps_request_evidence_out_of_raw() {
    let (mut f, main, summaries) = strict_fixture_with(false, EmbedderBehavior::Valid, false, true);
    let engine = f.service.engine.as_mut().unwrap();
    engine
        .handle(
            &request("strict-uri-hello", RequestBody::Hello),
            &mut vec![],
        )
        .unwrap();
    for turn in 1..=11 {
        run_text(engine, turn);
    }
    let saved = engine.store.snapshot().unwrap();
    assert_eq!(summaries.lock().unwrap().len(), 1);
    assert_eq!(
        main.lock().unwrap().len(),
        12,
        "one original read followed by a final response"
    );
    assert_eq!(
        saved
            .raw
            .iter()
            .filter(|r| r.message.role == Role::Tool)
            .count(),
        1
    );
    assert!(
        saved
            .raw
            .iter()
            .any(|r| r.message.tool_call_id.as_deref() == Some("source-read")
                && r.message.content.contains("strict turn 1"))
    );
    assert!(
        !saved.raw.iter().any(|r| r.message.role == Role::User
            && r.message.content.contains("Historical evidence only"))
    );
    assert_eq!(
        saved.raw.last().unwrap().message.content,
        "strict raw completion"
    );
    let memory = saved.state.runs.last().unwrap().memory.as_ref().unwrap();
    assert_eq!(
        memory.main_usage.as_ref().unwrap().reported_responses.get(),
        2
    );
    assert_eq!(
        memory
            .summary_usage
            .as_ref()
            .unwrap()
            .reported_responses
            .get(),
        1
    );
    assert_eq!(
        memory
            .total_usage
            .as_ref()
            .unwrap()
            .reported_responses
            .get(),
        3
    );
}

#[test]
fn real_engine_persists_intent_usage_raw_suffix_and_never_replays() {
    let mut f = fixture(Behavior::Complete, false);
    assert!(!f.service.source_apply_available());
    let engine = f.service.engine.as_mut().unwrap();
    let start = start(engine);
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    engine.handle(&start, &mut vec![]).unwrap();
    until(engine, |e| e.active.is_none());
    let saved = engine.store.snapshot().unwrap();
    assert_eq!(saved.raw.len(), 2);
    assert_eq!(saved.raw[1].message.content, "raw completion");
    assert_eq!(saved.state.runs[0].run.state, RunState::Succeeded);
    assert_eq!(saved.state.runs[0].usage.unwrap().usage.total_tokens, 10);
    engine.handle(&start, &mut vec![]).unwrap();
    assert!(engine.active.is_none());
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    assert_eq!(f.preparations.load(Ordering::SeqCst), 1);
    drop(f.service);
    let reopened = DesktopService::open(f.config).unwrap();
    let saved_again = reopened.engine.as_ref().unwrap().store.snapshot().unwrap();
    assert_eq!(saved_again.marker, saved.marker);
    assert_eq!(saved_again.raw[1].message.content, "raw completion");
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn real_engine_completion_handoff_waits_for_join_and_retains_copy_once() {
    let mut f = fixture(Behavior::Held, false);
    let engine = f.service.engine.as_mut().unwrap();
    let request = start(engine);
    until(engine, |_| f.calls.load(Ordering::SeqCst) == 1);
    assert!(f.completions.lock().unwrap().is_empty());
    f.release.store(true, Ordering::Release);
    until(engine, |e| e.active.is_none());
    let saved = engine.store.snapshot().unwrap();
    engine.handle(&request, &mut vec![]).unwrap();
    let mut receipts = f.completions.lock().unwrap();
    assert_eq!(receipts.len(), 1);
    let receipt = receipts.pop().unwrap();
    assert_eq!(receipt.project_id(), &saved.marker.project_id);
    assert_eq!(receipt.session_id(), &saved.marker.session_id);
    assert_eq!(receipt.terminal(), &saved.state.runs[0]);
    assert_eq!(receipt.target().run_id, saved.state.runs[0].run.run_id);
    assert_eq!(receipt.session_revision(), saved.marker.session_revision);
    assert_eq!(receipt.terminal().run.state, RunState::Succeeded);
    drop(receipts);
    drop(f.service);
    // The engine and factory are gone; the handoff still owns the sanitized copy.
    assert_eq!(
        std::fs::read_to_string(receipt.prepared().snapshot().path().join("ordinary")).unwrap(),
        "fixture"
    );
}

#[test]
fn real_engine_completion_handoff_is_withheld_on_storage_failure() {
    let mut f = fixture(Behavior::Held, false);
    let engine = f.service.engine.as_mut().unwrap();
    start(engine);
    until(engine, |_| f.calls.load(Ordering::SeqCst) == 1);
    f.release.store(true, Ordering::Release);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let active = engine.active.as_mut().unwrap();
        let real = active.real.as_mut().unwrap();
        real.poll(&active.target, Some(&mut engine.store)).unwrap();
        if real.outcome.is_some() && real.events.is_none() {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    // Core has joined, but no durable terminal checkpoint has been published.
    let displaced = f.config.store_root.with_extension("displaced");
    std::fs::rename(&f.config.store_root, displaced).unwrap();
    std::fs::create_dir(&f.config.store_root).unwrap();
    assert!(engine.step_real(None, false).is_err());
    assert!(f.completions.lock().unwrap().is_empty());
}

#[test]
fn real_engine_completion_handoff_requires_an_actual_join() {
    let mut f = fixture(Behavior::Complete, true);
    let engine = f.service.engine.as_mut().unwrap();
    start(engine);
    until(engine, |e| e.active.is_none());
    assert_eq!(f.preparations.load(Ordering::SeqCst), 1);
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    assert_eq!(
        engine.store.snapshot().unwrap().state.runs[0].run.state,
        RunState::Failed
    );
    assert!(f.completions.lock().unwrap().is_empty());
}

#[test]
fn real_engine_completed_copy_apply_keeps_run_and_recovery_evidence() {
    use polaris_core::{
        desktop_store::{
            SourceApplyCandidate, SourceApplyEntry, SourceApplyGuard, SourceApplyIdentity,
            SourceApplyIdentityProof, SourceApplyPayload, SourceApplyResult, SourceApplyVersion,
        },
        isolated_workspace::{self, Limits, RegisteredSecrets},
        workspace_apply::{RecoveryParent, protected_apply_pinned},
    };
    use polaris_desktop_protocol::snapshot::ApprovalDecision;
    use std::os::unix::fs::MetadataExt;
    let mut f = fixture(Behavior::Complete, false);
    let engine = f.service.engine.as_mut().unwrap();
    start(engine);
    until(engine, |e| e.active.is_none());
    let receipt = f.completions.lock().unwrap().pop().unwrap();
    let before = engine.store.snapshot().unwrap();
    let snapshot = receipt.prepared().snapshot();
    // Synthetic controller fixture only: no model, live source or public RPC.
    std::fs::write(snapshot.path().join("ordinary"), "edited fixture").unwrap();
    let limits = Limits {
        max_entries: 16,
        max_files: 8,
        max_file_bytes: 1024,
        max_total_bytes: 4096,
        max_depth: 4,
    };
    let changes =
        isolated_workspace::collect_changes(snapshot, &RegisteredSecrets::new([]).unwrap(), limits)
            .unwrap();
    let recovery_path = f._temp.path().canonicalize().unwrap().join("recovery");
    std::fs::create_dir(&recovery_path).unwrap();
    std::fs::set_permissions(&recovery_path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let directory = std::fs::File::open(&recovery_path).unwrap();
    directory.sync_all().unwrap();
    let metadata = directory.metadata().unwrap();
    let parent =
        RecoveryParent::pin(&directory, (metadata.dev(), metadata.ino()), &recovery_path).unwrap();
    let source_identity = SourceApplyIdentity {
        device: DecimalU64::new(snapshot.source_identity.device),
        inode: DecimalU64::new(snapshot.source_identity.inode),
    };
    let recovery_identity = SourceApplyIdentity {
        device: DecimalU64::new(metadata.dev()),
        inode: DecimalU64::new(metadata.ino()),
    };
    let version = |v: &isolated_workspace::ManifestEntry| SourceApplyVersion {
        hash: v
            .sha256
            .unwrap()
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect(),
        mode: v.mode,
    };
    let payload = SourceApplyPayload {
        source_path: snapshot.source_path.to_str().unwrap().into(),
        source_identity,
        recovery_parent_path: recovery_path.to_str().unwrap().into(),
        recovery_parent_identity: recovery_identity,
        entries: changes
            .changes
            .iter()
            .map(|c| SourceApplyEntry {
                relative_path: c.relative_path.to_str().unwrap().into(),
                before: c.before.as_ref().map(&version),
                after: c.after.as_ref().map(&version),
            })
            .collect(),
    };
    let payload_hash = payload.payload_hash().unwrap();
    let approval = ApprovalId::new("apply-approval").unwrap();
    let operation = OperationId::new("apply-operation").unwrap();
    let guard = |store: &Writer| {
        let saved = store.snapshot().unwrap();
        SourceApplyGuard {
            expected_session_revision: saved.marker.session_revision,
            expected_policy_revision: saved.state.policy_revision,
            now_ms: 10,
        }
    };
    let proof = SourceApplyIdentityProof {
        source: source_identity,
        recovery_parent: recovery_identity,
    };
    engine
        .store
        .publish_source_apply(
            receipt.target(),
            SourceApplyCandidate {
                run_id: receipt.target().run_id.clone(),
                attempt_id: receipt.target().attempt_id.clone(),
                approval_id: approval.clone(),
                operation_id: operation.clone(),
                policy_revision: before.state.policy_revision,
                expires_at_unix_ms: DecimalU64::new(100),
                payload,
                payload_hash: payload_hash.clone(),
            },
            guard(&engine.store),
            proof,
        )
        .unwrap();
    engine
        .store
        .resolve_source_apply(
            receipt.target(),
            &approval,
            ApprovalDecision::Allow,
            guard(&engine.store),
        )
        .unwrap();
    parent.validate_identity().unwrap();
    assert_eq!(
        engine
            .store
            .consume_source_apply_intent(
                receipt.target(),
                &approval,
                &payload_hash,
                guard(&engine.store),
                proof
            )
            .unwrap(),
        IntentReceipt::NewlyPublished
    );
    let report = protected_apply_pinned(snapshot, &changes, limits, &parent);
    assert!(report.failure.is_none(), "{report:?}");
    assert_eq!(
        std::fs::read_to_string(snapshot.source_path.join("ordinary")).unwrap(),
        "edited fixture"
    );
    assert!(report.recovery_directory().is_some());
    let revision = engine.store.snapshot().unwrap().marker.session_revision;
    engine
        .store
        .record_source_apply_result(
            receipt.target(),
            &operation,
            revision,
            SourceApplyResult {
                result_id: ResultId::new("apply-result").unwrap(),
                report: serde_json::to_value(&report).unwrap(),
            },
        )
        .unwrap();
    let saved = engine.store.snapshot().unwrap();
    assert_eq!(saved.state.runs, before.state.runs);
    assert_eq!(
        serde_json::to_value(&saved.raw).unwrap(),
        serde_json::to_value(&before.raw).unwrap()
    );
    assert_eq!(
        engine
            .store
            .consume_source_apply_intent(
                receipt.target(),
                &approval,
                &payload_hash,
                guard(&engine.store),
                proof
            )
            .unwrap(),
        IntentReceipt::AlreadyRecorded
    );
    drop(f.service);
    let reopened = DesktopService::open(f.config).unwrap();
    assert_eq!(
        reopened
            .engine
            .as_ref()
            .unwrap()
            .store
            .snapshot()
            .unwrap()
            .state
            .source_applies,
        saved.state.source_applies
    );
}

#[test]
fn real_engine_completion_drives_source_controller_without_replaying_run() {
    use crate::{
        CurrentSourcePolicy, PreparedSourceRecovery, SourceApplyClock, SourceApplyController,
        SourceApplyRequest, SourceApplyState,
    };
    use polaris_core::{
        desktop_store::SourceApplyIdentity, isolated_workspace::Limits,
        workspace_apply::RecoveryParent,
    };
    use polaris_desktop_protocol::snapshot::ApprovalDecision;
    use std::os::unix::fs::MetadataExt;
    struct Clock;
    impl SourceApplyClock for Clock {
        fn now_ms(&self) -> u64 {
            10
        }
    }

    let mut f = fixture(Behavior::Complete, false);
    let engine = f.service.engine.as_mut().unwrap();
    start(engine);
    until(engine, |e| e.active.is_none());
    let receipt = f.completions.lock().unwrap().pop().unwrap();
    let snapshot = receipt.prepared().snapshot();
    let source_path = snapshot.source_path.clone();
    std::fs::write(snapshot.path().join("ordinary"), "controller edit").unwrap();
    let before = engine.store.snapshot().unwrap();
    let policy = CurrentSourcePolicy {
        source_path: source_path.clone(),
        source_identity: SourceApplyIdentity {
            device: DecimalU64::new(snapshot.source_identity.device),
            inode: DecimalU64::new(snapshot.source_identity.inode),
        },
        policy_revision: before.state.policy_revision,
        read_allowed: true,
        write_allowed: true,
    };
    let recovery_path = f
        ._temp
        .path()
        .canonicalize()
        .unwrap()
        .join("controller-recovery");
    std::fs::create_dir(&recovery_path).unwrap();
    std::fs::set_permissions(&recovery_path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let directory = std::fs::File::open(&recovery_path).unwrap();
    directory.sync_all().unwrap();
    std::fs::File::open(recovery_path.parent().unwrap())
        .unwrap()
        .sync_all()
        .unwrap();
    let metadata = directory.metadata().unwrap();
    let parent =
        RecoveryParent::pin(&directory, (metadata.dev(), metadata.ino()), &recovery_path).unwrap();
    let mut controller = SourceApplyController::new(
        receipt,
        policy,
        Arc::new(Clock),
        Limits {
            max_entries: 16,
            max_files: 8,
            max_file_bytes: 1024,
            max_total_bytes: 4096,
            max_depth: 4,
        },
    );
    let candidate = controller
        .prepare(
            &mut engine.store,
            SourceApplyRequest {
                approval_id: ApprovalId::new("controller-approval").unwrap(),
                operation_id: OperationId::new("controller-operation").unwrap(),
                result_id: ResultId::new("controller-result").unwrap(),
                expected_session_revision: before.marker.session_revision,
                expires_at_unix_ms: DecimalU64::new(100),
            },
            PreparedSourceRecovery {
                parent,
                path: recovery_path,
            },
        )
        .unwrap()
        .clone();
    assert!(controller.apply().is_err());
    assert_eq!(
        std::fs::read_to_string(source_path.join("ordinary")).unwrap(),
        "fixture"
    );
    let revision = engine.store.snapshot().unwrap().marker.session_revision;
    controller
        .resolve(&mut engine.store, ApprovalDecision::Allow, revision)
        .unwrap();
    let revision = engine.store.snapshot().unwrap().marker.session_revision;
    assert_eq!(
        controller
            .commit_intent(&mut engine.store, revision, &candidate.payload_hash)
            .unwrap(),
        IntentReceipt::NewlyPublished
    );
    assert!(controller.apply().unwrap().failure.is_none());
    assert!(controller.apply().is_err());
    let revision = engine.store.snapshot().unwrap().marker.session_revision;
    let displaced = f.config.store_root.with_extension("controller-displaced");
    std::fs::rename(&f.config.store_root, &displaced).unwrap();
    assert!(controller.save_result(&mut engine.store, revision).is_err());
    assert_eq!(controller.state(), SourceApplyState::RecoveryRequired);
    assert!(controller.report().unwrap().recovery_directory().is_some());
    assert!(controller.apply().is_err());
    std::fs::rename(displaced, &f.config.store_root).unwrap();
    controller.retry_result_save(&mut engine.store).unwrap();
    assert_eq!(controller.state(), SourceApplyState::Saved);
    assert!(controller.report().unwrap().recovery_directory().is_some());
    assert_eq!(
        std::fs::read_to_string(source_path.join("ordinary")).unwrap(),
        "controller edit"
    );
    let saved = engine.store.snapshot().unwrap();
    assert_eq!(saved.state.runs, before.state.runs);
    assert_eq!(
        serde_json::to_value(&saved.raw).unwrap(),
        serde_json::to_value(&before.raw).unwrap()
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}
#[test]
fn real_engine_queued_cancel_starts_no_factory_or_provider() {
    let mut f = fixture(Behavior::Complete, false);
    let engine = f.service.engine.as_mut().unwrap();
    start(engine);
    let target = engine.active.as_ref().unwrap().target.clone();
    engine
        .handle(
            &request("cancel", RequestBody::RunCancel(session_id(), target)),
            &mut vec![],
        )
        .unwrap();
    until(engine, |e| e.active.is_none());
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.preparations.load(Ordering::SeqCst), 0);
    assert!(f.completions.lock().unwrap().is_empty());
    assert_eq!(
        engine.store.snapshot().unwrap().state.runs[0].run.state,
        RunState::Cancelled
    );
}
#[test]
fn real_engine_cancel_joins_pending_provider_and_records_missing_usage() {
    let mut f = fixture(Behavior::Pending, false);
    let engine = f.service.engine.as_mut().unwrap();
    start(engine);
    until(engine, |_| f.calls.load(Ordering::SeqCst) == 1);
    let target = engine.active.as_ref().unwrap().target.clone();
    engine
        .handle(
            &request("cancel", RequestBody::RunCancel(session_id(), target)),
            &mut vec![],
        )
        .unwrap();
    assert!(engine.active.is_some(), "ACK is not a join");
    until(engine, |e| e.active.is_none());
    let saved = engine.store.snapshot().unwrap();
    assert_eq!(saved.state.runs[0].run.state, RunState::Cancelled);
    assert!(saved.state.runs[0].usage.is_some());
}
#[test]
fn real_engine_approval_wait_does_not_starve_core_join() {
    let mut f = fixture(Behavior::Approval, false);
    let engine = f.service.engine.as_mut().unwrap();
    start(engine);
    until(engine, |e| {
        e.active.as_ref().unwrap().execution.awaiting_approval()
    });
    let target = engine.active.as_ref().unwrap().target.clone();
    engine
        .handle(
            &request("cancel", RequestBody::RunCancel(session_id(), target)),
            &mut vec![],
        )
        .unwrap();
    until(engine, |e| e.active.is_none());
    let saved = engine.store.snapshot().unwrap();
    assert_eq!(saved.state.runs[0].run.state, RunState::Cancelled);
    assert!(saved.state.unresolved_approvals.is_empty());
    assert_eq!(saved.raw[1].message.tool_calls[0].id, "fixture-call");
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
    engine
        .handle(
            &request(
                "next-draft",
                RequestBody::DraftUpdate(
                    session_id(),
                    DraftUpdate {
                        expected_draft_revision: saved.state.draft.draft_revision,
                        text: "continue explicitly".into(),
                        attachment_ids: vec![],
                    },
                ),
            ),
            &mut vec![],
        )
        .unwrap();
    let next = engine.store.snapshot().unwrap();
    engine
        .handle(
            &request(
                "next-run",
                RequestBody::RunStart(
                    session_id(),
                    RunStart {
                        expected_draft_revision: next.state.draft.draft_revision,
                        expected_configuration_revision: next
                            .state
                            .configuration
                            .configuration_revision,
                        expected_policy_revision: next.state.policy_revision,
                    },
                ),
            ),
            &mut vec![],
        )
        .unwrap();
    until(engine, |e| e.active.is_none());
    let resumed = engine.store.snapshot().unwrap();
    assert_eq!(resumed.state.runs[1].run.state, RunState::Succeeded);
    assert_eq!(resumed.state.runs[0], saved.state.runs[0]);
    assert_eq!(
        serde_json::to_vec(&resumed.raw[..saved.raw.len()]).unwrap(),
        serde_json::to_vec(&saved.raw).unwrap()
    );
    assert_eq!(resumed.raw.len(), saved.raw.len() + 2);
    assert_eq!(
        resumed.raw[saved.raw.len()].message.content,
        "continue explicitly"
    );
    assert_eq!(
        resumed.raw.last().unwrap().message.content,
        "continued without replay"
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 2);
    assert!(resumed.state.unresolved_approvals.is_empty());
}
#[test]
fn real_engine_start_failure_and_provider_failure_never_succeed() {
    for (behavior, invalid) in [
        (Behavior::Complete, true),
        (Behavior::Error, false),
        (Behavior::Panic, false),
    ] {
        let mut f = fixture(behavior, invalid);
        let engine = f.service.engine.as_mut().unwrap();
        start(engine);
        until(engine, |e| e.active.is_none());
        let saved = engine.store.snapshot().unwrap();
        assert_eq!(saved.state.runs[0].run.state, RunState::Failed);
        assert_eq!(f.preparations.load(Ordering::SeqCst), 1);
        assert_eq!(f.calls.load(Ordering::SeqCst), usize::from(!invalid));
        assert_eq!(saved.raw.len(), 1);
    }
}

#[test]
fn real_engine_preserves_observed_success_after_cancel() {
    let mut f = fixture(Behavior::Held, false);
    let engine = f.service.engine.as_mut().unwrap();
    start(engine);
    until(engine, |_| f.calls.load(Ordering::SeqCst) == 1);
    f.release.store(true, Ordering::Release);
    // Observe join without publishing terminal, creating the precise cancel race.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while engine
        .active
        .as_ref()
        .unwrap()
        .real
        .as_ref()
        .unwrap()
        .outcome
        .is_none()
    {
        let active = engine.active.as_mut().unwrap();
        active
            .real
            .as_mut()
            .unwrap()
            .poll(&active.target, Some(&mut engine.store))
            .unwrap();
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(5));
    }
    let target = engine.active.as_ref().unwrap().target.clone();
    engine
        .handle(
            &request("cancel", RequestBody::RunCancel(session_id(), target)),
            &mut vec![],
        )
        .unwrap();
    until(engine, |e| e.active.is_none());
    assert_eq!(
        engine.store.snapshot().unwrap().state.runs[0].run.state,
        RunState::Succeeded
    );
}

#[test]
fn real_engine_terminal_record_does_not_discard_unjoined_owner() {
    let mut f = fixture(Behavior::Pending, false);
    let engine = f.service.engine.as_mut().unwrap();
    let mut next = start(engine);
    until(engine, |_| f.calls.load(Ordering::SeqCst) == 1);
    let active = engine.active.as_ref().unwrap();
    engine
        .store
        .finish_with_messages(
            &active.target,
            &active.operation,
            Observation::Cancelled,
            ResultId::new(format!("result-{}", active.target.run_id.as_str())).unwrap(),
            vec![],
        )
        .unwrap();
    next.request_id = RequestId::new("another-run").unwrap();
    assert_eq!(
        engine.handle(&next, &mut vec![]).unwrap_err().code,
        ErrorCode::SessionBusy
    );
    assert!(
        engine
            .active
            .as_ref()
            .unwrap()
            .real
            .as_ref()
            .unwrap()
            .worker
            .is_some()
    );
    until(engine, |e| e.active.is_none());
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn storage_only_open_never_advertises_or_accepts_real_execution() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::set_permissions(temp.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut service = DesktopService::open(ServiceConfig {
        store_root: temp.path().canonicalize().unwrap(),
        project_id: project_id(),
        session_id: session_id(),
    })
    .unwrap();
    let engine = service.engine.as_mut().unwrap();
    let SuccessResult::Hello(hello) = engine
        .handle(&request("hello", RequestBody::Hello), &mut vec![])
        .unwrap()
    else {
        panic!()
    };
    assert!(!hello.capabilities.contains(&Capability::RunStart));
    let denied = engine
        .handle(
            &request(
                "start",
                RequestBody::RunStart(
                    session_id(),
                    RunStart {
                        expected_draft_revision: DecimalU64::new(0),
                        expected_configuration_revision: DecimalU64::new(0),
                        expected_policy_revision: DecimalU64::new(0),
                    },
                ),
            ),
            &mut vec![],
        )
        .unwrap_err();
    assert_eq!(denied.code, ErrorCode::CapabilityUnavailable);
    assert!(engine.store.snapshot().unwrap().state.runs.is_empty());
}

#[test]
fn real_engine_restores_and_atomically_saves_workflow_without_artifact_claim() {
    use polaris_core::workflow::{SessionWorkflow, VerificationEvidence, WorkflowConfig};
    let mut f = fixture(Behavior::Complete, false);
    let engine = f.service.engine.as_mut().unwrap();
    let mut workflow = SessionWorkflow::new(WorkflowConfig {
        enabled: true,
        always: vec!["builtin:workflow-core".into()],
        ..Default::default()
    });
    workflow.gates.verification = Some(VerificationEvidence {
        artifact_hash: "previous-artifact".into(),
        passed: true,
        stale: false,
    });
    let saved = SavedWorkflow::capture(&workflow);
    let revision = engine.store.snapshot().unwrap().marker.session_revision;
    engine
        .store
        .configure_workflow(revision, Some(saved))
        .unwrap();
    start(engine);
    until(engine, |e| e.active.is_none());
    let published = engine.store.snapshot().unwrap();
    let workflow = published.state.runs[0].workflow.as_ref().unwrap();
    assert!(workflow.gates.verification.as_ref().unwrap().stale);
    assert!(!workflow.state.skill_manifest_hash.is_empty());
    assert_eq!(published.state.workflow.as_ref(), Some(workflow));
    assert_eq!(
        published.raw.last().unwrap().message.content,
        "raw completion"
    );
    assert_eq!(published.state.runs[0].run.state, RunState::Succeeded);
}

#[test]
fn real_engine_rejected_output_persists_typed_gap_before_failed_terminal() {
    let mut f = fixture(Behavior::Rejected, false);
    let engine = f.service.engine.as_mut().unwrap();
    start(engine);
    until(engine, |e| e.active.is_none());
    let saved = engine.store.snapshot().unwrap();
    assert_eq!(
        saved.state.runs[0].history_gap,
        Some(HistoryGap::OutputRejected)
    );
    assert_eq!(saved.state.runs[0].run.state, RunState::Failed);
    assert_eq!(saved.raw.len(), 1);
    assert!(!history_ready(&saved));
    assert_eq!(saved.state.runs[0].usage.unwrap().reported_responses, 1);
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn transport_drop_retains_delayed_core_and_raw_past_old_deadline() {
    let mut f = fixture(Behavior::Blocking, false);
    let engine = f.service.engine.as_mut().unwrap();
    start(engine);
    engine.step_real(None, false).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    while f.calls.load(Ordering::SeqCst) == 0 {
        engine.step_real(None, false).unwrap();
        assert!(
            std::time::Instant::now() < deadline,
            "provider did not start"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    // Release even if the test fails, so the fixture cannot strand its worker.
    struct Release(Arc<AtomicBool>);
    impl Drop for Release {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Release);
        }
    }
    let release = Release(f.release.clone());
    let disconnected = engine.disconnected.clone();
    let (_client, reader) = tokio::io::duplex(1024);
    let mut serving = Box::pin(f.service.serve(reader, tokio::io::sink()));
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut serving)
            .await
            .is_err()
    );
    drop(serving);
    assert!(disconnected.load(Ordering::Acquire));
    assert!(
        f.service.engine.is_none(),
        "transport did not take ownership"
    );

    tokio::time::sleep(Duration::from_millis(3400)).await;
    // On the old path, dropping the unobserved blocking-task result released
    // Writer at three seconds even while the core thread was still running.
    let retained = matches!(
        DesktopService::open(f.config.clone()),
        Err(ServiceError::Store(StoreError::Busy))
    );
    drop(release);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    let reopened = loop {
        match DesktopService::open(f.config.clone()) {
            Ok(service) => break service,
            Err(ServiceError::Store(StoreError::Busy)) => {
                assert!(
                    std::time::Instant::now() < deadline,
                    "owner did not finalize after release"
                );
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            Err(error) => panic!("unexpected fixture reopen: {error}"),
        }
    };
    let saved = reopened.engine.as_ref().unwrap().store.snapshot().unwrap();
    assert!(
        retained,
        "production owner was dropped at the old transport deadline"
    );
    assert_eq!(saved.raw.len(), 2);
    assert_eq!(saved.raw[1].message.content, "raw completion");
    assert_eq!(saved.state.runs[0].run.state, RunState::Succeeded);
    assert_eq!(saved.state.runs[0].usage.unwrap().usage.total_tokens, 10);
    assert_eq!(f.calls.load(Ordering::SeqCst), 1);
}

#[test]
fn real_child_spawn_saved_before_update_and_snapshot_count_is_distinct() {
    let mut f = fixture(Behavior::Spawn, false);
    let engine = f.service.engine.as_mut().unwrap();
    start(engine);
    until(engine, |e| e.active.is_none());
    let saved = engine.store.snapshot().unwrap();
    assert_eq!(saved.state.children.len(), 1);
    let child = &saved.state.children[0];
    assert_eq!(child.child.state, RunState::Succeeded);
    assert_eq!(child.agent_type, "child-fixture");
    assert_eq!(child.task, "child fixture");
    assert_eq!(child.child.task_ids, saved.state.runs[0].run.task_ids);
    assert!(saved.state.tasks.is_empty());
    assert_eq!(saved.state.runs[0].run.state, RunState::Succeeded);
    let envelope = || EventEnvelope {
        run_id: child.root.run_id.clone(),
        sequence: 1,
        child: Some(polaris_core::desktop_events::ChildIdentity {
            run_id: child.child.run_id.clone(),
            attempt_id: child.child.attempt_id.clone(),
            parent_run_id: child.child.parent_run_id.clone(),
        }),
        event: AgentEvent::SpawnStarted {
            agent_type: child.agent_type.clone(),
            task: child.task.clone(),
        },
    };
    assert!(
        persist_child_event(&mut engine.store, &child.root, envelope())
            .unwrap()
            .is_none()
    );
    for _ in 0..3 {
        let snapshot = engine.snapshot(false).unwrap();
        assert_eq!(snapshot.child_attempt_count.get(), 1);
        assert_eq!(snapshot.children, vec![child.child.clone()]);
    }
    assert_eq!(engine.store.snapshot().unwrap().marker, saved.marker);
    assert_eq!(f.calls.load(Ordering::SeqCst), 3);
}

#[test]
fn real_child_cancel_seals_incomplete_after_join() {
    let mut f = fixture(Behavior::SpawnPending, false);
    let engine = f.service.engine.as_mut().unwrap();
    start(engine);
    until(engine, |e| {
        !e.store.snapshot().unwrap().state.children.is_empty()
    });
    let before = engine.store.snapshot().unwrap();
    assert_eq!(before.state.children[0].child.state, RunState::Running);
    let target = engine.active.as_ref().unwrap().target.clone();
    engine
        .handle(
            &request("child-cancel", RequestBody::RunCancel(session_id(), target)),
            &mut vec![],
        )
        .unwrap();
    until(engine, |e| e.active.is_none());
    let saved = engine.store.snapshot().unwrap();
    assert_eq!(
        saved.state.children[0].child.state,
        RunState::OutcomeUnknown
    );
    assert_eq!(saved.state.runs[0].run.state, RunState::Cancelled);
    assert_eq!(saved.state.tasks, before.state.tasks);
    assert_eq!(engine.snapshot(false).unwrap().child_attempt_count.get(), 1);
}

#[test]
fn real_child_final_drain_waits_for_disconnection_after_join() {
    let mut f = fixture(Behavior::Held, false);
    let engine = f.service.engine.as_mut().unwrap();
    start(engine);
    until(engine, |_| f.calls.load(Ordering::SeqCst) == 1);
    f.release.store(true, Ordering::Release);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let active = engine.active.as_mut().unwrap();
        let real = active.real.as_mut().unwrap();
        real.poll(&active.target, Some(&mut engine.store)).unwrap();
        if real.outcome.is_some() {
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    // Reproduce a full last batch arriving at the join boundary. Even an empty
    // queue is insufficient while a producer remains: only disconnect closes it.
    let active = engine.active.as_mut().unwrap();
    let (sink, events, _control) =
        polaris_core::desktop_events::EventSink::desktop(active.target.run_id.clone());
    let real = active.real.as_mut().unwrap();
    assert!(real.worker.is_none());
    real.events = Some(events);
    real.sequence = 0;
    for _ in 0..EVENT_CAPACITY {
        sink.send(AgentEvent::ToolStarted {
            name: "fixture".into(),
            detail: "hint".into(),
        })
        .unwrap();
    }
    engine.step_real(None, false).unwrap();
    assert!(engine.active.is_some());
    assert!(
        !engine.store.snapshot().unwrap().state.runs[0]
            .run
            .state
            .is_terminal()
    );
    engine.step_real(None, false).unwrap();
    assert!(engine.active.is_some(), "empty does not mean disconnected");
    drop(sink);
    until(engine, |e| e.active.is_none());
    let saved = engine.store.snapshot().unwrap();
    assert_eq!(saved.state.runs[0].run.state, RunState::Succeeded);
    assert_eq!(saved.raw[1].message.content, "raw completion");
}

#[test]
fn real_child_update_is_returned_only_after_persistence_and_rejects_reversal() {
    let mut f = fixture(Behavior::Held, false);
    let engine = f.service.engine.as_mut().unwrap();
    start(engine);
    engine.step_real(None, false).unwrap();
    let target = engine.active.as_ref().unwrap().target.clone();
    let identity = polaris_core::desktop_events::ChildIdentity {
        run_id: RunId::new("child-event-test").unwrap(),
        attempt_id: AttemptId::new("child-attempt-test").unwrap(),
        parent_run_id: target.run_id.clone(),
    };
    let event = |event| EventEnvelope {
        run_id: target.run_id.clone(),
        sequence: 1,
        child: Some(identity.clone()),
        event,
    };
    let update = persist_child_event(
        &mut engine.store,
        &target,
        event(AgentEvent::SpawnStarted {
            agent_type: "worker".into(),
            task: "purpose".into(),
        }),
    )
    .unwrap();
    let Some(EventBody::ChildUpdated(child)) = update else {
        panic!("missing saved update");
    };
    assert_eq!(
        engine.store.snapshot().unwrap().state.children[0].child,
        child
    );
    let update = persist_child_event(
        &mut engine.store,
        &target,
        event(AgentEvent::SpawnFinished {
            agent_type: "worker".into(),
            ok: true,
        }),
    )
    .unwrap();
    let Some(EventBody::ChildUpdated(child)) = update else {
        panic!("missing terminal update");
    };
    assert_eq!(
        engine.store.snapshot().unwrap().state.children[0].child,
        child
    );
    assert!(
        persist_child_event(
            &mut engine.store,
            &target,
            event(AgentEvent::SpawnFinished {
                agent_type: "worker".into(),
                ok: false
            })
        )
        .is_err()
    );
    f.release.store(true, Ordering::Release);
    until(engine, |e| e.active.is_none());
}

struct SourceClock;
impl crate::SourceApplyClock for SourceClock {
    fn now_ms(&self) -> u64 {
        10
    }
}
struct SourceFactory {
    recovery_root: PathBuf,
    calls: Arc<AtomicUsize>,
    edit: bool,
}
impl TrustedSourceFactory for SourceFactory {
    fn prepare(
        &self,
        completion: &TrustedRunCompletion,
        saved: &Published,
        request: &crate::SourceApplyRequest,
    ) -> Result<(crate::CurrentSourcePolicy, crate::PreparedSourceRecovery), ServiceError> {
        use polaris_core::{desktop_store::SourceApplyIdentity, workspace_apply::RecoveryParent};
        use std::os::unix::fs::MetadataExt;
        self.calls.fetch_add(1, Ordering::SeqCst);
        let snapshot = completion.prepared().snapshot();
        // Synthetic fixture edits only; no live provider/source or binary setup.
        if self.edit {
            std::fs::write(snapshot.path().join("ordinary"), "source-owner changed")?;
        }
        let path = self.recovery_root.join(request.operation_id.as_str());
        std::fs::create_dir(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700))?;
        let dir = std::fs::File::open(&path)?;
        dir.sync_all()?;
        std::fs::File::open(&self.recovery_root)?.sync_all()?;
        let meta = dir.metadata()?;
        let parent = RecoveryParent::pin(&dir, (meta.dev(), meta.ino()), &path).unwrap();
        Ok((
            crate::CurrentSourcePolicy {
                source_path: snapshot.source_path.clone(),
                source_identity: SourceApplyIdentity {
                    device: DecimalU64::new(snapshot.source_identity.device),
                    inode: DecimalU64::new(snapshot.source_identity.inode),
                },
                policy_revision: saved.state.policy_revision,
                read_allowed: true,
                write_allowed: true,
            },
            crate::PreparedSourceRecovery { parent, path },
        ))
    }
}
fn enable_source(f: &mut Fixture, edit: bool) -> Arc<AtomicUsize> {
    let calls = Arc::new(AtomicUsize::new(0));
    let runtime = SourceApplyRuntime {
        factory: Arc::new(SourceFactory {
            recovery_root: f._temp.path().canonicalize().unwrap(),
            calls: calls.clone(),
            edit,
        }),
        clock: Arc::new(SourceClock),
        limits: polaris_core::isolated_workspace::Limits {
            max_entries: 16,
            max_files: 8,
            max_file_bytes: 1024,
            max_total_bytes: 4096,
            max_depth: 4,
        },
        approval_ttl_ms: 1000,
    };
    // Configure via the production open API, keeping the existing run factory.
    let engine = f.service.engine.as_ref().unwrap();
    let factory = engine.trusted_runs.as_ref().unwrap().factory.clone();
    let configuration = engine.store.snapshot().unwrap().state.configuration;
    let old = f.service.engine.take();
    drop(old);
    f.service = DesktopService::open_with_trusted_source_apply(
        f.config.clone(),
        configuration,
        factory,
        runtime,
    )
    .unwrap();
    calls
}
fn source_until(engine: &mut Engine, predicate: impl Fn(&Engine) -> bool) {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        engine.poll_source(false).unwrap();
        if predicate(engine) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "source owner deadline"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn source_answer(engine: &Engine, id: &str) -> Request {
    let saved = engine.store.snapshot().unwrap();
    let c = &saved.state.source_applies[0].candidate;
    request(
        id,
        RequestBody::SourceApplyResolve(
            session_id(),
            polaris_desktop_protocol::request::SourceApplyResolve {
                run_id: c.run_id.clone(),
                attempt_id: c.attempt_id.clone(),
                approval_id: c.approval_id.clone(),
                payload_hash: c.payload_hash.clone(),
                expected_session_revision: saved.marker.session_revision,
                expected_policy_revision: saved.state.policy_revision,
                decision: polaris_desktop_protocol::snapshot::ApprovalDecision::Allow,
            },
        ),
    )
}

#[test]
fn source_engine_receipt_once_and_saved_answer_replay_never_reapplies() {
    let mut f = fixture(Behavior::Complete, false);
    let calls = enable_source(&mut f, true);
    assert!(f.service.source_apply_available());
    let e = f.service.engine.as_mut().unwrap();
    start(e);
    until(e, |e| e.active.is_none());
    assert!(f.completions.lock().unwrap().is_empty());
    source_until(e, |e| {
        !e.store.snapshot().unwrap().state.source_applies.is_empty()
    });
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    let before = e.store.snapshot().unwrap();
    let answer = source_answer(e, "source-answer");
    let (entered, release) = crate::source_apply_io::pause_next_job();
    e.handle(&answer, &mut vec![]).unwrap();
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    // Snapshot requests and exact replay return while the apply job is paused.
    assert!(matches!(
        e.handle(
            &request("during-apply", RequestBody::SessionSnapshot(session_id())),
            &mut vec![]
        )
        .unwrap(),
        SuccessResult::SessionSnapshot(_)
    ));
    e.handle(&answer, &mut vec![]).unwrap();
    assert_eq!(e.source.as_ref().unwrap().owned_for_test(), (1, true, 0));
    assert_eq!(
        std::fs::read_to_string(f._temp.path().join("source/ordinary")).unwrap(),
        "fixture"
    );
    release.send(()).unwrap();
    source_until(e, |e| {
        e.store.snapshot().unwrap().state.source_applies[0]
            .result
            .is_some()
    });
    let saved = e.store.snapshot().unwrap();
    assert_eq!(saved.state.runs, before.state.runs);
    assert_eq!(saved.marker.raw_hash, before.marker.raw_hash);
    assert_eq!(saved.state.configuration, before.state.configuration);
    assert_eq!(e.source.as_ref().unwrap().owned_for_test(), (1, false, 1));
    std::fs::write(
        f._temp.path().join("source/ordinary"),
        "external later edit",
    )
    .unwrap();
    e.handle(&answer, &mut vec![]).unwrap();
    e.poll_source(false).unwrap();
    assert_eq!(
        std::fs::read_to_string(f._temp.path().join("source/ordinary")).unwrap(),
        "external later edit"
    );
    e.draining = true;
    e.settle_without_output().unwrap();
    assert!(e.ready);
}

#[test]
fn source_engine_collection_generation_cancel_eof_and_empty_never_publish() {
    for reason in ["revision", "cancel", "eof", "empty"] {
        let mut f = fixture(Behavior::Complete, false);
        enable_source(&mut f, reason != "empty");
        let e = f.service.engine.as_mut().unwrap();
        start(e);
        until(e, |e| e.active.is_none());
        let (entered, release) = crate::source_apply_io::pause_next_job();
        e.poll_source(false).unwrap();
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        match reason {
            "revision" => {
                let revision = e.store.snapshot().unwrap().state.draft.draft_revision;
                e.handle(
                    &request(
                        "late-draft",
                        RequestBody::DraftUpdate(
                            session_id(),
                            DraftUpdate {
                                expected_draft_revision: revision,
                                text: "late".into(),
                                attachment_ids: vec![],
                            },
                        ),
                    ),
                    &mut vec![],
                )
                .unwrap();
            }
            "cancel" => {
                let run = e.store.snapshot().unwrap().state.runs[0].run.clone();
                e.handle(
                    &request(
                        "source-cancel",
                        RequestBody::RunCancel(
                            session_id(),
                            RunTarget {
                                run_id: run.run_id,
                                attempt_id: run.attempt_id,
                            },
                        ),
                    ),
                    &mut vec![],
                )
                .unwrap();
            }
            "eof" => {
                e.disconnected.store(true, Ordering::Release);
            }
            _ => {}
        }
        release.send(()).unwrap();
        source_until(e, |e| !e.source_unsettled());
        assert!(
            e.store.snapshot().unwrap().state.source_applies.is_empty(),
            "{reason}"
        );
        assert_eq!(e.source.as_ref().unwrap().owned_for_test().0, 1);
    }
}

#[test]
fn source_engine_failed_storage_still_joins_and_retains_report_not_ready() {
    let mut f = fixture(Behavior::Complete, false);
    enable_source(&mut f, true);
    let e = f.service.engine.as_mut().unwrap();
    start(e);
    until(e, |e| e.active.is_none());
    source_until(e, |e| {
        !e.store.snapshot().unwrap().state.source_applies.is_empty()
    });
    let answer = source_answer(e, "source-answer");
    let (entered, release) = crate::source_apply_io::pause_next_job();
    e.handle(&answer, &mut vec![]).unwrap();
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    e.failed = true;
    e.disconnected.store(true, Ordering::Release);
    assert!(e.settle_without_output().is_err());
    assert!(!e.ready);
    release.send(()).unwrap();
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while e.source.as_ref().unwrap().owned_for_test().1 {
        assert!(e.settle_without_output().is_err());
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(e.source.as_ref().unwrap().owned_for_test(), (1, false, 1));
    assert!(e.source_unsettled());
    assert!(!e.ready);
    assert!(
        e.store.snapshot().unwrap().state.source_applies[0]
            .result
            .is_none()
    );
    // Deliberate test teardown only; this is not the production serve exit path.
}

#[test]
fn source_engine_reserves_receipts_before_start_and_fresh_answers_obey_request_bound() {
    let mut f = fixture(Behavior::Complete, false);
    enable_source(&mut f, true);
    let e = f.service.engine.as_mut().unwrap();
    start(e);
    until(e, |e| e.active.is_none());
    source_until(e, |e| {
        !e.store.snapshot().unwrap().state.source_applies.is_empty()
    });
    for i in 1..RUNS {
        e.source
            .as_mut()
            .unwrap()
            .reserve(RunTarget {
                run_id: RunId::new(format!("reserved-{i}")).unwrap(),
                attempt_id: AttemptId::new(format!("reserved-attempt-{i}")).unwrap(),
            })
            .unwrap();
    }
    let saved = e.store.snapshot().unwrap();
    let request_start = request(
        "capacity-run",
        RequestBody::RunStart(
            session_id(),
            RunStart {
                expected_draft_revision: saved.state.draft.draft_revision,
                expected_configuration_revision: saved.state.configuration.configuration_revision,
                expected_policy_revision: saved.state.policy_revision,
            },
        ),
    );
    assert_eq!(
        e.handle(&request_start, &mut vec![]).unwrap_err().code,
        ErrorCode::CapabilityUnavailable
    );
    assert_eq!(e.store.snapshot().unwrap().state.runs.len(), 1);
    while e.store.snapshot().unwrap().state.requests.len() < REQUEST_RECORDS - 1 {
        let saved = e.store.snapshot().unwrap();
        let id = format!("capacity-draft-{}", saved.marker.session_revision.get());
        e.handle(
            &request(
                &id,
                RequestBody::DraftUpdate(
                    session_id(),
                    DraftUpdate {
                        expected_draft_revision: saved.state.draft.draft_revision,
                        text: id.clone(),
                        attachment_ids: vec![],
                    },
                ),
            ),
            &mut vec![],
        )
        .unwrap();
    }
    let answer = source_answer(e, "capacity-answer");
    assert_eq!(
        e.handle(&answer, &mut vec![]).unwrap_err().code,
        ErrorCode::CapabilityUnavailable
    );
    assert!(
        e.store.snapshot().unwrap().state.source_applies[0]
            .decision
            .is_none()
    );
}

#[test]
fn source_engine_disconnect_after_saved_answer_prevents_intent_and_preserves_ack() {
    let mut f = fixture(Behavior::Complete, false);
    enable_source(&mut f, true);
    let e = f.service.engine.as_mut().unwrap();
    start(e);
    until(e, |e| e.active.is_none());
    source_until(e, |e| {
        !e.store.snapshot().unwrap().state.source_applies.is_empty()
    });
    let answer = source_answer(e, "answer-before-disconnect");
    e.source
        .as_mut()
        .unwrap()
        .disconnect_after_answer_for_test();
    let ack = e.handle(&answer, &mut vec![]).unwrap();
    assert!(e.disconnected.load(Ordering::Acquire));
    let saved = e.store.snapshot().unwrap();
    let entry = &saved.state.source_applies[0];
    assert_eq!(
        entry.decision,
        Some(polaris_desktop_protocol::snapshot::ApprovalDecision::Allow)
    );
    assert!(entry.invalidated);
    assert!(entry.intent_revision.is_none());
    assert!(entry.result.is_none());
    assert!(saved.state.requests.iter().any(|r| r.request_id == answer.request_id
        && matches!(&r.result, RequestResult::SourceApplyResolved { approval_id } if approval_id == &entry.candidate.approval_id)));
    assert_eq!(e.source.as_ref().unwrap().owned_for_test(), (1, false, 0));
    assert_eq!(
        std::fs::read_to_string(f._temp.path().join("source/ordinary")).unwrap(),
        "fixture"
    );
    // Only the same saved answer is acknowledged; reconnect never grants work.
    assert_eq!(e.handle(&answer, &mut vec![]).unwrap(), ack);
    e.settle_without_output().unwrap();
    assert!(e.ready);
    assert!(
        e.store.snapshot().unwrap().state.source_applies[0]
            .intent_revision
            .is_none()
    );
}

struct PausedPrepare {
    inner: Arc<dyn TrustedRunFactory>,
    entered: std::sync::mpsc::Sender<()>,
    release: Arc<AtomicBool>,
    panic: bool,
}
impl TrustedRunFactory for PausedPrepare {
    fn prepare(
        &self,
        target: &RunTarget,
        saved: &Published,
    ) -> Result<TrustedRunInputs, ServiceError> {
        let inputs = self.inner.prepare(target, saved)?;
        self.entered.send(()).unwrap();
        while !self.release.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(
            !self.panic,
            "injected prepare panic after partial preparation"
        );
        Ok(inputs)
    }
}
struct ReleasePrepare(Arc<AtomicBool>);
impl Drop for ReleasePrepare {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}
fn pause_prepare(
    engine: &mut Engine,
    panic: bool,
) -> (std::sync::mpsc::Receiver<()>, ReleasePrepare) {
    let (entered, receiver) = std::sync::mpsc::channel();
    let release = Arc::new(AtomicBool::new(false));
    let trusted = engine.trusted_runs.as_mut().unwrap();
    trusted.factory = Arc::new(PausedPrepare {
        inner: trusted.factory.clone(),
        entered,
        release: release.clone(),
        panic,
    });
    (receiver, ReleasePrepare(release))
}
#[test]
fn real_prepare_cancel_disconnect_policy_and_configuration_prevent_provider_start() {
    for gate in ["cancel", "disconnect", "policy", "configuration"] {
        let mut f = fixture(Behavior::Complete, false);
        let engine = f.service.engine.as_mut().unwrap();
        let (entered, release) = pause_prepare(engine, false);
        start(engine);
        engine.step_real(None, false).unwrap();
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(matches!(
            engine
                .active
                .as_ref()
                .unwrap()
                .real
                .as_ref()
                .unwrap()
                .preparation,
            Preparation::Running(_)
        ));
        // Request handling remains on the owner while prepare is paused.
        engine
            .handle(
                &request(
                    "prepare-snapshot",
                    RequestBody::SessionSnapshot(session_id()),
                ),
                &mut vec![],
            )
            .unwrap();
        match gate {
            "cancel" => engine.active.as_mut().unwrap().cancel(),
            "disconnect" => engine.disconnected.store(true, Ordering::Release),
            "policy" => {
                engine
                    .store
                    .set_policy_revision(DecimalU64::new(1))
                    .unwrap();
            }
            "configuration" => {
                engine
                    .store
                    .configure_role_bindings(DecimalU64::new(0), None, &[])
                    .unwrap();
            }
            _ => unreachable!(),
        }
        engine.step_real(None, false).unwrap();
        assert!(engine.active.is_some());
        assert_eq!(f.calls.load(Ordering::SeqCst), 0);
        drop(release);
        until(engine, |e| e.active.is_none());
        assert_eq!(f.calls.load(Ordering::SeqCst), 0);
        assert!(f.completions.lock().unwrap().is_empty());
        assert!(
            engine.store.snapshot().unwrap().state.runs[0]
                .run
                .state
                .is_terminal()
        );
    }
}
#[test]
fn real_prepare_failed_store_joins_and_holds_inputs_without_start_or_ready() {
    let mut f = fixture(Behavior::Complete, false);
    let engine = f.service.engine.as_mut().unwrap();
    let (entered, release) = pause_prepare(engine, false);
    start(engine);
    engine.step_real(None, false).unwrap();
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    let before = engine.store.snapshot().unwrap();
    engine.failed = true;
    assert!(engine.settle_without_output().is_err());
    assert!(!engine.ready);
    drop(release);
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        assert!(engine.settle_without_output().is_err());
        let real = engine.active.as_ref().unwrap().real.as_ref().unwrap();
        if matches!(real.preparation, Preparation::Joined) {
            assert!(real.prepared_inputs.is_some());
            assert!(!real.joined);
            assert!(real.worker.is_none());
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::sleep(Duration::from_millis(1));
    }
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
    assert_eq!(before.marker, engine.store.snapshot().unwrap().marker);
    assert!(!engine.ready);
}
#[test]
fn real_prepare_spawn_failure_and_panic_never_prove_core_join() {
    for panic in [false, true] {
        let mut f = fixture(Behavior::Complete, false);
        let engine = f.service.engine.as_mut().unwrap();
        let paused = panic.then(|| pause_prepare(engine, true));
        if !panic {
            FAIL_PREPARE_SPAWN.with(|flag| flag.set(true));
        }
        start(engine);
        engine.step_real(None, false).unwrap();
        if let Some((entered, release)) = paused {
            entered.recv_timeout(Duration::from_secs(5)).unwrap();
            drop(release);
        }
        until(engine, |e| e.active.is_none());
        assert_eq!(f.calls.load(Ordering::SeqCst), 0);
        assert!(f.completions.lock().unwrap().is_empty());
        let state = engine.store.snapshot().unwrap().state.runs[0].run.state;
        assert_ne!(state, RunState::Succeeded);
    }
}

#[test]
fn real_execution_capability_is_forwarded_and_invalid_scope_prevents_provider() {
    use polaris_core::desktop_execution::{
        ConfirmedExecutionCapability, SandboxMode, SandboxPolicy,
    };
    struct CapabilityFactory {
        inner: Arc<dyn TrustedRunFactory>,
        invalid: bool,
    }
    impl TrustedRunFactory for CapabilityFactory {
        fn prepare(
            &self,
            target: &RunTarget,
            saved: &Published,
        ) -> Result<TrustedRunInputs, ServiceError> {
            let mut inputs = self.inner.prepare(target, saved)?;
            inputs.execution_capability = Some(if self.invalid {
                // A legacy policy lacks the prepared isolated boundary. If the
                // engine discards this field, the provider would wrongly run.
                ConfirmedExecutionCapability::ConfinedCode {
                    scope: SandboxPolicy::new(
                        SandboxMode::WorkspaceWrite,
                        &[inputs.prepared.snapshot().path().to_owned()],
                    )
                    .unwrap(),
                }
            } else {
                ConfirmedExecutionCapability::FixedHelpersOnly
            });
            Ok(inputs)
        }
    }
    for invalid in [false, true] {
        let mut f = fixture(Behavior::Complete, false);
        let engine = f.service.engine.as_mut().unwrap();
        let trusted = engine.trusted_runs.as_mut().unwrap();
        trusted.factory = Arc::new(CapabilityFactory {
            inner: trusted.factory.clone(),
            invalid,
        });
        start(engine);
        until(engine, |e| e.active.is_none());
        assert_eq!(f.calls.load(Ordering::SeqCst), usize::from(!invalid));
        assert_eq!(
            engine.store.snapshot().unwrap().state.runs[0].run.state,
            if invalid {
                RunState::Failed
            } else {
                RunState::Succeeded
            }
        );
    }
}

fn constructor_source_runtime(
    path: PathBuf,
    calls: Arc<AtomicUsize>,
    ttl: u64,
) -> SourceApplyRuntime {
    SourceApplyRuntime {
        factory: Arc::new(SourceFactory {
            recovery_root: path,
            calls,
            edit: false,
        }),
        clock: Arc::new(SourceClock),
        limits: polaris_core::isolated_workspace::Limits {
            max_entries: 16,
            max_files: 8,
            max_file_bytes: 1024,
            max_total_bytes: 4096,
            max_depth: 4,
        },
        approval_ttl_ms: ttl,
    }
}
#[test]
fn trusted_store_constructor_preserves_existing_writer_and_production_capabilities() {
    let f = fixture(Behavior::Complete, false);
    let factory = f
        .service
        .engine
        .as_ref()
        .unwrap()
        .trusted_runs
        .as_ref()
        .unwrap()
        .factory
        .clone();
    let path = f._temp.path().join("existing-store");
    std::fs::create_dir(&path).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).unwrap();
    let root = DesktopRoot::open_owned(&path.canonicalize().unwrap()).unwrap();
    let project = ProjectId::new("existing-project").unwrap();
    let session = SessionId::new("existing-session").unwrap();
    let store = root
        .create(
            project.clone(),
            session.clone(),
            InitialState {
                draft: polaris_desktop_protocol::snapshot::Draft {
                    draft_revision: DecimalU64::new(9),
                    text: "saved draft".into(),
                    attachment_ids: vec![],
                },
                configuration: Configuration {
                    history_mode: Default::default(),
                    configuration_revision: DecimalU64::new(7),
                    provider: "saved-provider".into(),
                    model: "saved-model".into(),
                    effort: "none".into(),
                },
                policy_revision: DecimalU64::new(5),
            },
        )
        .unwrap();
    let before = store.snapshot().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let mut service = DesktopService::from_trusted_store(
        store,
        factory,
        constructor_source_runtime(path, calls.clone(), 100),
    )
    .unwrap();
    assert!(service.attach_source_recovery().is_ok());
    let engine = service.engine.as_mut().unwrap();
    assert_eq!(engine.session, session);
    assert_eq!(engine.store.coordinates(), (&project, &session));
    let after = engine.store.snapshot().unwrap();
    assert_eq!(before.marker, after.marker);
    assert_eq!(before.state, after.state);
    assert_eq!(
        engine.trusted_runs.as_ref().unwrap().configuration,
        before.state.configuration
    );
    assert!(engine.epoch.as_str().starts_with("desktop-"));
    let SuccessResult::Hello(hello) = engine
        .handle(&request("hello-existing", RequestBody::Hello), &mut vec![])
        .unwrap()
    else {
        panic!()
    };
    assert!(hello.capabilities.contains(&Capability::RunStart));
    assert!(hello.capabilities.contains(&Capability::SourceApplyResolve));
    assert_eq!(calls.load(Ordering::SeqCst), 0);
    assert_eq!(f.preparations.load(Ordering::SeqCst), 0);
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
}
#[test]
fn trusted_store_constructor_errors_never_recover_or_remove_existing_artifacts() {
    for failed_store in [false, true] {
        let mut f = fixture(Behavior::Complete, false);
        let engine = f.service.engine.take().unwrap();
        let factory = engine.trusted_runs.as_ref().unwrap().factory.clone();
        let mut store = engine.store;
        let before = store.snapshot().unwrap();
        if failed_store {
            let displaced = f.config.store_root.with_extension("constructor-displaced");
            std::fs::rename(&f.config.store_root, &displaced).unwrap();
            assert!(store.recover().is_err());
            std::fs::rename(displaced, &f.config.store_root).unwrap();
            assert!(store.snapshot().is_err());
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let result = DesktopService::from_trusted_store(
            store,
            factory,
            constructor_source_runtime(
                f._temp.path().into(),
                calls.clone(),
                if failed_store { 100 } else { 0 },
            ),
        );
        assert!(result.is_err());
        assert!(f.config.store_root.is_dir());
        let reopened = DesktopRoot::open_owned(&f.config.store_root)
            .unwrap()
            .open(&before.marker.project_id, &before.marker.session_id)
            .unwrap();
        assert_eq!(reopened.snapshot().unwrap().state, before.state);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
        assert_eq!(f.preparations.load(Ordering::SeqCst), 0);
    }
}

fn provisioning_request(
    id: &str,
    revision: u64,
    provider: &str,
    model: &str,
    effort: &str,
) -> Request {
    request(
        id,
        RequestBody::SessionConfigure(
            session_id(),
            polaris_desktop_protocol::request::SessionConfigure {
                history_mode: Default::default(),
                expected_configuration_revision: DecimalU64::new(revision),
                provider: provider.into(),
                model: model.into(),
                effort: effort.into(),
            },
        ),
    )
}
#[test]
fn storage_provisioning_advertises_persists_replays_and_enforces_cas() {
    let mut f = fixture(Behavior::Complete, false);
    drop(f.service.engine.take());
    f.service = DesktopService::open(f.config.clone()).unwrap();
    let e = f.service.engine.as_mut().unwrap();
    let SuccessResult::Hello(hello) = e
        .handle(&request("hello", RequestBody::Hello), &mut vec![])
        .unwrap()
    else {
        panic!()
    };
    assert!(hello.capabilities.contains(&Capability::SessionConfigure));
    assert!(!hello.capabilities.contains(&Capability::RunStart));
    let first = provisioning_request("configure-a", 0, "openai", "model-a", "high");
    let ack = e.handle(&first, &mut vec![]).unwrap();
    let second = provisioning_request("configure-b", 1, "ollama", "model-b", "medium");
    e.handle(&second, &mut vec![]).unwrap();
    let saved = e.store.snapshot().unwrap();
    assert_eq!(
        saved.state.configuration.configuration_revision,
        DecimalU64::new(2)
    );
    assert_eq!(saved.state.policy_revision, DecimalU64::new(0));
    assert_eq!(
        serde_json::to_value(e.handle(&first, &mut vec![]).unwrap()).unwrap(),
        serde_json::to_value(&ack).unwrap()
    );
    assert_eq!(e.store.snapshot().unwrap().marker, saved.marker);
    let stale = provisioning_request("configure-stale", 0, "codex", "model-c", "medium");
    assert_eq!(
        e.handle(&stale, &mut vec![]).unwrap_err().code,
        ErrorCode::RevisionConflict
    );
    let conflict = provisioning_request("configure-a", 0, "openai", "different", "high");
    assert_eq!(
        e.handle(&conflict, &mut vec![]).unwrap_err().code,
        ErrorCode::RevisionConflict
    );
    drop(f.service.engine.take());
    f.service = DesktopService::open(f.config.clone()).unwrap();
    let e = f.service.engine.as_mut().unwrap();
    e.handle(&request("hello-reopen", RequestBody::Hello), &mut vec![])
        .unwrap();
    assert_eq!(
        e.store.snapshot().unwrap().state.configuration,
        saved.state.configuration
    );
    assert_eq!(
        serde_json::to_value(e.handle(&first, &mut vec![]).unwrap()).unwrap(),
        serde_json::to_value(ack).unwrap()
    );
    assert_eq!(f.preparations.load(Ordering::SeqCst), 0);
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
}
#[test]
fn storage_provisioning_refuses_invalid_busy_and_configured_owner() {
    let mut f = fixture(Behavior::Complete, false);
    let e = f.service.engine.as_mut().unwrap();
    let SuccessResult::Hello(hello) = e
        .handle(&request("hello", RequestBody::Hello), &mut vec![])
        .unwrap()
    else {
        panic!()
    };
    assert!(!hello.capabilities.contains(&Capability::SessionConfigure));
    assert_eq!(
        e.handle(
            &provisioning_request("denied-owner", 0, "openai", "model", "medium"),
            &mut vec![]
        )
        .unwrap_err()
        .code,
        ErrorCode::CapabilityUnavailable
    );
    drop(f.service.engine.take());
    f.service = DesktopService::open(f.config.clone()).unwrap();
    let e = f.service.engine.as_mut().unwrap();
    e.handle(&request("hello", RequestBody::Hello), &mut vec![])
        .unwrap();
    let before = e.store.snapshot().unwrap();
    for (provider, model, effort) in [
        ("fake", "scripted", "none"),
        ("openai", "", "medium"),
        ("codex", "model", "none"),
        ("ollama", "model", "high"),
        ("lmstudio", "model", "none"),
    ] {
        assert_eq!(
            e.handle(
                &provisioning_request("invalid", 0, provider, model, effort),
                &mut vec![]
            )
            .unwrap_err()
            .code,
            ErrorCode::CapabilityUnavailable
        );
    }
    assert_eq!(before.marker, e.store.snapshot().unwrap().marker);
    e.store
        .apply(
            &request(
                "draft",
                RequestBody::DraftUpdate(
                    session_id(),
                    DraftUpdate {
                        expected_draft_revision: DecimalU64::new(0),
                        text: "pending".into(),
                        attachment_ids: vec![],
                    },
                ),
            ),
            None,
        )
        .unwrap();
    e.store
        .apply(
            &request(
                "accepted-run",
                RequestBody::RunStart(
                    session_id(),
                    RunStart {
                        expected_draft_revision: DecimalU64::new(1),
                        expected_configuration_revision: DecimalU64::new(0),
                        expected_policy_revision: DecimalU64::new(0),
                    },
                ),
            ),
            Some(RunTarget {
                run_id: RunId::new("unsettled").unwrap(),
                attempt_id: AttemptId::new("attempt").unwrap(),
            }),
        )
        .unwrap();
    assert_eq!(
        e.handle(
            &provisioning_request("busy", 0, "codex", "model", "medium"),
            &mut vec![]
        )
        .unwrap_err()
        .code,
        ErrorCode::SessionBusy
    );
    assert_eq!(f.calls.load(Ordering::SeqCst), 0);
}
