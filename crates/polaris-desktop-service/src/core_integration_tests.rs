//! Real core + CLI helper + durable approval, with a deterministic local provider.
//! Explicit opt-in test: no credentials, network, or user workspace.
use crate::execution::RunExecution;
use polaris_core::{
    audit::AuditLog,
    desktop_run::{DesktopRun, DesktopRunInput},
    desktop_store::{InitialState, PrototypeRoot},
    isolated_run::PreparedWorkspace,
    isolated_workspace::{Limits, RegisteredSecrets, collect_changes},
    session::Session,
};
use polaris_desktop_protocol::{
    event::EventBody, ids::*, request::*, run_state::Observation, snapshot::*,
};
use polaris_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError, ToolCall};
use std::{
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

struct Scripted {
    step: AtomicUsize,
    source: PathBuf,
}
#[async_trait::async_trait]
impl Provider for Scripted {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        assert!(!request.tools.is_empty());
        let step = self.step.fetch_add(1, Ordering::SeqCst);
        let call = match step {
            0 => Some(("read", serde_json::json!({"path":"ordinary.txt"}))),
            1 => {
                assert!(
                    request
                        .messages
                        .last()
                        .unwrap()
                        .content
                        .contains("ordinary fixture")
                );
                // This source is a test-created private path, containing no shell quotes.
                let secret = self.source.join(".env");
                assert!(!secret.to_str().unwrap().contains('\''));
                Some((
                    "bash",
                    serde_json::json!({"command":format!(
                    "if /bin/cat '{}'; then exit 90; fi; printf shell-result > shell.txt", secret.display())}),
                ))
            }
            2 => {
                assert!(
                    !request
                        .messages
                        .last()
                        .unwrap()
                        .content
                        .contains("SYNTHETIC_PRIVATE_VALUE")
                );
                Some((
                    "write",
                    serde_json::json!({"path":"written.txt", "content":"helper-result"}),
                ))
            }
            3 => {
                assert!(
                    !request
                        .messages
                        .last()
                        .unwrap()
                        .content
                        .contains("unavailable")
                );
                None
            }
            _ => panic!("unexpected provider retry"),
        };
        Ok(CompletionResponse {
            text: if call.is_none() {
                "fixture complete".into()
            } else {
                String::new()
            },
            tool_calls: call
                .into_iter()
                .map(|(name, arguments)| ToolCall {
                    id: format!("call-{step}"),
                    name: name.into(),
                    arguments,
                })
                .collect(),
            usage: Some(polaris_provider::Usage {
                input_tokens: 10,
                output_tokens: 1,
                total_tokens: 11,
                cached_tokens: 0,
            }),
            ..Default::default()
        })
    }
}
fn req(id: &str, body: RequestBody) -> Request {
    Request {
        protocol_version: Default::default(),
        client_id: ClientId::new("fixture-client").unwrap(),
        request_id: RequestId::new(id).unwrap(),
        body,
    }
}

#[test]
#[ignore = "explicit real macOS sandbox/helper integration; build polaris-cli first"]
fn core_helper_approval_and_transcript_roundtrip() {
    const MARKER: &str = "POLARIS_CORE_INTEGRATION_CHILD";
    if std::env::var_os(MARKER).is_none() {
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "core_integration_tests::core_helper_approval_and_transcript_roundtrip",
                "--ignored",
                "--nocapture",
            ])
            .env_clear()
            .env(MARKER, "1")
            .env("HOME", home.path().canonicalize().unwrap())
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
    run_scenario(false);
    run_scenario(true);
}

fn run_scenario(cancel_at_approval: bool) {
    let temp = tempfile::tempdir().unwrap();
    let base = temp.path().canonicalize().unwrap();
    let source = base.join("source");
    std::fs::create_dir(&source).unwrap();
    std::fs::write(source.join("ordinary.txt"), "ordinary fixture").unwrap();
    std::fs::write(source.join(".env"), "SYNTHETIC_PRIVATE_VALUE").unwrap();
    let helper = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/debug/polaris")
        .canonicalize()
        .expect("build polaris-cli first");
    let limits = Limits {
        max_entries: 64,
        max_files: 32,
        max_file_bytes: 65536,
        max_total_bytes: 1048576,
        max_depth: 8,
    };
    let prepared = Arc::new(
        PreparedWorkspace::prepare(
            &source,
            &helper,
            polaris_core::desktop_execution::SandboxMode::WorkspaceWrite,
            &["/bin", "/usr/bin", "/usr/lib", "/System/Library"].map(PathBuf::from),
            limits,
        )
        .unwrap(),
    );
    assert!(!prepared.snapshot().path().join(".env").exists());
    let root = PrototypeRoot::new().unwrap();
    let project = ProjectId::new("fixture-project").unwrap();
    let session_id = SessionId::new("fixture-session").unwrap();
    let mut store = root
        .create(
            project.clone(),
            session_id.clone(),
            InitialState {
                draft: Draft {
                    draft_revision: DecimalU64::new(0),
                    text: "fixture input".into(),
                    attachment_ids: vec![],
                },
                configuration: Configuration {
                    history_mode: Default::default(),
                    configuration_revision: DecimalU64::new(0),
                    provider: "fixture".into(),
                    model: "fixture".into(),
                    effort: "medium".into(),
                },
                policy_revision: DecimalU64::new(0),
            },
        )
        .unwrap();
    let target = RunTarget {
        run_id: RunId::new("fixture-run").unwrap(),
        attempt_id: AttemptId::new("fixture-attempt").unwrap(),
    };
    store
        .apply(
            &req(
                "start",
                RequestBody::RunStart(
                    session_id.clone(),
                    RunStart {
                        expected_draft_revision: DecimalU64::new(0),
                        expected_configuration_revision: DecimalU64::new(0),
                        expected_policy_revision: DecimalU64::new(0),
                    },
                ),
            ),
            Some(target.clone()),
        )
        .unwrap();
    let operation = OperationId::new("provider-turn").unwrap();
    store.record_intent(&target, operation.clone()).unwrap();
    let mut execution = RunExecution::with_prepared_workspace(
        &target,
        Arc::new(AtomicBool::new(false)),
        prepared.clone(),
    )
    .unwrap();
    let provider = Arc::new(Scripted {
        step: AtomicUsize::new(0),
        source: source.clone(),
    });
    let mut session = Session::new();
    session.push_user("fixture input");
    let (mut worker, mut events) = DesktopRun::start(DesktopRunInput {
        run_id: target.run_id.clone(),
        prepared: prepared.clone(),
        execution: execution.port.clone(),
        provider: provider.clone(),
        provider_pool: provider.clone(),
        session,
        always_on: polaris_core::prompt::assemble_always_on("", "", &[]),
        skills: vec![],
        agent_types: vec![],
        max_turns: 8,
        spawn_concurrency: 1,
        spawn_write_concurrency: 1,
        audit: Arc::new(tokio::sync::Mutex::new(
            AuditLog::open(&base.join("audit.jsonl")).unwrap(),
        )),
    })
    .unwrap();
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut approvals = 0;
    let completion = loop {
        assert!(Instant::now() < deadline, "core integration deadline");
        execution.step(&mut store, &target).unwrap();
        while events.try_recv().is_ok() {}
        for event in execution.take_approval_events() {
            if let EventBody::ApprovalRequested(approval) = event {
                // Always: the mutation must not have started before the durable answer.
                let pending_path = if approvals == 0 {
                    "shell.txt"
                } else {
                    "written.txt"
                };
                assert!(!prepared.snapshot().path().join(pending_path).exists());
                assert!(
                    store
                        .snapshot()
                        .unwrap()
                        .state
                        .unresolved_approvals
                        .iter()
                        .any(|a| a.approval_id == approval.approval_id)
                );
                approvals += 1;
                if cancel_at_approval {
                    store
                        .apply(
                            &req(
                                "cancel",
                                RequestBody::RunCancel(session_id.clone(), target.clone()),
                            ),
                            None,
                        )
                        .unwrap();
                    worker.cancel();
                    execution.cancel();
                    continue;
                }
                store
                    .resolve_approval(
                        &req(
                            &format!("approval-{approvals}"),
                            RequestBody::ApprovalResolve(
                                session_id.clone(),
                                ApprovalResolve {
                                    approval_id: approval.approval_id.clone(),
                                    run_id: target.run_id.clone(),
                                    attempt_id: target.attempt_id.clone(),
                                    policy_revision: approval.policy_revision,
                                    decision: ApprovalDecision::Allow,
                                },
                            ),
                        ),
                        crate::execution::now_ms().unwrap(),
                    )
                    .unwrap();
                execution.approval_resolved(&approval.approval_id, ApprovalDecision::Allow);
            }
        }
        if let Some(completion) = worker.try_join() {
            break completion.unwrap();
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    if cancel_at_approval {
        assert!(completion.result.is_err());
        assert_eq!(approvals, 1);
    } else {
        assert_eq!(completion.result.as_ref().unwrap().text, "fixture complete");
        assert_eq!(
            approvals, 2,
            "read grant is automatic; shell/write are approved individually"
        );
    }
    execution.cancel();
    while !execution.quiescent() {
        assert!(Instant::now() < deadline);
        execution.step(&mut store, &target).unwrap();
        std::thread::sleep(Duration::from_millis(5));
    }
    if cancel_at_approval {
        assert!(!prepared.snapshot().path().join("shell.txt").exists());
        assert!(!source.join("shell.txt").exists());
        assert_eq!(provider.step.load(Ordering::SeqCst), 2);
        let messages = completion.session.messages.into_iter().skip(1).collect();
        store
            .finish_with_messages(
                &target,
                &operation,
                Observation::Cancelled,
                ResultId::new("cancel-result").unwrap(),
                messages,
            )
            .unwrap();
        execution.saved = true;
        let before = store.snapshot().unwrap();
        assert_eq!(
            before.state.runs[0].run.state,
            polaris_desktop_protocol::run_state::RunState::Cancelled
        );
        assert!(before.state.unresolved_approvals.is_empty());
        drop(store);
        let reopened = root
            .open(&project, &session_id)
            .unwrap()
            .snapshot()
            .unwrap();
        assert_eq!(reopened.marker, before.marker);
        assert!(!source.join("shell.txt").exists());
        return;
    }
    assert_eq!(
        std::fs::read_to_string(prepared.snapshot().path().join("shell.txt")).unwrap(),
        "shell-result"
    );
    assert_eq!(
        std::fs::read_to_string(prepared.snapshot().path().join("written.txt")).unwrap(),
        "helper-result"
    );
    assert!(!source.join("shell.txt").exists());
    let messages = completion
        .session
        .messages
        .into_iter()
        .skip(1)
        .collect::<Vec<_>>();
    assert_eq!(messages.len(), 7);
    store
        .finish_with_messages(
            &target,
            &operation,
            Observation::Succeeded,
            ResultId::new("turn-result").unwrap(),
            messages,
        )
        .unwrap();
    execution.saved = true;
    let saved = store.snapshot().unwrap();
    assert_eq!(saved.raw.len(), 8);
    drop(store);
    let reopened = root
        .open(&project, &session_id)
        .unwrap()
        .snapshot()
        .unwrap();
    assert_eq!(
        reopened.raw[2].message.tool_call_id.as_deref(),
        Some("call-0")
    );
    assert_eq!(reopened.marker, saved.marker);
    let changes = collect_changes(
        prepared.snapshot(),
        &RegisteredSecrets::new([]).unwrap(),
        limits,
    )
    .unwrap();
    assert!(changes.is_applicable());
    // Fixture's trusted owner authorizes apply only after actual worker + tool cleanup.
    let recovery = base.join("recovery");
    std::fs::create_dir(&recovery).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&recovery, std::fs::Permissions::from_mode(0o700)).unwrap();
    let report = polaris_core::workspace_apply::protected_apply(
        prepared.snapshot(),
        &changes,
        limits,
        &recovery,
    );
    assert!(report.failure.is_none(), "{:?}", report.failure);
    assert_eq!(
        std::fs::read_to_string(source.join("written.txt")).unwrap(),
        "helper-result"
    );
    assert_eq!(
        std::fs::read_to_string(source.join(".env")).unwrap(),
        "SYNTHETIC_PRIVATE_VALUE"
    );
}
