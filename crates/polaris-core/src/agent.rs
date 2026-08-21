//! The agent loop. Returns the body text at the point tool calls stop.
//!
//! When an assistant turn calls a tool, that turn itself is recorded to the
//! session with its `tool_calls` before each tool is executed. OpenAI's
//! round-trip protocol requires this assistant message to already be
//! present in the history before the following `role: "tool"` messages —
//! get the order wrong and the next turn's send is rejected by the API.

use std::path::{Path, PathBuf};

use polaris_provider::{CompletionRequest, Provider};

use crate::audit::{AuditLog, Record};
use crate::session::Session;
use crate::stop::{StopReason, StopTracker};

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("stopped: {0:?}")]
    Stopped(StopReason),
    #[error("provider: {0}")]
    Provider(#[from] polaris_provider::ProviderError),
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
}

/// The context a mutating operation needs. `agent::run` receives it and
/// hands it to `dispatch`.
///
/// `sandbox` and `helper` are needed by all of `write` / `edit` / `bash`.
/// `gate` and `approver` are used only by `write` / `edit` (`bash` does not
/// go through the predicate — see the `crate::approval` docs, and the
/// per-tool behavior explanation near the top of this file).
pub struct ToolContext<'a> {
    pub sandbox: &'a polaris_sandbox::SandboxPolicy,
    pub helper: &'a Path,
    pub gate: &'a mut crate::approval::Gate,
    pub approver: &'a mut dyn crate::approval::Approver,
}

/// Pass in `always_on` as something [`crate::prompt::assemble_always_on`]
/// has already assembled. It is not assembled inside the loop, so that the
/// caller is made to guarantee the same string is sent every turn and the
/// cache prefix never shifts.
///
/// This takes [`crate::prompt::AlwaysOn`] rather than a string and a
/// `Vec<ToolSpec>` taken separately, so that what gets loaded every turn
/// cannot be built outside this type. If the caller could append to an
/// already-assembled string, the ceiling on the always-on context would be
/// left up to how the caller happens to write it.
///
/// `skills` is not loaded into the always-on context. It's only consulted
/// when the `skill` tool is invoked.
/// What a successful turn produced: the final text, and the token usage
/// accumulated across every `provider.complete()` call the turn made
/// (a turn that used a tool calls the provider more than once). A
/// response whose `usage` came back `None` contributes nothing to this
/// total rather than failing the turn — usage is a best-effort report,
/// never something the loop depends on to function.
#[derive(Debug)]
pub struct AgentOutcome {
    pub text: String,
    pub usage: polaris_provider::Usage,
}

pub async fn run(
    provider: &dyn Provider,
    session: &mut Session,
    audit: &mut AuditLog,
    stop: &mut StopTracker,
    always_on: &crate::prompt::AlwaysOn,
    skills: &[polaris_skills::Skill],
    ctx: &mut ToolContext<'_>,
) -> Result<AgentOutcome, AgentError> {
    let mut usage = polaris_provider::Usage::default();
    loop {
        // Call unconditionally every turn. If this were only called on
        // error, a call pattern that never triggers an error would never
        // trip MaxTurns, and the loop could run forever.
        if let Some(r) = stop.observe_turn() {
            return Err(AgentError::Stopped(r));
        }

        let res = provider
            .complete(CompletionRequest {
                system: always_on.system().to_string(),
                messages: session.messages.clone(),
                tools: always_on.tools().to_vec(),
            })
            .await?;

        if let Some(u) = res.usage {
            usage.input_tokens += u.input_tokens;
            usage.output_tokens += u.output_tokens;
            usage.total_tokens += u.total_tokens;
        }

        if res.tool_calls.is_empty() {
            session.push_assistant(&res.text);
            return Ok(AgentOutcome {
                text: res.text,
                usage,
            });
        }

        // Before sending the tool results, record this assistant turn
        // itself to the history with its tool_calls. Skip this and push
        // only the tool results, and on the next send the API side has
        // nothing to match "which call this result answers" against — a
        // real OpenAI endpoint rejects it (tests against a mock never
        // inspect the wire format, so they cannot detect this omission).
        session.push_assistant_tool_calls(&res.text, res.tool_calls.clone());

        for call in &res.tool_calls {
            let outcome = dispatch(call, skills, ctx);
            // `result` is exactly the body actually returned to the model
            // (on success) or the error text (on failure).
            let result: &str = match &outcome {
                Ok(body) => body.as_str(),
                Err(msg) => msg.as_str(),
            };
            // `sandbox` and `target` are only filled in for `write` /
            // `edit`. Both are tools whose target path resolves to exactly
            // one value, and each one's public schema declares that
            // argument under the name "path"
            // (`polaris_tools::write_spec` / `edit_spec`). This is filled
            // in the same way whether the record is a success or a
            // failure — if a denied attempt couldn't leave behind what it
            // was trying to touch, the reconstructability that is the
            // whole point of the audit log would be missing precisely on
            // failure.
            let is_mutation = call.name == "write" || call.name == "edit";
            let target: Option<PathBuf> = if is_mutation {
                call.arguments["path"].as_str().map(PathBuf::from)
            } else {
                None
            };
            audit.record(&Record {
                tool: &call.name,
                detail: &call.arguments.to_string(),
                sandbox: is_mutation.then_some(ctx.sandbox),
                target: target.as_deref(),
                result,
            })?;
            match outcome {
                Ok(body) => {
                    // It succeeded, so reset the consecutive-error streak.
                    // Skip this call and a repeated identical error with a
                    // success in between gets misjudged as "consecutive"
                    // and stops the loop (see the comment in stop.rs).
                    stop.observe_success();
                    session.push_tool_result(&call.id, &body);
                }
                Err(msg) => {
                    if let Some(r) = stop.observe_error(&msg) {
                        return Err(AgentError::Stopped(r));
                    }
                    session.push_tool_result(&call.id, &msg);
                }
            }
        }
    }
}

/// Routes a tool call to its actual implementation. A failure becomes a
/// string returned to the model.
///
/// `write` and `edit` resolve to a single target path, so before execution
/// they're run through predicate-based prediction via `ctx.gate.check`. If
/// it's out of bounds, this stops before execution and returns the reason —
/// a refusal is not an exception but an ordinary tool result, and the model
/// gets the chance to write to a different location instead (this is what
/// `agent::tests::a_denied_write_comes_back_as_a_tool_result_not_a_loop_failure`
/// pins down).
///
/// `bash` does not go through this path. Because it executes arbitrary
/// code, what it will touch cannot be determined ahead of time (see the
/// `polaris_tools::bash` docs). It is attempted under confinement, and if
/// denied, the reason carried in the child's output is returned to the
/// model as-is.
fn dispatch(
    call: &polaris_provider::ToolCall,
    skills: &[polaris_skills::Skill],
    ctx: &mut ToolContext<'_>,
) -> Result<String, String> {
    match call.name.as_str() {
        "read" => {
            let path = call.arguments["path"]
                .as_str()
                .ok_or_else(|| "path is missing".to_string())?;
            let offset = call.arguments["offset"].as_u64().unwrap_or(0) as usize;
            let limit = call.arguments["limit"]
                .as_u64()
                .map(|n| n as usize)
                .unwrap_or(polaris_tools::read::DEFAULT_LIMIT);
            polaris_tools::read::read(Path::new(path), offset, limit).map_err(|e| e.to_string())
        }
        "write" => {
            let path = call.arguments["path"]
                .as_str()
                .ok_or_else(|| "path is missing".to_string())?;
            let content = call.arguments["content"]
                .as_str()
                .ok_or_else(|| "content is missing".to_string())?;
            let path = Path::new(path);
            ctx.gate.check(ctx.sandbox, path, ctx.approver)?;
            polaris_tools::write::write(ctx.sandbox, ctx.helper, path, content)
                .map_err(|e| e.to_string())
        }
        "edit" => {
            let path = call.arguments["path"]
                .as_str()
                .ok_or_else(|| "path is missing".to_string())?;
            let old = call.arguments["old"]
                .as_str()
                .ok_or_else(|| "old is missing".to_string())?;
            let new = call.arguments["new"]
                .as_str()
                .ok_or_else(|| "new is missing".to_string())?;
            let path = Path::new(path);
            ctx.gate.check(ctx.sandbox, path, ctx.approver)?;
            polaris_tools::edit::edit(ctx.sandbox, ctx.helper, path, old, new)
                .map_err(|e| e.to_string())
        }
        "bash" => {
            let command = call.arguments["command"]
                .as_str()
                .ok_or_else(|| "command is missing".to_string())?;
            polaris_tools::bash::run(ctx.sandbox, command).map_err(|e| e.to_string())
        }
        "skill" => {
            let q = call.arguments["q"]
                .as_str()
                .ok_or_else(|| "q is missing".to_string())?;
            Ok(polaris_tools::skill::lookup(skills, q))
        }
        other => Err(format!("unknown tool: {other}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_provider::{CompletionResponse, ToolCall};
    use std::sync::Mutex;

    /// A test approver. Always allows, and counts how many times it was asked.
    struct AlwaysAllow {
        asked: usize,
    }
    impl crate::approval::Approver for AlwaysAllow {
        fn ask(&mut self, _reason: &str) -> crate::approval::Decision {
            self.asked += 1;
            crate::approval::Decision::Allow
        }
    }

    /// A test approver. Records how many times it was asked and the last
    /// reason, while returning a given decision. This is the only way to
    /// observe, at the wiring level, whether `Gate::check` was actually
    /// called (i.e. whether it was consulted before execution).
    /// `asked == 0` is direct evidence that "the predicate was never
    /// consulted at all" — the sandbox's own denial message can sometimes
    /// be indistinguishable from `write`/`edit`'s pre-check (Fix round 1's
    /// point), so the denial wording alone isn't enough.
    struct RecordingApprover {
        decision: crate::approval::Decision,
        asked: usize,
        last_reason: Option<String>,
    }
    impl crate::approval::Approver for RecordingApprover {
        fn ask(&mut self, reason: &str) -> crate::approval::Decision {
            self.asked += 1;
            self.last_reason = Some(reason.to_string());
            self.decision
        }
    }

    /// An unused set of context parts, for tests that never call `write` /
    /// `edit` / `bash`. `ToolContext` only holds references, so this owns
    /// the borrowed-from values here and returns them to the caller, who
    /// assembles the `ToolContext` inside each test.
    fn dummy_tool_parts() -> (
        polaris_sandbox::SandboxPolicy,
        std::path::PathBuf,
        crate::approval::Gate,
        AlwaysAllow,
    ) {
        (
            polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::FullAccess, &[])
                .expect("policy"),
            std::path::PathBuf::from("/bin/true"),
            crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never),
            AlwaysAllow { asked: 0 },
        )
    }

    /// A provider that calls a tool the 1st time and returns body text the 2nd time.
    struct Scripted {
        replies: Mutex<Vec<CompletionResponse>>,
    }

    #[async_trait::async_trait]
    impl Provider for Scripted {
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

    /// Pulls the name declared as required straight out of the published
    /// tool definition itself.
    ///
    /// If the test side just wrote `"q"` literally, the schema's
    /// declaration and what `dispatch` reads would remain two independent
    /// claims. Change only one of them and both claims stay correct on
    /// their own terms, so no test fails — the model gets told to send
    /// `query`, the harness looks for `q`, and every skill call dies, all
    /// while green.
    fn declared_required_param(tool: &str) -> String {
        let specs = polaris_tools::all_specs();
        let spec = specs
            .iter()
            .find(|s| s.name == tool)
            .unwrap_or_else(|| panic!("no tool definition for {tool}"));
        let json = serde_json::to_value(spec).expect("cannot serialize");
        json["parameters"]["required"][0]
            .as_str()
            .unwrap_or_else(|| panic!("{tool}'s schema does not declare a required argument"))
            .to_string()
    }

    fn call_with(tool: &str, param: &str, value: &str) -> ToolCall {
        let mut args = serde_json::Map::new();
        args.insert(param.to_string(), serde_json::Value::String(value.into()));
        ToolCall {
            id: "c1".into(),
            name: tool.into(),
            arguments: serde_json::Value::Object(args),
        }
    }

    #[test]
    fn the_skill_tool_reads_the_argument_name_its_schema_declares() {
        // Take the argument name from the published schema, not a literal.
        // Change the schema's `q` to `query` (while dispatch still reads
        // `q`) and this test alone exposes the mismatch.
        let skills = vec![polaris_skills::Skill {
            name: "demo".into(),
            description: "description".into(),
            body: "demo body".into(),
            path: "/x/demo/SKILL.md".into(),
        }];
        let param = declared_required_param("skill");
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let out =
            dispatch(&call_with("skill", &param, "demo"), &skills, &mut ctx).unwrap_or_else(|e| {
                panic!(
                    "dispatch does not read the argument name {param} the public schema declares: {e}"
                )
            });
        assert!(
            out.contains("demo body"),
            "the body was not returned: {out}"
        );
    }

    #[test]
    fn the_read_tool_reads_the_argument_name_its_schema_declares() {
        // Apply the same binding as the skill side to read too. read's
        // schema has existed since M1, but nothing tied its declaration to
        // what gets read, either.
        let dir = tempfile::tempdir().expect("temp directory");
        let target = dir.path().join("a.txt");
        std::fs::write(&target, "hello\n").expect("cannot write");

        let param = declared_required_param("read");
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let out = dispatch(
            &call_with("read", &param, target.to_str().expect("path")),
            &[],
            &mut ctx,
        )
        .unwrap_or_else(|e| {
            panic!(
                "dispatch does not read the argument name {param} the public schema declares: {e}"
            )
        });
        assert!(out.contains("hello"), "the body was not returned: {out}");
    }

    #[test]
    fn a_read_without_an_explicit_limit_says_it_stopped_early() {
        // dispatch fills in the default when a call omits limit. The side
        // that receives that default has no way of knowing it was
        // truncated, so the "(showed lines ... of N total ...)" disclaimer
        // is the only way to convey "the file doesn't end here".
        let dir = tempfile::tempdir().expect("temp directory");
        let target = dir.path().join("long.txt");
        let total = polaris_tools::read::DEFAULT_LIMIT + 3;
        let body: String = (0..total).map(|i| format!("line {i}\n")).collect();
        std::fs::write(&target, body).expect("cannot write");

        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let out = dispatch(
            &call_with("read", "path", target.to_str().expect("path")),
            &[],
            &mut ctx,
        )
        .expect("should be able to read");

        assert!(
            out.contains(&format!(
                "continue with offset={}",
                polaris_tools::read::DEFAULT_LIMIT
            )),
            "the result doesn't show it was cut off at the default limit: {}",
            &out[out.len().saturating_sub(160)..]
        );
    }

    #[tokio::test]
    async fn runs_tool_then_returns_final_text() {
        let dir = tempfile::tempdir().expect("temp directory");
        let target = dir.path().join("a.txt");
        std::fs::write(&target, "hello\n").expect("cannot write");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({ "path": target.to_str().unwrap() }),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    text: "it was 1 line".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let mut session = Session::new();
        session.push_user("how many lines is a.txt");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("cannot open");
        let mut stop = StopTracker::new(10);

        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let out = run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &[],
            &mut ctx,
        )
        .await
        .expect("should succeed")
        .text;
        assert_eq!(out, "it was 1 line");

        let log = std::fs::read_to_string(dir.path().join("audit.jsonl")).expect("cannot read");
        assert!(log.contains("\"tool\":\"read\""), "read was not recorded");
    }

    #[tokio::test]
    async fn interleaved_success_does_not_trip_the_repeated_error_stop() {
        // A regression test through the loop, paired with the unit test on
        // the stop.rs side. Reproduces "the same error 3 times" as 5 errors
        // with successes interleaved between them. If observe_success were
        // not called from the loop's success path, the 3rd error would
        // stop it (despite the successes in between).
        let dir = tempfile::tempdir().expect("temp directory");
        let target = dir.path().join("a.txt");
        std::fs::write(&target, "hello\n").expect("cannot write");

        let fail_call = || CompletionResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: serde_json::json!({ "path": "/home/u/.ssh/id_rsa" }),
            }],
            ..Default::default()
        };
        let ok_call = || CompletionResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: serde_json::json!({ "path": target.to_str().unwrap() }),
            }],
            ..Default::default()
        };

        let p = Scripted {
            replies: Mutex::new(vec![
                fail_call(),
                ok_call(),
                fail_call(),
                ok_call(),
                fail_call(),
                CompletionResponse {
                    text: "done".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let mut session = Session::new();
        session.push_user("read it");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("cannot open");
        let mut stop = StopTracker::new(50);

        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let out = run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &[],
            &mut ctx,
        )
        .await
        .expect("shouldn't stop, since the interleaved successes mean this isn't 3 in a row")
        .text;
        assert_eq!(out, "done");
    }

    #[tokio::test]
    async fn stops_when_tool_fails_three_times() {
        let dir = tempfile::tempdir().expect("temp directory");
        let call = || CompletionResponse {
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: serde_json::json!({ "path": "/home/u/.ssh/id_rsa" }),
            }],
            ..Default::default()
        };
        let p = Scripted {
            replies: Mutex::new(vec![call(), call(), call()]),
        };

        let mut session = Session::new();
        session.push_user("read it");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("cannot open");
        let mut stop = StopTracker::new(50);

        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let err = run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &[],
            &mut ctx,
        )
        .await
        .expect_err("should stop");
        assert!(matches!(
            err,
            AgentError::Stopped(StopReason::RepeatedError(_))
        ));
    }

    #[tokio::test]
    async fn dispatches_the_skill_tool() {
        let dir = tempfile::tempdir().expect("temp directory");
        let skills = vec![polaris_skills::Skill {
            name: "demo".into(),
            description: "description".into(),
            body: "demo body".into(),
            path: "/x/demo/SKILL.md".into(),
        }];

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "skill".into(),
                        arguments: serde_json::json!({ "q": "demo" }),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    text: "read it".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let mut session = Session::new();
        session.push_user("read demo's body");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("cannot open");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };

        let out = run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &skills,
            &mut ctx,
        )
        .await
        .expect("should succeed")
        .text;
        assert_eq!(out, "read it");

        let tool_msg = session
            .messages
            .iter()
            .find(|m| m.tool_call_id.is_some())
            .expect("no tool result was pushed");
        assert!(
            tool_msg.content.contains("demo body"),
            "the body was not passed through"
        );

        let log = std::fs::read_to_string(dir.path().join("audit.jsonl")).expect("cannot read");
        let skill_lines = log
            .lines()
            .filter(|l| l.contains("\"tool\":\"skill\""))
            .count();
        assert_eq!(skill_lines, 1, "the skill call was not recorded: {log}");
    }

    #[tokio::test]
    async fn the_skill_tool_reports_when_no_skills_are_loaded() {
        let dir = tempfile::tempdir().expect("temp directory");
        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "skill".into(),
                        arguments: serde_json::json!({ "q": "something" }),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    text: "got it".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };
        let mut session = Session::new();
        session.push_user("look up a skill");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("cannot open");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };

        run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &[],
            &mut ctx,
        )
        .await
        .expect("should succeed");

        let tool_msg = session
            .messages
            .iter()
            .find(|m| m.tool_call_id.is_some())
            .expect("no tool result was pushed");
        // Checking only "non-empty" would let any replacement pass through
        // — even deleting the whole branch that reports there are no
        // skills at all, this test would stay green, since a failed search
        // would just return a list of candidates (in this case, an empty
        // list). Look for the same specific wording that skill.rs's
        // `an_empty_skill_set_is_distinguishable_from_a_query_matching_nothing`
        // pins down, on the path that goes through the loop as well.
        assert!(
            tool_msg.content.contains("no skill was found at all"),
            "the model was not told that there are no skills at all: {}",
            tool_msg.content
        );
    }

    /// A provider that calls `write` once and then returns body text, plus
    /// everything around it. The helper uses a minimal shell script rather
    /// than the real polaris helper binary. What we want to see here is the
    /// loop's wiring, not the helper's own correctness.
    fn write_helper(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("helper.sh");
        std::fs::write(
            &p,
            r#"#!/bin/sh
python3 -c '
import json,sys,os
m=json.load(sys.stdin)
os.makedirs(os.path.dirname(m["path"]), exist_ok=True)
open(m["path"],"w").write(m["content"])
print("wrote")
'
"#,
        )
        .expect("cannot write");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        p
    }

    #[tokio::test]
    async fn the_write_tool_reads_the_argument_names_its_schema_declares() {
        // Bind the argument names the public schema declares to the names
        // dispatch reads. Don't leave a state where renaming only the
        // schema side still passes every test (the same defect class as
        // M3a's B1). Pull the argument names from the schema; don't write
        // them as literals inside this test.
        let specs = polaris_tools::all_specs();
        let spec = specs.iter().find(|s| s.name == "write").expect("no write");
        let json = serde_json::to_value(spec).expect("serialize");
        let required: Vec<String> = json["parameters"]["required"]
            .as_array()
            .expect("no required")
            .iter()
            .map(|v| {
                v.as_str()
                    .expect("a required element is not a string")
                    .to_string()
            })
            .collect();
        assert_eq!(
            required.len(),
            2,
            "write's required arguments are not 2: {required:?}"
        );

        let root = tempfile::tempdir().expect("temp directory");
        let helper_dir = tempfile::tempdir().expect("temp directory");
        let helper = write_helper(helper_dir.path());
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("policy");
        let target = sandbox.writable_roots()[0].join("out.txt");

        // Build the arguments using only the names the schema declares. If
        // either one had been renamed away from "path", it would receive
        // the value meant for content, the write target would mismatch,
        // and the assert below would fail.
        let mut args = serde_json::Map::new();
        for name in &required {
            let value = if name.contains("path") {
                target.display().to_string()
            } else {
                "body".to_string()
            };
            args.insert(name.clone(), serde_json::Value::String(value));
        }

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "write".into(),
                        arguments: serde_json::Value::Object(args),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    text: "wrote it".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("temp directory");
        let mut session = Session::new();
        session.push_user("create a.txt");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("cannot open");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::OnRequest);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };

        let out = run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &[],
            &mut ctx,
        )
        .await
        .expect("the loop failed")
        .text;

        assert_eq!(out, "wrote it");
        assert_eq!(
            std::fs::read_to_string(&target).expect("not written"),
            "body",
            "dispatch does not read the argument name the schema declares"
        );
    }

    #[tokio::test]
    async fn a_denied_write_comes_back_as_a_tool_result_not_a_loop_failure() {
        // A sandbox denial is not an exception. It comes back in a form the
        // model can understand, and the loop continues. Turning this into
        // an Err here would cost the model its chance to write to a
        // different location instead, and the denial would become an
        // execution failure outright.
        let root = tempfile::tempdir().expect("temp directory");
        let outside = tempfile::tempdir().expect("temp directory");
        let helper_dir = tempfile::tempdir().expect("temp directory");
        let helper = write_helper(helper_dir.path());
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("policy");

        let target = outside
            .path()
            .canonicalize()
            .expect("canonicalize")
            .join("pwned.txt");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "write".into(),
                        arguments: serde_json::json!({
                            "path": target.display().to_string(),
                            "content": "body"
                        }),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    text: "I'll write elsewhere instead".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("temp directory");
        let mut session = Session::new();
        session.push_user("write outside");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("cannot open");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        // Set to Never, closing off the path that would slip through via
        // approval. What we want to see here is that "the denial comes
        // back as a tool result".
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };

        let out = run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &[],
            &mut ctx,
        )
        .await
        .expect("the denial failed the whole loop")
        .text;

        assert_eq!(
            out, "I'll write elsewhere instead",
            "the loop did not advance to the 2nd turn"
        );
        assert!(!target.exists(), "the file was created");

        let tool_msg = session
            .messages
            .iter()
            .find(|m| m.tool_call_id.is_some())
            .expect("no tool result was pushed");
        assert!(
            tool_msg.content.contains("workspace-write"),
            "the denial reason is missing the policy: {}",
            tool_msg.content
        );
        assert!(
            tool_msg.content.contains(&target.display().to_string()),
            "the denial reason is missing the path: {}",
            tool_msg.content
        );
    }

    #[tokio::test]
    async fn a_write_to_a_hardlinked_path_inside_the_root_is_stopped_by_the_gate_before_anything_runs()
     {
        // Fix round 1's point: the `a_denied_write_comes_back_...` test
        // above uses a write outside the root, and even with
        // `Gate::check` removed the real sandbox itself returns a denial
        // message of the same shape (containing the path and the policy),
        // so it cannot distinguish "the predicate stopped it before
        // execution" from "it was attempted and the sandbox denied it". A
        // hard link creates that distinction — the target sits inside the
        // writable root (`<root>/hardlink.txt`), and the real sandbox
        // looks only at the path and allows it through (see
        // `polaris_tools::predicate::tests::an_existing_hardlink_is_surfaced_for_approval`).
        // Confirm that `Gate::check` stopped this before execution on 4
        // points: (1) the approver was actually asked, (2) the reason
        // asked about includes an explanation of the hard link, (3) the
        // helper never ran and the file's contents did not change, and
        // (4) the tool result's wording is `Gate`'s own denial wording,
        // not the sandbox side's denial (`ToolError::WriteDenied`'s
        // "child output:").
        let root = tempfile::tempdir().expect("temp directory");
        let outside = tempfile::tempdir().expect("temp directory");
        let helper_dir = tempfile::tempdir().expect("temp directory");
        let helper = write_helper(helper_dir.path());

        let real = outside.path().join("real.txt");
        std::fs::write(&real, "original").expect("cannot write");
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("policy");
        let linked = sandbox.writable_roots()[0].join("hardlink.txt");
        std::fs::hard_link(&real, &linked).expect("hard_link");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "write".into(),
                        arguments: serde_json::json!({
                            "path": linked.display().to_string(),
                            "content": "tampered"
                        }),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    text: "elsewhere".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("temp directory");
        let mut session = Session::new();
        session.push_user("rewrite hardlink.txt");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("cannot open");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::OnRequest);
        let mut approver = RecordingApprover {
            decision: crate::approval::Decision::Deny,
            asked: 0,
            last_reason: None,
        };
        {
            let mut ctx = ToolContext {
                sandbox: &sandbox,
                helper: &helper,
                gate: &mut gate,
                approver: &mut approver,
            };

            let out = run(
                &p,
                &mut session,
                &mut audit,
                &mut stop,
                &always_on,
                &[],
                &mut ctx,
            )
            .await
            .expect("the denial failed the whole loop")
            .text;
            assert_eq!(out, "elsewhere");
        }

        assert_eq!(approver.asked, 1, "the approver was not asked");
        assert!(
            approver
                .last_reason
                .as_deref()
                .unwrap_or("")
                .contains("hard link"),
            "the reason asked about is missing the hard-link explanation: {:?}",
            approver.last_reason
        );

        assert_eq!(
            std::fs::read_to_string(&real).expect("cannot read"),
            "original",
            "the helper ran ahead of the predicate and rewrote the hard \
             link's underlying file (the real sandbox would allow this \
             write through on the path alone)"
        );

        let tool_msg = session
            .messages
            .iter()
            .find(|m| m.tool_call_id.is_some())
            .expect("no tool result was pushed");
        assert!(
            tool_msg.content.starts_with("the user did not approve"),
            "the denial wording is not the Gate's own \
             (it may be confused with the sandbox side's denial): {}",
            tool_msg.content
        );
        assert!(
            !tool_msg.content.contains("child output"),
            "the sandbox side's (ToolError::WriteDenied) denial wording leaked in: {}",
            tool_msg.content
        );
    }

    /// A stand-in helper that discards stdin and unconditionally
    /// overwrites `target` with fixed content.
    ///
    /// Used only by the gate test below. What we want to see there is
    /// "whether the gate stopped it before execution", not the semantics
    /// of the mutation, so there's no need to parse JSON. In fact, *not*
    /// parsing it makes the test stronger: if the gate were ever slipped
    /// through, the helper always rewrites the target, so the claim "the
    /// contents did not change" reliably takes effect (a stand-in that
    /// parses JSON could fail to parse, write nothing, and end up with
    /// intact contents despite having slipped through).
    fn clobber_helper(dir: &std::path::Path, target: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("clobber-helper.sh");
        std::fs::write(
            &p,
            format!(
                "#!/bin/sh\nset -e\ncat > /dev/null\nprintf 'tampered' > {}\necho clobbered\n",
                target.display()
            ),
        )
        .expect("cannot write");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        p
    }

    #[tokio::test]
    async fn an_edit_to_a_hardlinked_path_inside_the_root_is_stopped_by_the_gate_before_anything_runs()
     {
        // The counterpart to the write version above. The spec imposes
        // predict-and-ask on both `write` and `edit`, but only `write` had
        // been pinned down (final review's point: removing
        // `ctx.gate.check` from `dispatch`'s `"edit"` arm still left all
        // 254 tests green; doing the same removal on the `"write"` arm
        // makes 1 fail). The hard-link mitigation only takes effect
        // because the gate runs, so this is a matter of effectiveness, not
        // tidiness.
        //
        // The apparatus uses a hard link for the same reason as the write
        // version. The target sits inside the writable root
        // (`<root>/hardlink.txt`), and since the real sandbox looks only
        // at the path and allows it through, only the predicate can stop
        // it.
        let root = tempfile::tempdir().expect("temp directory");
        let outside = tempfile::tempdir().expect("temp directory");
        let helper_dir = tempfile::tempdir().expect("temp directory");

        let real = outside.path().join("real.txt");
        std::fs::write(&real, "original").expect("cannot write");
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("policy");
        let linked = sandbox.writable_roots()[0].join("hardlink.txt");
        std::fs::hard_link(&real, &linked).expect("hard_link");
        let helper = clobber_helper(helper_dir.path(), &linked);

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "edit".into(),
                        arguments: serde_json::json!({
                            "path": linked.display().to_string(),
                            "old": "original",
                            "new": "tampered"
                        }),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    text: "elsewhere".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("temp directory");
        let mut session = Session::new();
        session.push_user("edit hardlink.txt");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("cannot open");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::OnRequest);
        let mut approver = RecordingApprover {
            decision: crate::approval::Decision::Deny,
            asked: 0,
            last_reason: None,
        };
        {
            let mut ctx = ToolContext {
                sandbox: &sandbox,
                helper: &helper,
                gate: &mut gate,
                approver: &mut approver,
            };

            let out = run(
                &p,
                &mut session,
                &mut audit,
                &mut stop,
                &always_on,
                &[],
                &mut ctx,
            )
            .await
            .expect("the denial failed the whole loop")
            .text;
            assert_eq!(out, "elsewhere");
        }

        assert_eq!(approver.asked, 1, "the approver was not asked");
        assert!(
            approver
                .last_reason
                .as_deref()
                .unwrap_or("")
                .contains("hard link"),
            "the reason asked about is missing the hard-link explanation: {:?}",
            approver.last_reason
        );

        assert_eq!(
            std::fs::read_to_string(&real).expect("cannot read"),
            "original",
            "the helper ran ahead of the predicate and rewrote the hard \
             link's underlying file (the real sandbox would allow this \
             write through on the path alone)"
        );

        let tool_msg = session
            .messages
            .iter()
            .find(|m| m.tool_call_id.is_some())
            .expect("no tool result was pushed");
        assert!(
            tool_msg.content.starts_with("the user did not approve"),
            "the denial wording is not the Gate's own \
             (it may be confused with the sandbox side's denial): {}",
            tool_msg.content
        );
        assert!(
            !tool_msg.content.contains("child output"),
            "the sandbox side's (ToolError::WriteDenied) denial wording leaked in: {}",
            tool_msg.content
        );
    }

    #[tokio::test]
    async fn bash_attempts_and_reports_without_ever_consulting_the_approver() {
        // Fix round 1's point: `bash` is supposed to never go through the
        // predicate, but there was no test confirming that at the wiring
        // level. Run `bash` once under `ApprovalPolicy::Always` (which
        // always asks regardless of `Verdict`), and confirm the approver
        // was never asked. Always is chosen so that if `Gate::check` were
        // ever mistakenly mixed into the `bash` arm, it would be caught
        // regardless of whether the target path happened to be inside or
        // outside the root (under `OnRequest`, it could be missed
        // depending on which target path the leaked check happened to
        // see).
        //
        // Fix round 2's point: the above only pinned down half — "doesn't
        // ask". What the spec actually requires is the conjunction
        // "attempt it, and report the result" (since what it will touch
        // cannot be decided ahead of time, it doesn't refuse up front; it
        // actually touches it and reports the outcome). Replacing the
        // entire `bash` arm with a no-op (i.e., never executing anything
        // at all) still left this test green (undetected across all 254
        // passing tests). Run a command with a real side effect, and
        // confirm both (1) that the side effect actually happened
        // (evidence the confined child actually ran) and (2) that its
        // output came back as the tool result (evidence the outcome of
        // the attempt is being reported).
        let root = tempfile::tempdir().expect("temp directory");
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("policy");
        let proof = sandbox.writable_roots()[0].join("proof.txt");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "bash".into(),
                        arguments: serde_json::json!({
                            "command": format!(
                                "echo bash-really-ran > {} && cat {}",
                                proof.display(),
                                proof.display()
                            )
                        }),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    text: "done".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("temp directory");
        let mut session = Session::new();
        session.push_user("create proof.txt and tell me what's in it");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("cannot open");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Always);
        let mut approver = RecordingApprover {
            decision: crate::approval::Decision::Allow,
            asked: 0,
            last_reason: None,
        };
        {
            let mut ctx = ToolContext {
                sandbox: &sandbox,
                helper: std::path::Path::new("/bin/true"), // bash does not use a helper
                gate: &mut gate,
                approver: &mut approver,
            };

            let out = run(
                &p,
                &mut session,
                &mut audit,
                &mut stop,
                &always_on,
                &[],
                &mut ctx,
            )
            .await
            .expect("should succeed")
            .text;
            assert_eq!(out, "done");
        }

        // (1) That the side effect actually happened. Replacing the `bash`
        // arm with a no-op could still let the `run` above succeed, so
        // this is the only point that can detect a no-op.
        assert_eq!(
            std::fs::read_to_string(&proof)
                .expect("proof.txt is missing (bash was not actually executed)")
                .trim(),
            "bash-really-ran",
            "no evidence that bash actually ran"
        );

        // (2) That its output came back to the model as the tool result
        // (evidence that the outcome of the attempt is being reported).
        let tool_msg = session
            .messages
            .iter()
            .find(|m| m.tool_call_id.is_some())
            .expect("no tool result was pushed");
        assert!(
            tool_msg.content.contains("bash-really-ran"),
            "the command's output was not returned as the tool result: {}",
            tool_msg.content
        );

        // (3) That it never went through the predicate at all (Fix round
        // 1's original claim).
        assert_eq!(
            approver.asked, 0,
            "bash asked the approver (it touched the path that's supposed to skip the predicate)"
        );
    }

    #[tokio::test]
    async fn a_successful_write_s_audit_record_carries_the_policy_and_the_target() {
        // Fix round 1's point: there was no test checking, through the
        // call path in `agent.rs`, the claim from Tasks 11-12 that the
        // audit record's `sandbox`/`target` get filled in for
        // `write`/`edit` (`audit.rs`'s tests only check the `Record` type
        // itself; the caller's assembly logic is out of scope there).
        // Actually run the loop through one cycle, read back the written
        // audit line, and check both fields.
        let root = tempfile::tempdir().expect("temp directory");
        let helper_dir = tempfile::tempdir().expect("temp directory");
        let helper = write_helper(helper_dir.path());
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("policy");
        let target = sandbox.writable_roots()[0].join("audited.txt");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "write".into(),
                        arguments: serde_json::json!({
                            "path": target.display().to_string(),
                            "content": "body"
                        }),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    text: "wrote it".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("temp directory");
        let audit_path = dir.path().join("audit.jsonl");
        let mut session = Session::new();
        session.push_user("create audited.txt");
        let mut audit = AuditLog::open(&audit_path).expect("cannot open");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::OnRequest);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };

        run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &[],
            &mut ctx,
        )
        .await
        .expect("should succeed");

        let log = std::fs::read_to_string(&audit_path).expect("cannot read");
        let line = log
            .lines()
            .find(|l| l.contains("\"tool\":\"write\""))
            .expect("no write audit line");
        let v: serde_json::Value = serde_json::from_str(line).expect("not JSON");
        // Check for containment of the file name rather than an exact
        // match. tmpdir's random prefix (on macOS,
        // `/private/var/folders/<hash>/T/.tmpXXXXXX/...`) itself looks
        // like a high-entropy string, so `screen()` sometimes partially
        // rewrites it to `[REDACTED]` (confirmed by measurement — this is
        // behavior consistent with secret_screen's own design policy of
        // leaning toward avoiding over-detection rather than misses, and
        // is not a defect here). Checking for an exact match would make
        // the test fragile against the runtime environment's temp
        // directory name, so this only checks that "the target field
        // exists and carries the target file name".
        assert!(
            v["target"]
                .as_str()
                .expect("no target field (or it's null)")
                .contains("audited.txt"),
            "the audit record's target does not carry the write destination: {v}"
        );
        assert!(
            v["sandbox"]
                .as_str()
                .expect("no sandbox field (or it's null)")
                .contains("workspace-write"),
            "the audit record's sandbox is missing the policy: {v}"
        );
    }

    #[tokio::test]
    async fn usage_accumulates_across_a_tool_calling_turn() {
        let dir = tempfile::tempdir().expect("temp directory");
        let target = dir.path().join("a.txt");
        std::fs::write(&target, "hello\n").expect("cannot write");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({ "path": target.to_str().unwrap() }),
                    }],
                    usage: Some(polaris_provider::Usage {
                        input_tokens: 10,
                        output_tokens: 5,
                        total_tokens: 15,
                    }),
                },
                CompletionResponse {
                    text: "it was 1 line".into(),
                    tool_calls: vec![],
                    usage: Some(polaris_provider::Usage {
                        input_tokens: 20,
                        output_tokens: 3,
                        total_tokens: 23,
                    }),
                },
            ]),
        };

        let mut session = Session::new();
        session.push_user("how many lines is a.txt");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("cannot open");
        let mut stop = StopTracker::new(10);

        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let outcome = run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &[],
            &mut ctx,
        )
        .await
        .expect("should succeed");

        assert_eq!(outcome.text, "it was 1 line");
        assert_eq!(outcome.usage.input_tokens, 30);
        assert_eq!(outcome.usage.output_tokens, 8);
        assert_eq!(outcome.usage.total_tokens, 38);
    }

    #[tokio::test]
    async fn a_response_with_no_usage_contributes_zero_not_a_failure() {
        let dir = tempfile::tempdir().expect("temp directory");
        let p = Scripted {
            replies: Mutex::new(vec![CompletionResponse {
                text: "done".into(),
                tool_calls: vec![],
                usage: None,
            }]),
        };

        let mut session = Session::new();
        session.push_user("hi");
        let mut audit = AuditLog::open(&dir.path().join("audit.jsonl")).expect("cannot open");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let outcome = run(
            &p,
            &mut session,
            &mut audit,
            &mut stop,
            &always_on,
            &[],
            &mut ctx,
        )
        .await
        .expect("should succeed");

        assert_eq!(outcome.usage.total_tokens, 0);
    }
}
