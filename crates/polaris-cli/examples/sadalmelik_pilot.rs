//! Two-send live canary using production Session, workflow and Codex transport.
//! The common runner owns isolation; the parent owns the frozen input files.

use polaris_core::{
    agent::{self, ToolContext},
    approval::{ApprovalPolicy, Approver, Decision, Gate},
    audit::AuditLog,
    prompt,
    session::Session,
    stop::StopTracker,
    workflow::{Phase, SessionWorkflow, WorkflowConfig},
};
use polaris_provider::{
    CompletionRequest, CompletionResponse, Provider, ProviderError, Token, TokenSource,
    attempts::{AttemptBudget, AttemptContext, AttemptLedger},
    codex::{CodexProvider, ENDPOINT_BASE},
    turn_affinity::TurnContext,
};
use polaris_sandbox::{SandboxMode, SandboxPolicy};
use serde_json::{Value, json};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

const MODEL: &str = "gpt-6-astra";
const EFFORT: &str = "medium";

struct ReadOnlyTokens;
#[async_trait::async_trait]
impl TokenSource for ReadOnlyTokens {
    async fn token(&self) -> Result<Token, ProviderError> {
        if std::env::var("POLARIS_AUTH_READ_ONLY").as_deref() != Ok("1") {
            return Err(ProviderError::Auth("readonly auth required".into()));
        }
        let path = polaris_auth::store::default_path()
            .map_err(|_| ProviderError::Auth("auth unavailable".into()))?;
        let credentials = polaris_auth::ensure_fresh(polaris_auth::ISSUER, &path)
            .await
            .map_err(|_| ProviderError::Auth("auth unavailable or expired".into()))?;
        Ok(Token {
            access_token: credentials.access_token,
            account_id: credentials.account_id,
            effort: Some(EFFORT.into()),
        })
    }
    async fn refreshed(&self) -> Result<Token, ProviderError> {
        Err(ProviderError::Auth("refresh disabled".into()))
    }
}

struct OneSend {
    inner: Arc<dyn Provider>,
    calls: AtomicUsize,
}
impl OneSend {
    fn prepare(&self, mut request: CompletionRequest) -> Result<CompletionRequest, ProviderError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) != 0 {
            return Err(ProviderError::Budget("one request per arm".into()));
        }
        request.tools.clear();
        Ok(request)
    }
    fn checked(response: CompletionResponse) -> Result<CompletionResponse, ProviderError> {
        if !response.tool_calls.is_empty() || !response.hosted_web_search.is_empty() {
            return Err(ProviderError::Budget("tools prohibited in pilot".into()));
        }
        let u = response
            .usage
            .ok_or_else(|| ProviderError::Decode("usage missing".into()))?;
        if u.input_tokens.checked_add(u.output_tokens) != Some(u.total_tokens)
            || u.cached_tokens > u.input_tokens
        {
            return Err(ProviderError::Decode("usage inconsistent".into()));
        }
        Ok(response)
    }
}
#[async_trait::async_trait]
impl Provider for OneSend {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        Self::checked(self.inner.complete(self.prepare(request)?).await?)
    }
    async fn complete_in_turn(
        &self,
        request: CompletionRequest,
        turn: &TurnContext,
    ) -> Result<CompletionResponse, ProviderError> {
        Self::checked(
            self.inner
                .complete_in_turn(self.prepare(request)?, turn)
                .await?,
        )
    }
}

struct Deny;
impl Approver for Deny {
    fn ask(&mut self, _: &str) -> Decision {
        Decision::Deny
    }
}

fn private_new(path: &Path) -> Result<File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| "new output unavailable".into())
}
fn save(path: &Path, data: &Value) -> Result<(), String> {
    let mut file = private_new(path)?;
    serde_json::to_writer(&mut file, data).map_err(|_| "output encoding failed")?;
    file.write_all(b"\n")
        .and_then(|()| file.sync_all())
        .map_err(|_| "output persistence failed".into())
}
fn read_json(path: &Path) -> Result<Value, String> {
    serde_json::from_slice(&fs::read(path).map_err(|_| "input unavailable")?)
        .map_err(|_| "invalid JSON".into())
}

async fn run_turn(
    provider: Arc<dyn Provider>,
    work: &Path,
    arm: &str,
    user: &str,
) -> Result<agent::AgentOutcome, String> {
    let mut session = Session {
        compaction_threshold: Some(usize::MAX),
        disable_files_md_auto_regenerate: true,
        ..Session::new()
    };
    if arm == "candidate-workflow-only" {
        session.workflow = Some(SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            initial_phase: Phase::Implement,
            always: vec!["builtin:workflow-core".into()],
            phase_skills: [(Phase::Implement, vec!["builtin:implement".into()])].into(),
        }));
    }
    session.push_user(user);
    let always = prompt::assemble_always_on("", "sadalmelik pilot fixture", &[]);
    let sandbox = SandboxPolicy::new(SandboxMode::ReadOnly, &[])
        .map_err(|_| "sandbox configuration failed")?;
    let mut gate = Gate::new(ApprovalPolicy::Never);
    let mut approver = Deny;
    let mut ctx = ToolContext {
        sandbox: &sandbox,
        helper: work,
        gate: &mut gate,
        approver: &mut approver,
    };
    let audit = Arc::new(tokio::sync::Mutex::new(
        AuditLog::open(&work.join(format!("{arm}.audit.jsonl")))
            .map_err(|_| "audit unavailable")?,
    ));
    // The loop checks after incrementing, so 2 admits exactly its first pass.
    let mut stop = StopTracker::new(2);
    agent::run(
        provider.as_ref(),
        &mut session,
        audit,
        &mut stop,
        &always,
        &[],
        &[],
        provider.clone(),
        1,
        0,
        None,
        &mut ctx,
    )
    .await
    .map_err(|error| {
        match error {
            agent::AgentError::Provider(ProviderError::Auth(_)) => "authentication unavailable",
            agent::AgentError::Provider(ProviderError::Http(_)) => "transport failed",
            agent::AgentError::Provider(ProviderError::Budget(_)) => "request or tool limit",
            agent::AgentError::Provider(ProviderError::Decode(_)) => "response or usage invalid",
            _ => "agent failed",
        }
        .into()
    })
}

fn assess(metric: &Value, outcome: &agent::AgentOutcome, expected: &Value) -> Value {
    let u = outcome.usage;
    let usage_matches = metric
        .pointer("/usage/input_tokens")
        .and_then(Value::as_u64)
        == Some(u.input_tokens.into())
        && metric
            .pointer("/usage/output_tokens")
            .and_then(Value::as_u64)
            == Some(u.output_tokens.into())
        && metric
            .pointer("/usage/cache_read_tokens")
            .and_then(Value::as_u64)
            == Some(u.cached_tokens.into())
        && outcome.usage_report.missing_responses == 0
        && outcome.usage_report.failed_requests == 0
        && outcome.usage_report.reported_responses == 1;
    let request_model_match = metric["model"] == MODEL && metric["effort"] == EFFORT;
    let answer_pass =
        serde_json::from_str::<Value>(&outcome.text).is_ok_and(|value| &value == expected);
    let input_multiplier = if u.input_tokens > 272_000 { 2.0 } else { 1.0 };
    let output_multiplier = if u.input_tokens > 272_000 { 1.5 } else { 1.0 };
    let noncached = u.input_tokens.saturating_sub(u.cached_tokens) as f64;
    // Frozen M0 rates; a missing cache-write breakdown remains unknown.
    let upper = ((noncached * 12.5 + u.cached_tokens as f64) * input_multiplier
        + u.output_tokens as f64 * 50.0 * output_multiplier)
        / 1_000_000.0;
    let lower = ((noncached * 10.0 + u.cached_tokens as f64) * input_multiplier
        + u.output_tokens as f64 * 50.0 * output_multiplier)
        / 1_000_000.0;
    let limit_pass = u.output_tokens <= 4096 && upper <= 40.0;
    let pass = usage_matches
        && request_model_match
        && answer_pass
        && limit_pass
        && metric["outcome"] == "completed";
    json!({"pass":pass, "answer_pass":answer_pass, "usage_matches":usage_matches, "request_model_match":request_model_match,
        "response_model_verified":false, "post_response_limit_pass":limit_pass,
        "usage":{"input":u.input_tokens,"output":u.output_tokens,"cache":u.cached_tokens,"total":u.total_tokens},
        "cache_write_tokens":metric.pointer("/usage/cache_write_tokens"),
        "api_equivalent_usd_interval":[lower,upper],"pricing_basis":"M0 frozen rates; cache-write split unknown; not actual Codex billing",
        "answer":outcome.text})
}

async fn run(work: PathBuf, arm: String) -> Result<(), String> {
    if !matches!(
        arm.as_str(),
        "candidate-features-off" | "candidate-workflow-only"
    ) || work.canonicalize().ok().as_ref() != Some(&work)
    {
        return Err("invalid pilot arguments".into());
    }
    if std::env::var_os("POLARIS_METRICS_PATH").map(PathBuf::from)
        != Some(work.join(format!("{arm}.metrics.jsonl")))
    {
        return Err("metrics path mismatch".into());
    }
    if arm == "candidate-workflow-only"
        && read_json(&work.join("candidate-features-off.result.json"))?["pass"] != true
    {
        return Err("first arm did not pass".into());
    }
    save(
        &work.join(format!("{arm}.started.json")),
        &json!({"arm":arm,"state":"claimed"}),
    )?;
    let req = read_json(&work.join("request.json"))?;
    if req["tools"] != json!([]) || req["examples"] != json!([]) {
        return Err("invalid pilot fixture".into());
    }
    let user = req["prompt"].as_str().ok_or("prompt missing")?;
    let expected = read_json(&work.join("expected.json"))?;
    let ledger = AttemptLedger::open_observed(
        &work.join("attempts.json"),
        AttemptContext {
            parent_id: Some(arm.clone()),
            ..Default::default()
        },
        AttemptBudget::new(2).map_err(|_| "invalid budget")?,
    )
    .map_err(|_| "attempt ledger unavailable")?;
    let inner = CodexProvider::new(ENDPOINT_BASE.into(), MODEL.into(), Arc::new(ReadOnlyTokens))
        .map_err(|_| "provider configuration failed")?
        .with_attempt_ledger(ledger.clone())
        .with_visible_request_byte_limit(32_000);
    inner.set_effort(Some(EFFORT));
    let provider = Arc::new(OneSend {
        inner: Arc::new(inner),
        calls: AtomicUsize::new(0),
    });
    let started = Instant::now();
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(115),
        run_turn(provider, &work, &arm, user),
    )
    .await;
    let outcome = match result {
        Ok(Ok(value)) => value,
        failure => {
            let reason = match failure {
                Ok(Err(reason)) => reason,
                _ => "timeout".into(),
            };
            save(
                &work.join(format!("{arm}.result.json")),
                &json!({"arm":arm,"pass":false,"error":reason,"elapsed_seconds":started.elapsed().as_secs_f64(),"attempts":ledger.snapshot()}),
            )?;
            return Err(reason);
        }
    };
    let content = fs::read_to_string(work.join(format!("{arm}.metrics.jsonl")))
        .map_err(|_| "metrics missing")?;
    let rows = content
        .lines()
        .map(serde_json::from_str::<Value>)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|_| "metrics invalid")?;
    if rows.len() != 1 {
        return Err("physical metric count mismatch".into());
    }
    let mut report = assess(&rows[0], &outcome, &expected);
    let attempts = ledger.snapshot();
    let own = attempts
        .iter()
        .filter(|r| r.parent_id.as_deref() == Some(&arm))
        .count();
    if own != 1 || attempts.len() > 2 {
        report["pass"] = json!(false);
    }
    report["arm"] = json!(arm);
    report["elapsed_seconds"] = json!(started.elapsed().as_secs_f64());
    report["physical_sends"] = json!(own);
    save(&work.join(format!("{arm}.result.json")), &report)?;
    println!("{}", report);
    if report["pass"] != true {
        return Err("pilot acceptance failed".into());
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = if args.len() == 2 {
        run(PathBuf::from(&args[0]), args[1].clone()).await
    } else {
        Err("expected work and arm".into())
    };
    if let Err(reason) = result {
        eprintln!("予備測定を停止: {reason}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_provider::{Message, Usage, UsageReport};
    struct Fake;
    #[async_trait::async_trait]
    impl Provider for Fake {
        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            assert!(request.tools.is_empty());
            Ok(CompletionResponse {
                text: "{}".into(),
                usage: Some(Usage {
                    input_tokens: 10,
                    output_tokens: 2,
                    total_tokens: 12,
                    cached_tokens: 0,
                }),
                ..Default::default()
            })
        }
    }
    fn request() -> CompletionRequest {
        CompletionRequest {
            system: String::new(),
            messages: vec![Message::user("test")],
            tools: polaris_tools::all_specs(),
        }
    }
    #[tokio::test]
    async fn second_logical_request_is_blocked_and_tools_are_removed() {
        let provider = OneSend {
            inner: Arc::new(Fake),
            calls: AtomicUsize::new(0),
        };
        provider.complete(request()).await.unwrap();
        assert!(provider.complete(request()).await.is_err());
    }
    #[test]
    fn missing_usage_and_tool_responses_are_rejected_before_dispatch() {
        assert!(OneSend::checked(CompletionResponse::default()).is_err());
        let mut response = CompletionResponse {
            usage: Some(Usage::default()),
            ..Default::default()
        };
        response.tool_calls.push(polaris_provider::ToolCall {
            id: "1".into(),
            name: "bash".into(),
            arguments: json!({"command":"touch forbidden"}),
        });
        assert!(OneSend::checked(response).is_err());
    }
    #[test]
    fn null_cache_usage_model_mismatch_and_wrong_answer_fail() {
        let usage = Usage {
            input_tokens: 10,
            output_tokens: 2,
            total_tokens: 12,
            cached_tokens: 0,
        };
        let outcome = agent::AgentOutcome {
            text: "{}".into(),
            usage,
            usage_report: UsageReport {
                usage,
                reported_responses: 1,
                missing_responses: 0,
                failed_requests: 0,
            },
        };
        let valid = json!({"model":MODEL,"effort":EFFORT,"usage":{"input_tokens":10,"output_tokens":2,"cache_read_tokens":0},"outcome":"completed"});
        assert_eq!(assess(&valid, &outcome, &json!({}))["pass"], true);
        for pointer in ["/model", "/usage/cache_read_tokens"] {
            let mut invalid = valid.clone();
            *invalid.pointer_mut(pointer).unwrap() = Value::Null;
            assert_eq!(assess(&invalid, &outcome, &json!({}))["pass"], false);
        }
        assert_eq!(
            assess(&valid, &outcome, &json!({"wrong":true}))["pass"],
            false
        );
    }
    #[test]
    fn arm_claim_cannot_be_overwritten() {
        let temp = tempfile::tempdir().unwrap();
        let p = temp.path().join("started.json");
        save(&p, &json!({})).unwrap();
        assert!(save(&p, &json!({})).is_err());
    }

    #[tokio::test]
    async fn production_loop_injects_workflow_only_in_the_enabled_arm() {
        struct Capture(std::sync::Mutex<Vec<String>>);
        #[async_trait::async_trait]
        impl Provider for Capture {
            async fn complete(
                &self,
                req: CompletionRequest,
            ) -> Result<CompletionResponse, ProviderError> {
                self.0.lock().unwrap().push(req.system.clone());
                Fake.complete(req).await
            }
        }
        for (arm, expected) in [
            ("candidate-features-off", false),
            ("candidate-workflow-only", true),
        ] {
            let temp = tempfile::tempdir().unwrap();
            let capture = Arc::new(Capture(std::sync::Mutex::new(vec![])));
            let provider = Arc::new(OneSend {
                inner: capture.clone(),
                calls: AtomicUsize::new(0),
            });
            run_turn(provider, temp.path(), arm, "Return {}.")
                .await
                .unwrap();
            let requests = capture.0.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!(requests[0].contains("Workflow focus: implement."), expected);
            assert_eq!(
                requests[0].contains("Work within the approved specification."),
                expected
            );
        }
    }
}
