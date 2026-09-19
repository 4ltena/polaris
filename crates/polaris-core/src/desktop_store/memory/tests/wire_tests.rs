//! Actual owner context and agent requests, serialized by both provider builders.

use super::*;
use polaris_provider::{CompletionRequest, CompletionResponse, Message, Provider, Role, ToolCall};
use std::sync::Mutex;

#[tokio::test]
async fn structured_owner_full_wire_budget_preserves_workflow_catalog_and_tool_continuation() {
    owner_wire_and_audit(false).await;
}

#[tokio::test]
async fn structured_owner_related_omission_reaches_actual_agent_audit_once() {
    owner_wire_and_audit(true).await;
}

async fn owner_wire_and_audit(omit_related: bool) {
    struct Capture(Mutex<Vec<CompletionRequest>>);
    #[async_trait::async_trait]
    impl Provider for Capture {
        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
            let mut requests = self.0.lock().unwrap();
            requests.push(request);
            Ok(match requests.len() {
                1 => CompletionResponse {
                    tool_calls: vec![ToolCall {
                        id: "load-deploy-guide".into(),
                        name: "skill".into(),
                        arguments: serde_json::json!({"q":"deploy-guide"}),
                    }],
                    ..Default::default()
                },
                2 => CompletionResponse {
                    text: "done".into(),
                    ..Default::default()
                },
                _ => panic!("unexpected provider request"),
            })
        }
    }
    struct NoAuxiliary;
    impl StrictSummaryProvider for NoAuxiliary {
        fn summarize<'a>(&'a self, _: SummaryRequest) -> FutureResult<'a, String> {
            panic!("published summary must be reused")
        }
    }
    #[async_trait::async_trait]
    impl Provider for NoAuxiliary {
        async fn complete(
            &self,
            _: CompletionRequest,
        ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
            panic!("no child provider should be called")
        }
    }
    let mut query = String::from("turn 11: Rigel deploy the service");
    if omit_related {
        for i in 0..128 {
            query.push_str(&format!(" unused{i}"));
        }
    }
    let (_root, mut writer, target) = setup_query_turns(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
        11,
        &query,
    );
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let (work, pending, response) = structured_pending(&mut writer, &target).await;
    writer
        .publish_structured_memory(&target, &work, &pending, &response, &database)
        .unwrap();
    let before = writer.memory_snapshot(&target).unwrap();
    let recent = before.conversation.recent_messages();
    let (runtime, mut owner) = crate::desktop_memory::DesktopMemory::pair(
        target.clone(),
        Arc::new(StrictHistory::new(
            Arc::new(NoAuxiliary),
            Arc::new(Embed(EmbeddingModel {
                model: "fixture".into(),
                revision: "fixed".into(),
                dimension: 2,
            })),
        )),
        database,
        resources(),
    )
    .unwrap();
    let skill_body = "OPTIONAL BODY: verify the service after deploying it.";
    let skill_path = directory.path().join("SKILL.md");
    std::fs::write(
        &skill_path,
        format!("---\nname: deploy-guide\ndescription: Deploys the service.\n---\n{skill_body}\n"),
    )
    .unwrap();
    let skill = polaris_skills::Skill {
        name: "deploy-guide".into(),
        description: "Deploys the service.".into(),
        body: skill_body.into(),
        path: skill_path,
    };
    let provider = Arc::new(Capture(Mutex::new(vec![])));
    let captured = provider.clone();
    let audit = Arc::new(tokio::sync::Mutex::new(
        crate::audit::AuditLog::open(&directory.path().join("audit.jsonl")).unwrap(),
    ));
    let baseline = crate::prompt::assemble_always_on("", "", &[]);
    let always_on = baseline.clone();
    let task = async move {
        let mut session = crate::session::Session::new();
        session.push_user(&query);
        session.desktop_memory = Some(runtime);
        session.workflow = Some(crate::workflow::SessionWorkflow::new(
            crate::workflow::WorkflowConfig {
                enabled: true,
                mode: crate::workflow::WorkflowMode::Adaptive,
                initial_phase: crate::workflow::Phase::Implement,
                ..crate::config::Config::default().workflow
            },
        ));
        let sandbox =
            polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[])
                .unwrap();
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = crate::agent::AutoApprove;
        let mut context = crate::agent::ToolContext {
            sandbox: &sandbox,
            helper: std::path::Path::new("/bin/true"),
            gate: &mut gate,
            approver: &mut approver,
        };
        crate::agent::run(
            &*provider,
            &mut session,
            audit,
            &mut crate::stop::StopTracker::new(3),
            &always_on,
            &[skill],
            // The shipped types, as every real run has: `spawn` is
            // withdrawn from a run with an empty catalogue, and an empty
            // one here would quietly measure a five-tool wire.
            &polaris_skills::builtin::agent_types(),
            Arc::new(NoAuxiliary),
            1,
            1,
            None,
            &mut context,
        )
        .await
    };
    tokio::pin!(task);
    let outcome = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            tokio::select! {
                result = &mut task => break result,
                _ = tokio::task::yield_now() => owner.poll(Some(&mut writer), false),
            }
        }
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(outcome.text, "done");
    let log = std::fs::read_to_string(directory.path().join("audit.jsonl")).unwrap();
    let records: Vec<serde_json::Value> = log
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let observations: Vec<_> = records
        .iter()
        .filter(|row| row["tool"] == "conversation-memory" && row["detail"] == "preparation")
        .collect();
    assert_eq!(observations.len(), 1);
    let detail = observations[0]["result"].as_str().unwrap();
    assert!(detail.contains("status=found"));
    assert!(detail.contains(if omit_related {
        "related_omitted=query_terms"
    } else {
        "related_omitted=none"
    }));
    assert!(!detail.contains("conversation://"));

    let requests = captured.0.lock().unwrap();
    assert_eq!(requests.len(), 2);
    let memory = writer.snapshot().unwrap();
    assert_eq!(memory.marker.raw_hash, before.conversation.raw_hash);
    let reported_tokens = memory
        .state
        .runs
        .last()
        .unwrap()
        .memory
        .as_ref()
        .unwrap()
        .reference_tokens
        .get();
    let evidence = &requests[0].messages[0];
    assert_eq!(evidence.role, Role::User);
    assert!(evidence.content.contains("Rigel"));
    assert_eq!(
        crate::budget::count_tokens(&evidence.content) as u64,
        reported_tokens
    );
    assert!(reported_tokens <= 768);
    assert_eq!(
        serde_json::to_value(&requests[0].messages[1..]).unwrap(),
        serde_json::to_value(&recent).unwrap()
    );
    assert_eq!(recent.iter().filter(|m| m.role == Role::User).count(), 10);
    assert!(!recent.iter().any(|m| m.content == "turn 1: Rigel"));
    assert_eq!(
        serde_json::to_value(&requests[1].messages[..requests[0].messages.len()]).unwrap(),
        serde_json::to_value(&requests[0].messages).unwrap()
    );
    assert_eq!(requests[1].messages.len(), requests[0].messages.len() + 2);
    let result = requests[1].messages.last().unwrap();
    assert_eq!(result.role, Role::Tool);
    assert_eq!(result.tool_call_id.as_deref(), Some("load-deploy-guide"));
    assert!(result.content.contains(skill_body));
    let base_names: Vec<_> = baseline
        .tools()
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(base_names.len(), 6);
    for (index, request) in requests.iter().enumerate() {
        assert_eq!(
            request
                .system
                .matches("Required workflow instructions")
                .count(),
            1
        );
        assert!(request.system.contains("phase=implement"));
        for phase in crate::workflow::Phase::ALL {
            assert_eq!(
                request
                    .system
                    .matches(&format!("### builtin:{phase}\n"))
                    .count(),
                usize::from(phase == crate::workflow::Phase::Implement)
            );
        }
        assert_eq!(
            request.system.matches("## Optional skill catalog").count(),
            1
        );
        assert!(!request.system.contains(skill_body));
        assert!(!evidence.content.contains(skill_body));
        let catalog = request
            .system
            .split_once("## Optional skill catalog")
            .unwrap()
            .1;
        assert!(
            crate::budget::count_tokens(&format!("## Optional skill catalog{catalog}")) <= 2048
        );
        let workflow = request
            .system
            .strip_prefix(baseline.system())
            .unwrap()
            .split_once("## Optional skill catalog")
            .unwrap()
            .0
            .trim();
        assert!(crate::budget::count_tokens(workflow) <= crate::workflow::WORKFLOW_TOKEN_LIMIT);
        assert!(baseline.tokens() <= crate::budget::BUDGET_LIMIT);
        for wire in [
            polaris_provider::openai::tool_wire_shape(baseline.tools()),
            polaris_provider::codex::tool_wire_shape(baseline.tools()),
        ] {
            assert!(
                crate::budget::count_tokens(baseline.system())
                    + crate::budget::count_tokens(&serde_json::to_string(&wire).unwrap())
                    <= crate::budget::BUDGET_LIMIT
            );
        }
        let components = crate::budget::request_context_tokens(request);
        assert!(components.openai_state <= crate::budget::STATE_SCHEMA_LIMIT);
        assert!(components.codex_state <= crate::budget::STATE_SCHEMA_LIMIT);
        assert_eq!(request.tools.len(), 7);
        assert_eq!(
            request.tools[..6]
                .iter()
                .map(|tool| tool.name.as_str())
                .collect::<Vec<_>>(),
            base_names
        );
        assert_eq!(request.tools[6].name, "state");
        let openai = polaris_provider::openai::build_body("fixture", request, Some("medium"));
        let codex = polaris_provider::codex::build_body("fixture", request, Some("medium"));
        assert_eq!(openai["messages"][0]["content"], request.system);
        assert_eq!(codex["instructions"], request.system);
        let openai_messages = openai["messages"].as_array().unwrap();
        let codex_items = codex["input"].as_array().unwrap();
        // Compare real user/evidence ordering without equating provider array lengths.
        let expected_users: Vec<_> = request
            .messages
            .iter()
            .filter(|m| m.role == Role::User)
            .map(|m| m.content.as_str())
            .collect();
        assert_eq!(
            openai_messages
                .iter()
                .filter(|m| m["role"] == "user")
                .map(|m| m["content"].as_str().unwrap())
                .collect::<Vec<_>>(),
            expected_users
        );
        assert_eq!(
            codex_items
                .iter()
                .filter(|m| m["role"] == "user")
                .map(|m| m["content"][0]["text"].as_str().unwrap())
                .collect::<Vec<_>>(),
            expected_users
        );
        if index == 1 {
            let n = openai_messages.len();
            assert_eq!(
                openai_messages[n - 2]["tool_calls"][0]["id"],
                "load-deploy-guide"
            );
            assert_eq!(openai_messages[n - 1]["tool_call_id"], "load-deploy-guide");
            let n = codex_items.len();
            assert_eq!(codex_items[n - 2]["type"], "function_call");
            assert_eq!(codex_items[n - 1]["type"], "function_call_output");
            assert_eq!(codex_items[n - 2]["call_id"], "load-deploy-guide");
            assert_eq!(codex_items[n - 1]["call_id"], "load-deploy-guide");
            assert_eq!(codex_items[n - 1]["output"], result.content);
        }
        for (transport, body) in [("openai", openai), ("codex", codex)] {
            let full = crate::budget::count_tokens(&body.to_string());
            assert!(full > reported_tokens as usize);
            println!(
                "structured_wire transport={transport} request={} full_reference_tokens={full} evidence_reference_tokens={reported_tokens} tool_result_reference_tokens={}",
                index + 1,
                if index == 1 {
                    crate::budget::count_tokens(&result.content)
                } else {
                    0
                }
            );
        }
    }
    assert_eq!(requests[0].system, requests[1].system);
    assert_eq!(
        polaris_provider::cache_key("fixture", Some("medium"), &requests[0]),
        polaris_provider::cache_key("fixture", Some("medium"), &requests[1])
    );
    assert_eq!(
        serde_json::to_value(requests[1].messages.last().unwrap()).unwrap(),
        serde_json::to_value(Message::tool_result("load-deploy-guide", &result.content)).unwrap()
    );
}
