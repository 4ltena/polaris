//! The `spawn` tool's implementation. Type discovery reuses
//! `polaris_tools::skill::lookup` (Task 1/3), and execution itself calls
//! `agent::run_loop`. Parallelism will be contained entirely within this
//! file; `agent::dispatch`'s own outer loop stays sequential.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::Mutex;

use polaris_provider::Provider;
use polaris_sandbox::{SandboxMode, SandboxPolicy};
use polaris_skills::{AgentAccess, AgentType};

use crate::agent::{AutoApprove, ToolContext, run_loop};
use crate::approval::{ApprovalPolicy, Gate};
use crate::audit::AuditLog;
use crate::session::Session;
use crate::stop::StopTracker;

#[derive(Debug, Clone)]
pub struct SpawnTask {
    pub agent_type: String,
    pub task: String,
    pub write_root: Option<String>,
}

#[derive(Debug)]
pub enum TaskOutcome {
    Ok(String),
    Failed(String),
}

/// Runs one task against its resolved type definition. When the type
/// cannot be found, the error carries the same candidate list
/// `polaris_tools::skill::lookup` produces — the failure response follows
/// the router's pattern, not just its search implementation.
pub async fn run_one(
    task: &SpawnTask,
    agent_types: &[AgentType],
    provider: Arc<dyn Provider>,
    audit: Arc<Mutex<AuditLog>>,
    base_sandbox: &SandboxPolicy,
    helper: &Path,
) -> TaskOutcome {
    let Some(agent) = agent_types.iter().find(|a| a.name == task.agent_type) else {
        let candidates = polaris_tools::skill::lookup(agent_types, &task.agent_type);
        return TaskOutcome::Failed(format!(
            "unknown subagent type {:?}. {candidates}",
            task.agent_type
        ));
    };

    // Depth 1: whatever `allowed-tools` says, `spawn` is excluded
    // unconditionally. This is the only thing that makes the empty
    // `agent_types` handed to `run_loop` below correct rather than merely
    // convenient — the subagent's own loop can never reach the `spawn`
    // arm to consult it.
    let mut tools: Vec<polaris_tools::ToolSpec> = polaris_tools::all_specs()
        .into_iter()
        .filter(|t| t.name != "spawn" && agent.allowed_tools.iter().any(|a| a == t.name))
        .collect();
    tools.sort_by_key(|t| t.name);

    let sandbox = match resolve_subagent_sandbox(agent, task, base_sandbox) {
        Ok(s) => s,
        Err(e) => return TaskOutcome::Failed(e),
    };

    let mut session = Session::new();
    session.push_user(&task.task);
    let mut stop = StopTracker::with_wall_seconds(agent.max_turns, agent.wall_seconds);
    let mut gate = Gate::new(ApprovalPolicy::Never);
    let mut approver = AutoApprove;
    let mut ctx = ToolContext {
        sandbox: &sandbox,
        helper,
        gate: &mut gate,
        approver: &mut approver,
    };

    let result = run_loop(
        provider.as_ref(),
        &mut session,
        audit.clone(),
        &mut stop,
        &agent.body,
        &tools,
        &[],
        // A subagent can never spawn (depth is fixed at 1), so there is
        // no type catalog to resolve against, and its own provider /
        // audit handles are simply the ones it was given.
        &[],
        provider.clone(),
        &agent.name,
        &mut ctx,
    )
    .await;

    match result {
        Ok(outcome) => match validate_output(agent, &outcome.text) {
            Ok(()) => TaskOutcome::Ok(outcome.text),
            Err(first_err) => {
                // Retry exactly once, with the validation error attached.
                session.push_user(&format!(
                    "Your previous output did not match the required schema: {first_err}. \
                     Return output matching the schema exactly."
                ));
                let mut stop2 = StopTracker::with_wall_seconds(agent.max_turns, agent.wall_seconds);
                let mut gate2 = Gate::new(ApprovalPolicy::Never);
                let mut approver2 = AutoApprove;
                let mut ctx2 = ToolContext {
                    sandbox: &sandbox,
                    helper,
                    gate: &mut gate2,
                    approver: &mut approver2,
                };
                let retry = run_loop(
                    provider.as_ref(),
                    &mut session,
                    audit.clone(),
                    &mut stop2,
                    &agent.body,
                    &tools,
                    &[],
                    &[],
                    provider.clone(),
                    &agent.name,
                    &mut ctx2,
                )
                .await;
                match retry {
                    Ok(outcome2) => match validate_output(agent, &outcome2.text) {
                        Ok(()) => TaskOutcome::Ok(outcome2.text),
                        Err(second_err) => TaskOutcome::Failed(format!(
                            "schema mismatch after retry: {second_err}"
                        )),
                    },
                    Err(e) => TaskOutcome::Failed(e.to_string()),
                }
            }
        },
        Err(e) => TaskOutcome::Failed(e.to_string()),
    }
}

fn validate_output(agent: &AgentType, text: &str) -> Result<(), String> {
    let schema_text = std::fs::read_to_string(&agent.output_schema).map_err(|e| {
        format!(
            "cannot read output schema {}: {e}",
            agent.output_schema.display()
        )
    })?;
    let schema: serde_json::Value = serde_json::from_str(&schema_text)
        .map_err(|e| format!("output schema is not valid JSON: {e}"))?;
    let instance: serde_json::Value = serde_json::from_str(text)
        .map_err(|e| format!("subagent output is not valid JSON: {e}"))?;
    polaris_tools::validate(&schema, &instance)
}

/// Maps a type's `access` onto an actual sandbox policy. For read-write,
/// the `write_root` the task declared is confined to what the parent's own
/// writable roots already cover — the invariant checked here is that no
/// route exists by which a subagent obtains more authority than its parent.
fn resolve_subagent_sandbox(
    agent: &AgentType,
    task: &SpawnTask,
    base_sandbox: &SandboxPolicy,
) -> Result<SandboxPolicy, String> {
    match agent.access {
        AgentAccess::Read => SandboxPolicy::new(SandboxMode::ReadOnly, &[])
            .map_err(|e| format!("cannot build read-only sandbox: {e}")),
        AgentAccess::ReadWrite => {
            let Some(root) = &task.write_root else {
                return Err(format!(
                    "{} is a read-write type and requires write_root",
                    agent.name
                ));
            };
            let root_path = PathBuf::from(root);
            let canonical = root_path
                .canonicalize()
                .map_err(|e| format!("write_root {root} does not exist: {e}"))?;
            if !base_sandbox.contains(&canonical) {
                return Err(format!(
                    "write_root {root} is outside the parent's own writable roots"
                ));
            }
            SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[canonical])
                .map_err(|e| format!("cannot build subagent sandbox: {e}"))
        }
    }
}

/// One wave. Task 9 runs the wave sequentially; Task 10 parallelizes it
/// and adds write-target collision checking.
pub async fn run_wave(
    tasks: Vec<SpawnTask>,
    agent_types: &[AgentType],
    provider: Arc<dyn Provider>,
    audit: Arc<Mutex<AuditLog>>,
    base_sandbox: &SandboxPolicy,
    helper: &Path,
) -> String {
    let mut lines = Vec::with_capacity(tasks.len());
    for task in &tasks {
        let outcome = run_one(
            task,
            agent_types,
            provider.clone(),
            audit.clone(),
            base_sandbox,
            helper,
        )
        .await;
        match outcome {
            TaskOutcome::Ok(text) => lines.push(format!("{}: {text}", task.agent_type)),
            TaskOutcome::Failed(msg) => lines.push(format!("{}: FAILED — {msg}", task.agent_type)),
        }
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_provider::{CompletionRequest, CompletionResponse, ToolCall};

    /// Same shape as `agent::tests::Scripted`: hands back the queued
    /// responses in order, then an empty one.
    struct Scripted {
        replies: std::sync::Mutex<Vec<CompletionResponse>>,
    }

    #[async_trait::async_trait]
    impl polaris_provider::Provider for Scripted {
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
            let mut r = self.replies.lock().expect("lock");
            Ok(if r.is_empty() {
                CompletionResponse::default()
            } else {
                r.remove(0)
            })
        }
    }

    fn scripted(replies: Vec<CompletionResponse>) -> Arc<dyn Provider> {
        Arc::new(Scripted {
            replies: std::sync::Mutex::new(replies),
        })
    }

    fn text(s: &str) -> CompletionResponse {
        CompletionResponse {
            text: s.to_string(),
            tool_calls: vec![],
            ..Default::default()
        }
    }

    fn read_call(path: &Path) -> CompletionResponse {
        CompletionResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "read".into(),
                arguments: serde_json::json!({ "path": path.to_str().expect("path") }),
            }],
            ..Default::default()
        }
    }

    /// The real `agents/file-inspector` from this repository — the type
    /// definition and the JSON Schema shipped alongside it, not a copy
    /// written inside the test. Nothing else exercises those two files, so
    /// a copy here would leave them free to rot.
    fn file_inspector() -> AgentType {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../agents/file-inspector");
        let text = std::fs::read_to_string(dir.join("SKILL.md")).expect("cannot read SKILL.md");
        polaris_skills::agent_type::parse(&text, "file-inspector", &dir)
            .expect("agents/file-inspector does not parse")
    }

    fn audit_in(dir: &Path) -> Arc<Mutex<AuditLog>> {
        Arc::new(Mutex::new(
            AuditLog::open(&dir.join("audit.jsonl")).expect("cannot open"),
        ))
    }

    fn full_access() -> SandboxPolicy {
        SandboxPolicy::new(SandboxMode::FullAccess, &[]).expect("policy")
    }

    #[tokio::test]
    async fn a_subagent_that_uses_a_tool_and_returns_matching_json_succeeds() {
        let dir = tempfile::tempdir().expect("temp directory");
        let target = dir.path().join("a.rs");
        std::fs::write(&target, "fn main() {}\n").expect("cannot write");

        let result = serde_json::json!({
            "path": target.to_str().expect("path"),
            "responsibility": "The entry point.",
            "test_file": null
        })
        .to_string();

        let provider = scripted(vec![read_call(&target), text(&result)]);
        let audit = audit_in(dir.path());
        let task = SpawnTask {
            agent_type: "file-inspector".into(),
            task: format!("inspect {}", target.display()),
            write_root: None,
        };

        let outcome = run_one(
            &task,
            &[file_inspector()],
            provider,
            audit,
            &full_access(),
            Path::new("/bin/true"),
        )
        .await;

        let body = match outcome {
            TaskOutcome::Ok(b) => b,
            TaskOutcome::Failed(e) => panic!("the subagent failed: {e}"),
        };
        assert_eq!(body, result);

        // The subagent's own tool call has to reach the same audit log,
        // attributed to the subagent rather than to the root — that
        // attribution is the whole point of `Record::caller` (Task 4).
        let log =
            std::fs::read_to_string(dir.path().join("audit.jsonl")).expect("cannot read the log");
        assert!(
            log.contains("\"tool\":\"read\""),
            "the subagent's read was not recorded: {log}"
        );
        assert!(
            log.contains("\"caller\":\"file-inspector\""),
            "the record is not attributed to the subagent: {log}"
        );
    }

    #[tokio::test]
    async fn output_that_never_matches_the_schema_fails_after_exactly_one_retry() {
        let dir = tempfile::tempdir().expect("temp directory");
        // `responsibility` is required and missing from both replies. Only
        // two are queued: if a third attempt were ever made, `Scripted`
        // would hand back an empty response and the failure text would
        // name a JSON parse error instead of a schema mismatch.
        let provider = scripted(vec![text(r#"{"path":"a.rs"}"#), text(r#"{"path":"a.rs"}"#)]);
        let task = SpawnTask {
            agent_type: "file-inspector".into(),
            task: "inspect a.rs".into(),
            write_root: None,
        };

        let outcome = run_one(
            &task,
            &[file_inspector()],
            provider,
            audit_in(dir.path()),
            &full_access(),
            Path::new("/bin/true"),
        )
        .await;

        match outcome {
            TaskOutcome::Failed(e) => {
                assert!(
                    e.starts_with("schema mismatch after retry"),
                    "the failure did not come from the retried validation: {e}"
                );
                assert!(
                    e.contains("responsibility"),
                    "the reason does not name the missing field: {e}"
                );
            }
            TaskOutcome::Ok(b) => panic!("output not matching the schema was accepted: {b}"),
        }
    }

    #[tokio::test]
    async fn an_unknown_type_fails_with_the_same_candidate_list_the_router_produces() {
        let dir = tempfile::tempdir().expect("temp directory");
        let task = SpawnTask {
            agent_type: "file-inspecter".into(),
            task: "inspect a.rs".into(),
            write_root: None,
        };
        let outcome = run_one(
            &task,
            &[file_inspector()],
            scripted(vec![]),
            audit_in(dir.path()),
            &full_access(),
            Path::new("/bin/true"),
        )
        .await;
        match outcome {
            TaskOutcome::Failed(e) => {
                assert!(e.contains("unknown subagent type"), "{e}");
                assert!(
                    e.contains("file-inspector"),
                    "the candidate list is missing the type that does exist: {e}"
                );
            }
            TaskOutcome::Ok(b) => panic!("an unknown type was run anyway: {b}"),
        }
    }

    fn read_write_type() -> AgentType {
        let mut a = file_inspector();
        a.name = "scribe".into();
        a.access = AgentAccess::ReadWrite;
        a.allowed_tools = vec!["read".into(), "write".into()];
        a
    }

    #[tokio::test]
    async fn a_read_write_type_without_a_write_root_is_refused_before_the_provider_is_called() {
        let dir = tempfile::tempdir().expect("temp directory");
        let task = SpawnTask {
            agent_type: "scribe".into(),
            task: "write a.rs".into(),
            write_root: None,
        };
        // `scripted(vec![])` would answer with an empty response rather
        // than panicking, so this is pinned by the message instead: the
        // refusal has to name `write_root`.
        let outcome = run_one(
            &task,
            &[read_write_type()],
            scripted(vec![]),
            audit_in(dir.path()),
            &full_access(),
            Path::new("/bin/true"),
        )
        .await;
        match outcome {
            TaskOutcome::Failed(e) => assert!(e.contains("write_root"), "{e}"),
            TaskOutcome::Ok(b) => panic!("a read-write type ran with no write root: {b}"),
        }
    }

    #[tokio::test]
    async fn a_write_root_outside_the_parent_s_own_roots_is_refused() {
        // The invariant: a subagent must not be able to reach a place its
        // parent could not write to itself. The parent here can only write
        // under `inside`, and the task asks for `outside`.
        let inside = tempfile::tempdir().expect("temp directory");
        let outside = tempfile::tempdir().expect("temp directory");
        let base = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[inside.path().to_path_buf()])
            .expect("policy");

        let task = SpawnTask {
            agent_type: "scribe".into(),
            task: "write a.rs".into(),
            write_root: Some(outside.path().display().to_string()),
        };
        let outcome = run_one(
            &task,
            &[read_write_type()],
            scripted(vec![]),
            audit_in(inside.path()),
            &base,
            Path::new("/bin/true"),
        )
        .await;
        match outcome {
            TaskOutcome::Failed(e) => {
                assert!(e.contains("outside the parent's own writable roots"), "{e}")
            }
            TaskOutcome::Ok(b) => panic!("a subagent was granted a root its parent lacks: {b}"),
        }
    }

    #[tokio::test]
    async fn a_subagent_is_never_offered_the_spawn_tool() {
        // Depth is fixed at 1. Ask for a type whose `allowed-tools`
        // explicitly names `spawn`, and confirm the tool list the loop is
        // handed still does not contain it. Read the list back off the
        // request the provider actually received — a subagent that cannot
        // see the tool cannot call it, whatever its SKILL.md claims.
        struct CapturesTools {
            seen: std::sync::Mutex<Vec<Vec<String>>>,
        }
        #[async_trait::async_trait]
        impl polaris_provider::Provider for CapturesTools {
            async fn complete(
                &self,
                req: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                self.seen
                    .lock()
                    .expect("lock")
                    .push(req.tools.iter().map(|t| t.name.to_string()).collect());
                Ok(CompletionResponse {
                    text: r#"{"path":"a.rs","responsibility":"x"}"#.into(),
                    tool_calls: vec![],
                    ..Default::default()
                })
            }
        }

        let dir = tempfile::tempdir().expect("temp directory");
        let provider = Arc::new(CapturesTools {
            seen: std::sync::Mutex::new(vec![]),
        });
        let mut agent = file_inspector();
        agent.allowed_tools = vec!["read".into(), "spawn".into()];

        let task = SpawnTask {
            agent_type: "file-inspector".into(),
            task: "inspect a.rs".into(),
            write_root: None,
        };
        let outcome = run_one(
            &task,
            &[agent],
            provider.clone(),
            audit_in(dir.path()),
            &full_access(),
            Path::new("/bin/true"),
        )
        .await;
        assert!(matches!(outcome, TaskOutcome::Ok(_)), "{outcome:?}");

        let seen = provider.seen.lock().expect("lock");
        let offered = seen.first().expect("the provider was never called");
        assert!(
            !offered.iter().any(|n| n == "spawn"),
            "a subagent was offered the spawn tool: {offered:?}"
        );
        assert!(
            offered.iter().any(|n| n == "read"),
            "the rest of allowed-tools was dropped too: {offered:?}"
        );
    }
}
