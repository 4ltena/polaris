//! Authored action decisions exercise agent/tool wiring, not semantic classification.
//! The default uses a synthetic helper; an explicit ignored variant uses a separately
//! built production helper. Never derive decisions from fixture fields in product code.

use super::*;
use polaris_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError, ToolCall};
use std::path::{Path, PathBuf};

struct ScriptedAction {
    calls: AtomicUsize,
    path: PathBuf,
    write_content: Option<String>,
    required_request_text: Vec<String>,
}

#[async_trait::async_trait]
impl Provider for ScriptedAction {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        for required in &self.required_request_text {
            assert!(
                request
                    .messages
                    .iter()
                    .any(|message| message.content.contains(required)),
                "authored source/evidence was absent from the actual model request"
            );
        }
        assert!(request.tools.iter().any(|tool| tool.name == "write"));
        if let Some(content) = &self.write_content {
            if call == 0 {
                return Ok(CompletionResponse {
                    tool_calls: vec![ToolCall {
                        id: "m12-setting-write".into(),
                        name: "write".into(),
                        arguments: json!({"path": self.path, "content": content}),
                    }],
                    ..Default::default()
                });
            }
            assert_eq!(call, 1, "unexpected provider continuation");
            let result = request
                .messages
                .iter()
                .find(|message| {
                    message.role == polaris_provider::Role::Tool
                        && message.tool_call_id.as_deref() == Some("m12-setting-write")
                })
                .expect("write result was absent from the continuation");
            assert!(result.content.contains("wrote"), "{}", result.content);
        } else {
            assert_eq!(
                call, 0,
                "clarification must not request a tool continuation"
            );
        }
        Ok(CompletionResponse {
            text: if self.write_content.is_some() {
                "合成設定を更新しました。"
            } else {
                "3回の決定は撤回済みで後継値は未定です。設定を変えず、新しい値を確認します。"
            }
            .into(),
            ..Default::default()
        })
    }
}

// Same confined synthetic-helper approach as agent::tests::write_helper.
// A successful dispatch must actually modify the supplied temporary file;
// this does not establish production helper correctness.
#[cfg(unix)]
fn synthetic_write_helper(directory: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    let path = directory.join("helper.sh");
    std::fs::write(
        &path,
        r#"#!/bin/sh
python3 -c '
import json,sys
m=json.load(sys.stdin)
open(m["path"],"w").write(m["content"])
print("wrote")
'
"#,
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

struct ActionRun {
    session: crate::session::Session,
    events: Vec<crate::AgentEvent>,
    audit: Vec<Value>,
}

async fn run_action(
    writer: &mut Writer,
    target: &RunTarget,
    database: &Path,
    history: Arc<StrictHistory>,
    provider: Arc<ScriptedAction>,
    helper: &Path,
    root: &Path,
) -> ActionRun {
    let (runtime, mut owner) = crate::desktop_memory::DesktopMemory::pair(
        target.clone(),
        history,
        database.to_owned(),
        resources(),
    )
    .unwrap();
    let audit_path = root.join("action-audit.jsonl");
    let audit = Arc::new(tokio::sync::Mutex::new(
        crate::audit::AuditLog::open(&audit_path).unwrap(),
    ));
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let task = async {
        let mut session = crate::session::Session::new();
        session.desktop_memory = Some(runtime);
        session.disable_files_md_auto_regenerate = true;
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.to_path_buf()],
        )
        .unwrap();
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = crate::agent::AutoApprove;
        let mut tools = crate::agent::ToolContext {
            sandbox: &sandbox,
            helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        crate::agent::run(
            provider.as_ref(),
            &mut session,
            audit,
            &mut crate::stop::StopTracker::new(3),
            &crate::prompt::assemble_always_on("", "", &[]),
            &[],
            &[],
            provider.clone(),
            1,
            1,
            Some(events),
            &mut tools,
        )
        .await
        .unwrap();
        session
    };
    tokio::pin!(task);
    let session = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        loop {
            tokio::select! {
                result = &mut task => break result,
                _ = tokio::task::yield_now() => owner.poll(Some(writer), false),
            }
        }
    })
    .await
    .unwrap();
    let mut events = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        events.push(event);
    }
    let audit = std::fs::read_to_string(audit_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    ActionRun {
        session,
        events,
        audit,
    }
}

fn assert_action_observation(run: &ActionRun, writer: &Writer) {
    let observation = run.session.memory_observation.as_ref().unwrap();
    let saved = writer.snapshot().unwrap();
    let status = saved.state.runs.last().unwrap().memory.as_ref().unwrap();
    assert_eq!(observation.retrieval_status, status.retrieval_status);
    assert_eq!(observation.source_uris, status.retrieval_sources);
    assert_eq!(observation.reference_tokens, status.reference_tokens.get());
    let events: Vec<_> = run
        .events
        .iter()
        .filter_map(|event| {
            if let crate::AgentEvent::MemoryPrepared(observation) = event {
                Some(observation)
            } else {
                None
            }
        })
        .collect();
    assert_eq!(events, vec![observation]);
}

fn tool_audit(run: &ActionRun) -> Vec<&Value> {
    let names: Vec<_> = polaris_tools::all_specs()
        .into_iter()
        .map(|s| s.name)
        .collect();
    run.audit
        .iter()
        .filter(|row| names.iter().any(|name| row["tool"] == name.as_str()))
        .collect()
}

#[cfg(unix)]
#[tokio::test]
async fn authored_t4_positive_control_changes_only_synthetic_copy_through_agent_write() {
    verify_positive_action(None).await;
}

#[cfg(unix)]
#[tokio::test]
#[ignore = "POLARIS_M12_HELPERに事前ビルドした製品helperを明示して実行する"]
async fn authored_t4_positive_control_uses_built_production_helper() {
    let helper = PathBuf::from(
        std::env::var_os("POLARIS_M12_HELPER")
            .expect("POLARIS_M12_HELPERに事前ビルドしたhelperを指定してください"),
    );
    assert!(helper.is_absolute() && helper.is_file());
    verify_positive_action(Some(helper)).await;
}

#[cfg(unix)]
async fn verify_positive_action(production_helper: Option<PathBuf>) {
    let fixture = fixture();
    let (_store_root, mut writer) = new_writer();
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    MemoryStore::open(&database)
        .unwrap()
        .enable_structured_memory()
        .unwrap();
    let control = &fixture["positive_control"];
    for turn in 1..=control["branch_after_turn"].as_u64().unwrap() {
        let target = accept_text(
            &mut writer,
            turn,
            fixture["inputs"][(turn - 1) as usize]["text"]
                .as_str()
                .unwrap(),
        );
        finish(&mut writer, &target, turn);
    }
    let target = accept_text(
        &mut writer,
        control["query_turn"].as_u64().unwrap(),
        control["query"].as_str().unwrap(),
    );
    writer.bind_memory(&target, resources()).unwrap();
    let summary = Arc::new(AnnotatedSummary {
        fixture: fixture.clone(),
        calls: AtomicUsize::new(0),
    });
    let embed = Arc::new(CountedEmbed {
        model: EmbeddingModel {
            model: "fixture".into(),
            revision: "fixed".into(),
            dimension: 2,
        },
        passages: AtomicUsize::new(0),
    });
    let copy = tempfile::tempdir().unwrap();
    let path = copy.path().join(
        fixture["setting_fixture"]["relative_path"]
            .as_str()
            .unwrap(),
    );
    std::fs::write(
        &path,
        fixture["setting_fixture"]["initial_content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let before = content_hash(&std::fs::read(&path).unwrap());
    assert_eq!(before, fixture["setting_fixture"]["initial_sha256"]);
    let helper_dir = tempfile::tempdir().unwrap();
    let helper_kind = if production_helper.is_some() {
        "production"
    } else {
        "synthetic"
    };
    let helper = production_helper.unwrap_or_else(|| synthetic_write_helper(helper_dir.path()));
    let provider = Arc::new(ScriptedAction {
        calls: AtomicUsize::new(0),
        path: path.clone(),
        write_content: Some(control["expected_content"].as_str().unwrap().into()),
        required_request_text: fixture["inputs"].as_array().unwrap()[..4]
            .iter()
            .map(|input| input["text"].as_str().unwrap().into())
            .chain(std::iter::once(control["query"].as_str().unwrap().into()))
            .collect(),
    });
    let run = run_action(
        &mut writer,
        &target,
        &database,
        Arc::new(StrictHistory::new(summary.clone(), embed.clone())),
        provider.clone(),
        &helper,
        copy.path(),
    )
    .await;
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        control["expected_content"].as_str().unwrap()
    );
    assert_ne!(content_hash(&std::fs::read(&path).unwrap()), before);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 2);
    assert_eq!(summary.calls.load(Ordering::SeqCst), 0);
    assert_eq!(embed.passages.load(Ordering::SeqCst), 0);
    assert_action_observation(&run, &writer);
    let started: Vec<_> = run
        .events
        .iter()
        .filter_map(|event| {
            if let crate::AgentEvent::ToolStarted { name, .. } = event {
                Some(name.as_str())
            } else {
                None
            }
        })
        .collect();
    assert_eq!(started, ["write"]); // No external-call/bash/spawn dispatch.
    assert!(run.events.iter().any(|event| matches!(event,
        crate::AgentEvent::ToolFinished { name, ok: true, .. } if name == "write")));
    let rows = tool_audit(&run);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["tool"], "write");
    let snapshot = writer.memory_snapshot(&target).unwrap();
    println!(
        "authored_m12_positive {}",
        json!({
            "fixture_sha": content_hash(FIXTURE.as_bytes()),
            "raw_sha": snapshot.conversation.raw_hash,
            "generation": snapshot.conversation.state.generation,
            "status": run.session.memory_observation.as_ref().unwrap().retrieval_status,
            "sources": run.session.memory_observation.as_ref().unwrap().source_uris,
            "file_before_sha": before,
            "file_after_sha": content_hash(&std::fs::read(&path).unwrap()),
            "main_calls": provider.calls.load(Ordering::SeqCst),
            "summary_calls": summary.calls.load(Ordering::SeqCst),
            "passage_calls": embed.passages.load(Ordering::SeqCst),
            "write_calls": rows.len(), "external_tool_calls": 0,
        "semantic_classification_measured": false, "helper_kind": helper_kind
        })
    );
}

/// Called after each existing independently seeded withdrawal probe prepares.
/// The scripted clarification is a supplied action decision, not an assertion
/// that the model classified withdrawal or that the host prohibits all writes.
pub(super) async fn verify_withdrawal_action(
    writer: &mut Writer,
    target: &RunTarget,
    database: &Path,
    summary: Arc<AnnotatedSummary>,
    embed: Arc<CountedEmbed>,
    fixture: &Value,
    evidence: &str,
) {
    assert!(evidence.contains("Aster") && evidence.contains("sync.retry_limit"));
    assert!(
        evidence.contains("withdrawn_without_replacement")
            && evidence.contains("needs_reconfirmation")
    );
    let copy = tempfile::tempdir().unwrap();
    let path = copy.path().join(
        fixture["setting_fixture"]["relative_path"]
            .as_str()
            .unwrap(),
    );
    std::fs::write(
        &path,
        fixture["setting_fixture"]["initial_content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let before = content_hash(&std::fs::read(&path).unwrap());
    assert_eq!(before, fixture["setting_fixture"]["initial_sha256"]);
    let prior_summary = summary.calls.load(Ordering::SeqCst);
    let prior_embedding = embed.passages.load(Ordering::SeqCst);
    let provider = Arc::new(ScriptedAction {
        calls: AtomicUsize::new(0),
        path: path.clone(),
        write_content: None,
        required_request_text: vec![evidence.into(), fixture["query"].as_str().unwrap().into()],
    });
    let run = run_action(
        writer,
        target,
        database,
        Arc::new(StrictHistory::new(summary.clone(), embed.clone())),
        provider.clone(),
        Path::new("/must-not-run-m12"),
        copy.path(),
    )
    .await;
    assert_eq!(content_hash(&std::fs::read(&path).unwrap()), before);
    assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    assert_eq!(summary.calls.load(Ordering::SeqCst), prior_summary);
    assert_eq!(embed.passages.load(Ordering::SeqCst), prior_embedding);
    assert_action_observation(&run, writer);
    assert!(!run.events.iter().any(|event| matches!(
        event,
        crate::AgentEvent::ToolStarted { .. }
            | crate::AgentEvent::ToolFinished { .. }
            | crate::AgentEvent::SpawnStarted { .. }
    )));
    assert!(tool_audit(&run).is_empty());
    assert!(
        run.session
            .messages
            .iter()
            .all(|message| message.tool_calls.is_empty())
    );
    let snapshot = writer.memory_snapshot(target).unwrap();
    println!(
        "authored_m12_withdrawal {}",
        json!({
            "fixture_sha": content_hash(FIXTURE.as_bytes()),
            "raw_sha": snapshot.conversation.raw_hash,
            "generation": snapshot.conversation.state.generation,
            "status": run.session.memory_observation.as_ref().unwrap().retrieval_status,
            "sources": run.session.memory_observation.as_ref().unwrap().source_uris,
            "file_before_sha": before,
            "file_after_sha": content_hash(&std::fs::read(&path).unwrap()),
            "main_calls": provider.calls.load(Ordering::SeqCst),
            "summary_calls": summary.calls.load(Ordering::SeqCst),
            "passage_calls": embed.passages.load(Ordering::SeqCst),
            "write_calls": 0, "external_tool_calls": 0,
            "semantic_classification_measured": false, "host_global_write_block_verified": false
        })
    );
}
