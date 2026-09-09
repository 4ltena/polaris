//! Owned desktop worker completion, cancellation, and trusted input boundary tests.
use super::*;
use std::os::unix::fs::PermissionsExt;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
enum Behavior {
    Complete,
    Pending,
    Panic,
    Error,
}
struct Fake {
    calls: Arc<AtomicUsize>,
    behavior: Behavior,
}
#[async_trait::async_trait]
impl Provider for Fake {
    async fn complete(
        &self,
        request: polaris_provider::CompletionRequest,
    ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
        // Count every request, including an unexpected request that panics.
        self.calls.fetch_add(1, Ordering::SeqCst);
        // This in-memory run has no classification request. The first call is
        // the main agent request, with all_specs from assemble_always_on. Never
        // mistake an empty-tools summary/classification request for completion.
        let expected = crate::prompt::assemble_always_on("", "", &[]);
        assert_eq!(request.system, expected.system());
        assert!(!request.tools.is_empty());
        assert_eq!(
            serde_json::to_value(&request.tools).unwrap(),
            serde_json::to_value(expected.tools()).unwrap(),
        );
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].content, "fixture input");
        // Advertised schemas are not executed: all fake replies have no calls.
        match self.behavior {
            Behavior::Complete => Ok(polaris_provider::CompletionResponse {
                text: "fake completion".into(),
                ..Default::default()
            }),
            Behavior::Pending => std::future::pending().await,
            Behavior::Panic => panic!("fixture worker panic"),
            Behavior::Error => Err(polaris_provider::ProviderError::Http(
                "fixture failure".into(),
            )),
        }
    }
}

fn fixture(
    behavior: Behavior,
) -> (
    tempfile::TempDir,
    DesktopRunInput,
    tokio::sync::mpsc::Receiver<crate::desktop_execution::ExecutionRequest>,
    Arc<AtomicUsize>,
) {
    let temp = tempfile::tempdir().unwrap();
    let root = std::fs::canonicalize(temp.path()).unwrap();
    let source = root.join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("ordinary"), b"fixture").unwrap();
    let helper = root.join("helper");
    std::fs::write(&helper, b"dummy executable; never launched").unwrap();
    std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
    // Only reads the in-memory protection registry; no auth loader/provider.
    let prepared = Arc::new(
        PreparedWorkspace::prepare(
            &source,
            &helper,
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[],
            crate::isolated_workspace::Limits {
                max_entries: 16,
                max_files: 8,
                max_file_bytes: 1024,
                max_total_bytes: 4096,
                max_depth: 4,
            },
        )
        .unwrap(),
    );
    let run_id = RunId::new("fixture-run").unwrap();
    let (execution, requests) = ExecutionPort::channel_with_read_helper(
        run_id.clone(),
        prepared.helper_path().into(),
        prepared
            .policy()
            .restrict(polaris_sandbox::SandboxMode::ReadOnly, &[])
            .unwrap(),
    )
    .unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let provider: Arc<dyn Provider> = Arc::new(Fake {
        calls: calls.clone(),
        behavior,
    });
    let mut session = Session::new();
    session.push_user("fixture input");
    let input = DesktopRunInput {
        run_id,
        prepared,
        execution,
        provider: provider.clone(),
        provider_pool: provider,
        session,
        always_on: crate::prompt::assemble_always_on("", "", &[]),
        skills: vec![],
        agent_types: vec![],
        max_turns: 4,
        spawn_concurrency: 1,
        spawn_write_concurrency: 1,
        audit: Arc::new(Mutex::new(
            AuditLog::open(&root.join("audit.jsonl")).unwrap(),
        )),
    };
    (temp, input, requests, calls)
}

fn wait_until(mut predicate: impl FnMut() -> bool) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while !predicate() {
        assert!(Instant::now() < deadline, "fixture deadline exceeded");
        std::thread::sleep(Duration::from_millis(1));
    }
}
fn joined(worker: &mut DesktopRun) -> DesktopRunCompletion {
    let mut completion = None;
    wait_until(|| {
        completion = worker.try_join();
        completion.is_some()
    });
    completion.unwrap().unwrap()
}

#[test]
fn agents_refresh_uses_original_source_at_each_desktop_request() {
    struct UpdatingProvider {
        original: std::path::PathBuf,
        captured: Arc<std::sync::Mutex<Vec<String>>>,
    }
    #[async_trait::async_trait]
    impl Provider for UpdatingProvider {
        async fn complete(
            &self,
            request: polaris_provider::CompletionRequest,
        ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
            let first = {
                let mut captured = self.captured.lock().unwrap();
                captured.push(request.system);
                captured.len() == 1
            };
            if first {
                std::fs::write(&self.original, "## Always on\n- updated host instruction\n")
                    .unwrap();
                Ok(polaris_provider::CompletionResponse {
                    tool_calls: vec![polaris_provider::ToolCall {
                        id: "refresh-continuation".into(),
                        name: "skill".into(),
                        arguments: serde_json::json!({"q": ""}),
                    }],
                    ..Default::default()
                })
            } else {
                Ok(polaris_provider::CompletionResponse {
                    text: "done".into(),
                    ..Default::default()
                })
            }
        }
    }
    let (temp, mut input, mut requests, _) = fixture(Behavior::Complete);
    let source = std::fs::canonicalize(temp.path().join("source")).unwrap();
    let original = source.join("AGENTS.md");
    std::fs::write(&original, "## Always on\n- initial host instruction\n").unwrap();
    std::fs::write(
        input.prepared.snapshot().path().join("AGENTS.md"),
        "## Always on\n- copy must not become authority\n",
    )
    .unwrap();
    input.always_on = input
        .always_on
        .with_agents_refresh(crate::constitution::AgentsRefresh::new(None, &source).unwrap())
        .unwrap();
    let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
    input.provider = Arc::new(UpdatingProvider {
        original,
        captured: captured.clone(),
    });
    let (mut worker, _events) = DesktopRun::start(input).unwrap();
    let completion = joined(&mut worker);
    assert_eq!(completion.result.unwrap().text, "done");
    let captured = captured.lock().unwrap();
    assert_eq!(captured.len(), 2);
    assert!(captured[0].contains("initial host instruction"));
    assert!(!captured[0].contains("updated host instruction"));
    assert!(captured[1].contains("updated host instruction"));
    assert!(!captured[1].contains("initial host instruction"));
    assert!(
        captured
            .iter()
            .all(|system| !system.contains("copy must not become authority"))
    );
    assert!(requests.try_recv().is_err());
}

#[test]
fn actual_agent_completion_is_returned_once_after_join() {
    let (_temp, mut input, mut requests, calls) = fixture(Behavior::Complete);
    input.session.compaction_threshold = Some(0);
    let weak = Arc::downgrade(&input.prepared);
    let (mut worker, _events) = DesktopRun::start(input).unwrap();
    let completion = joined(&mut worker);
    assert_eq!(completion.result.unwrap().text, "fake completion");
    assert_eq!(completion.session.messages.len(), 2);
    assert_eq!(completion.session.compaction_threshold, Some(usize::MAX));
    assert!(completion.session.disable_files_md_auto_regenerate);
    assert_eq!(completion.session.messages[1].content, "fake completion");
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(requests.try_recv().is_err());
    assert!(weak.upgrade().is_none());
    assert!(worker.try_join().is_none());
}

#[test]
fn workflow_uses_trusted_skill_snapshot_without_reopening_host_paths() {
    struct Capture(Arc<std::sync::Mutex<Vec<String>>>);
    #[async_trait::async_trait]
    impl Provider for Capture {
        async fn complete(
            &self,
            request: polaris_provider::CompletionRequest,
        ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
            self.0.lock().unwrap().push(request.system);
            Ok(polaris_provider::CompletionResponse {
                text: "done".into(),
                ..Default::default()
            })
        }
    }
    for available in [true, false] {
        let (_temp, mut input, _requests, _) = fixture(Behavior::Complete);
        let captured = Arc::new(std::sync::Mutex::new(Vec::new()));
        input.provider = Arc::new(Capture(captured.clone()));
        input.session.workflow = Some(crate::workflow::SessionWorkflow::new(
            crate::workflow::WorkflowConfig {
                enabled: true,
                always: vec!["user:trusted-workflow".into()],
                ..Default::default()
            },
        ));
        if available {
            input.skills.push(polaris_skills::Skill {
                name: "trusted-workflow".into(),
                description: "fixture".into(),
                body: "Use the trusted workflow snapshot.".into(),
                path: "/not-opened-by-desktop/fixture/SKILL.md".into(),
            });
        }
        let (mut worker, _events) = DesktopRun::start(input).unwrap();
        let completion = joined(&mut worker);
        assert_eq!(completion.result.is_ok(), available);
        let sent = captured.lock().unwrap();
        assert_eq!(sent.len(), usize::from(available));
        if available {
            assert!(sent[0].contains("Use the trusted workflow snapshot."));
            assert!(
                !completion
                    .session
                    .workflow
                    .unwrap()
                    .state()
                    .skill_manifest_hash
                    .is_empty()
            );
        }
    }
}

#[test]
fn tool_retention_shortens_requests_but_completion_returns_original_raw_history() {
    struct Memory;
    impl crate::tool_memory::ToolMemoryBackend for Memory {
        fn save<'a>(
            &'a self,
            _: &'a polaris_provider::ToolCall,
            text: &'a str,
        ) -> futures_util::future::BoxFuture<'a, std::io::Result<crate::tool_memory::SavedToolResult>>
        {
            Box::pin(async move {
                Ok(crate::tool_memory::SavedToolResult {
                    id: "saved-fixture".into(),
                    preview: "excerpt".into(),
                    bytes: text.len(),
                })
            })
        }
        fn read<'a>(
            &'a self,
            _: &'a str,
            _: usize,
            _: usize,
        ) -> futures_util::future::BoxFuture<'a, std::io::Result<String>> {
            Box::pin(async { Err(std::io::Error::other("not used by fixture")) })
        }
    }
    struct Inspect;
    #[async_trait::async_trait]
    impl Provider for Inspect {
        async fn complete(
            &self,
            request: polaris_provider::CompletionRequest,
        ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
            assert!(
                request.messages[1]
                    .content
                    .starts_with("[Stored tool result ")
            );
            assert!(request.messages[1].content.len() < 1000);
            Ok(polaris_provider::CompletionResponse {
                text: "done".into(),
                ..Default::default()
            })
        }
    }
    let (_temp, mut input, _requests, _) = fixture(Behavior::Complete);
    let evidence = "original evidence\n".repeat(300);
    input.session.messages = vec![
        polaris_provider::Message::assistant_with_tool_calls(
            "",
            vec![polaris_provider::ToolCall {
                id: "old-call".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path":"ordinary"}),
            }],
        ),
        polaris_provider::Message::tool_result("old-call", &evidence),
        polaris_provider::Message::user("continue"),
    ];
    input.session.tool_memory = Some(crate::tool_memory::ToolMemory {
        backend: Arc::new(Memory),
        mode: crate::tool_memory::RetentionMode::Retrieval,
        threshold_bytes: 512,
    });
    input.provider = Arc::new(Inspect);
    let (mut worker, _events) = DesktopRun::start(input).unwrap();
    let completion = joined(&mut worker);
    assert!(completion.result.is_ok());
    assert_eq!(completion.session.messages[1].content, evidence);
    assert_eq!(completion.session.messages.last().unwrap().content, "done");
}

#[test]
fn rejected_assistant_output_never_dispatches_its_tool() {
    struct Oversized(Arc<AtomicUsize>);
    #[async_trait::async_trait]
    impl Provider for Oversized {
        async fn complete(
            &self,
            _: polaris_provider::CompletionRequest,
        ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
            self.0.fetch_add(1, Ordering::SeqCst);
            Ok(polaris_provider::CompletionResponse {
                text: "\0".repeat(22_000),
                tool_calls: vec![polaris_provider::ToolCall {
                    id: "must-not-dispatch".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command":"echo must-not-run"}),
                }],
                ..Default::default()
            })
        }
    }
    let (_temp, mut input, mut requests, _) = fixture(Behavior::Complete);
    let calls = Arc::new(AtomicUsize::new(0));
    input.provider = Arc::new(Oversized(calls.clone()));
    let (mut worker, _events) = DesktopRun::start(input).unwrap();
    let completion = joined(&mut worker);
    assert!(completion.result.is_err());
    assert!(completion.session.persistence_error.is_some());
    assert_eq!(completion.session.messages.len(), 1);
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(requests.try_recv().is_err());
}

#[test]
fn newly_generated_tool_output_survives_retention_then_cancellation() {
    struct Memory;
    impl crate::tool_memory::ToolMemoryBackend for Memory {
        fn save<'a>(
            &'a self,
            _: &'a polaris_provider::ToolCall,
            text: &'a str,
        ) -> futures_util::future::BoxFuture<'a, std::io::Result<crate::tool_memory::SavedToolResult>>
        {
            Box::pin(async move {
                Ok(crate::tool_memory::SavedToolResult {
                    id: "new-result".into(),
                    preview: "preview".into(),
                    bytes: text.len(),
                })
            })
        }
        fn read<'a>(
            &'a self,
            _: &'a str,
            _: usize,
            _: usize,
        ) -> futures_util::future::BoxFuture<'a, std::io::Result<String>> {
            Box::pin(async { unreachable!("fixture never reads memory") })
        }
    }
    struct ReadThenWait(Arc<AtomicUsize>);
    #[async_trait::async_trait]
    impl Provider for ReadThenWait {
        async fn complete(
            &self,
            request: polaris_provider::CompletionRequest,
        ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
            if self.0.load(Ordering::SeqCst) == 0 {
                self.0.store(1, Ordering::SeqCst);
                return Ok(polaris_provider::CompletionResponse {
                    tool_calls: vec![polaris_provider::ToolCall {
                        id: "read-new".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path":"ordinary"}),
                    }],
                    ..Default::default()
                });
            }
            assert!(
                request
                    .messages
                    .last()
                    .unwrap()
                    .content
                    .starts_with("[Stored tool result ")
            );
            self.0.store(2, Ordering::SeqCst);
            std::future::pending().await
        }
    }
    let (_temp, mut input, mut requests, _) = fixture(Behavior::Complete);
    let step = Arc::new(AtomicUsize::new(0));
    input.provider = Arc::new(ReadThenWait(step.clone()));
    input.session.tool_memory = Some(crate::tool_memory::ToolMemory {
        backend: Arc::new(Memory),
        mode: crate::tool_memory::RetentionMode::Retrieval,
        threshold_bytes: 512,
    });
    let (mut worker, _events) = DesktopRun::start(input).unwrap();
    let mut request = None;
    wait_until(|| {
        request = requests.try_recv().ok();
        request.is_some()
    });
    let original = "new raw evidence\n".repeat(300);
    request
        .unwrap()
        .reply
        .send(Arc::new(crate::desktop_execution::ExecutionResult {
            end: crate::desktop_execution::ControlledEnd::Exited,
            status: Some(0),
            stdout: serde_json::to_string(&polaris_tools::isolated_read::Reply::Text(
                original.clone(),
            ))
            .unwrap(),
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_eof: true,
            stderr_eof: true,
            problem: None,
        }))
        .unwrap();
    wait_until(|| step.load(Ordering::SeqCst) == 2);
    worker.cancel();
    let completion = joined(&mut worker);
    assert!(completion.result.is_err());
    assert!(completion.session.persistence_error.is_none());
    assert_eq!(completion.session.messages.len(), 3);
    assert_eq!(completion.session.messages[2].content, original);
    assert_eq!(
        completion.session.messages[2].tool_call_id.as_deref(),
        Some("read-new")
    );
    assert!(requests.try_recv().is_err());
}

#[test]
fn pending_cancel_is_not_completion_and_preserves_history_and_workspace() {
    let (_temp, input, _requests, calls) = fixture(Behavior::Pending);
    let weak = Arc::downgrade(&input.prepared);
    let (mut worker, _events) = DesktopRun::start(input).unwrap();
    wait_until(|| calls.load(Ordering::SeqCst) == 1);
    assert!(worker.try_join().is_none());
    assert!(weak.upgrade().is_some());
    worker.cancel();
    let completion = joined(&mut worker);
    assert!(matches!(
        completion.result,
        Err(DesktopRunError::Agent(AgentError::Desktop(
            crate::desktop_events::EventFailure::Cancelled
        )))
    ));
    assert_eq!(completion.session.messages.len(), 1);
    assert!(weak.upgrade().is_none());
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[test]
fn drop_cancels_pending_future_and_releases_thread_owned_workspace() {
    let (_temp, input, _requests, calls) = fixture(Behavior::Pending);
    let weak = Arc::downgrade(&input.prepared);
    let (worker, _events) = DesktopRun::start(input).unwrap();
    wait_until(|| calls.load(Ordering::SeqCst) == 1);
    let control = worker.control();
    drop(worker);
    assert_eq!(
        control.failure(),
        Some(crate::desktop_events::EventFailure::Cancelled)
    );
    wait_until(|| weak.upgrade().is_none());
}

#[test]
fn wrong_run_missing_grant_and_other_prepared_policy_make_no_provider_calls() {
    for mismatch in 0..3 {
        let (_temp, mut input, _requests, calls) = fixture(Behavior::Complete);
        let (_other_temp, other, _other_requests, _) = fixture(Behavior::Complete);
        match mismatch {
            0 => input.run_id = RunId::new("wrong-run").unwrap(),
            1 => input.execution = ExecutionPort::channel(input.run_id.clone()).0,
            _ => input.execution = other.execution,
        }
        assert!(matches!(
            DesktopRun::start(input),
            Err(DesktopRunError::ExecutionMismatch)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn v2_persistence_and_sticky_failure_are_rejected_before_provider() {
    for persistence in [false, true] {
        let (temp, mut input, _requests, calls) = fixture(Behavior::Complete);
        if persistence {
            input.session.persistence = Some(
                crate::session_store::PersistedSession::create(
                    &temp.path().join("data"),
                    &temp.path().join("index.sqlite"),
                    "fixture",
                    crate::conversation_state::HistoryMode::default(),
                    None,
                )
                .unwrap(),
            );
        } else {
            input.session.persistence_error = Some("prior v2 failure".into());
        }
        assert!(matches!(
            DesktopRun::start(input),
            Err(DesktopRunError::LegacySession)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn panic_and_provider_error_are_not_retried_or_reported_as_success() {
    for behavior in [Behavior::Panic, Behavior::Error] {
        let (_temp, input, _requests, calls) = fixture(behavior);
        let (mut worker, _events) = DesktopRun::start(input).unwrap();
        let completion = joined(&mut worker);
        match behavior {
            Behavior::Panic => assert!(matches!(completion.result, Err(DesktopRunError::Panicked))),
            _ => assert!(matches!(
                completion.result,
                Err(DesktopRunError::Agent(AgentError::Provider(_)))
            )),
        }
        assert_eq!(completion.session.messages.len(), 1);
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn invalid_turn_and_concurrency_limits_never_start_provider() {
    for limits in [
        (0, 1, 1),
        (MAX_TURNS + 1, 1, 1),
        (4, 0, 1),
        (4, MAX_CONCURRENCY + 1, 1),
        (4, 1, 0),
        (4, 1, 2),
    ] {
        let (_temp, mut input, _requests, calls) = fixture(Behavior::Complete);
        (
            input.max_turns,
            input.spawn_concurrency,
            input.spawn_write_concurrency,
        ) = limits;
        assert!(matches!(
            DesktopRun::start(input),
            Err(DesktopRunError::InvalidLimits)
        ));
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }
}
