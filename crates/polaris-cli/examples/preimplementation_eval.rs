//! One approved two-turn pre-implementation evaluation trial.
//!
//! The Python launcher owns the 48-trial / 96-physical-request schedule. This example accepts one
//! public case and never reads fixture expectations or judges answer quality.

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
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

const MODEL: &str = "gpt-6-astra";
const EFFORT: &str = "medium";
const MAX_OUTPUT: u32 = 4096;
const MAX_SENDS: u64 = 96;
fn attempt_cap(value: Option<&str>) -> Result<u64, String> {
    let cap = value
        .map(str::parse::<u64>)
        .transpose()
        .map_err(|_| "invalid attempt cap")?
        .unwrap_or(MAX_SENDS);
    if cap == 0 || cap > MAX_SENDS {
        return Err("invalid attempt cap".into());
    }
    Ok(cap)
}
const MAX_COST_USD: f64 = 40.0;

#[derive(Debug)]
struct Args {
    trial_id: String,
    case_path: PathBuf,
    arm: String,
}

#[derive(Debug)]
struct PublicCase {
    id: String,
    phase: String,
    prompts: Vec<String>,
}

fn safe_trial_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"._-".contains(&b))
}

fn parse_args(values: &[String]) -> Result<Args, String> {
    if values.len() != 6
        || values[0] != "--trial-id"
        || values[2] != "--case-path"
        || values[4] != "--arm"
    {
        return Err("usage: preimplementation_eval --trial-id SAFE --case-path ABSOLUTE_JSON --arm baseline|workflow".into());
    }
    let case_path = PathBuf::from(&values[3]);
    if !safe_trial_id(&values[1])
        || !case_path.is_absolute()
        || !matches!(values[5].as_str(), "baseline" | "workflow")
    {
        return Err("invalid trial arguments".into());
    }
    Ok(Args {
        trial_id: values[1].clone(),
        case_path,
        arm: values[5].clone(),
    })
}

fn work_dir() -> Result<PathBuf, String> {
    let work = std::env::var_os("POLARIS_PREIMPLEMENTATION_WORK")
        .map(PathBuf::from)
        .ok_or("work directory missing")?;
    if !work.is_absolute() || work.canonicalize().ok().as_ref() != Some(&work) {
        return Err("work directory must be canonical".into());
    }
    Ok(work)
}

fn read_case(path: &Path) -> Result<PublicCase, String> {
    let data = fs::read(path).map_err(|_| "public case unavailable")?;
    let value: Value = serde_json::from_slice(&data).map_err(|_| "public case invalid")?;
    let object = value.as_object().ok_or("public case invalid")?;
    if object.len() != 3
        || !object.contains_key("id")
        || !object.contains_key("phase")
        || !object.contains_key("prompts")
    {
        return Err("public case invalid".into());
    }
    let id = object
        .get("id")
        .and_then(Value::as_str)
        .ok_or("public case invalid")?
        .to_string();
    let phase = object
        .get("phase")
        .and_then(Value::as_str)
        .ok_or("public case invalid")?
        .to_string();
    let prompts = object
        .get("prompts")
        .and_then(Value::as_array)
        .ok_or("public case invalid")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_string)
                .ok_or("public case invalid".into())
        })
        .collect::<Result<Vec<_>, String>>()?;
    let case = PublicCase { id, phase, prompts };
    if !safe_trial_id(&case.id)
        || !matches!(
            case.phase.as_str(),
            "general" | "brainstorm" | "specify" | "review"
        )
        || case.prompts.len() != 2
        || case.prompts.iter().any(|prompt| prompt.is_empty())
    {
        return Err("public case contract invalid".into());
    }
    Ok(case)
}

struct ReadOnlyTokens;
#[async_trait::async_trait]
impl TokenSource for ReadOnlyTokens {
    async fn token(&self) -> Result<Token, ProviderError> {
        if std::env::var("POLARIS_AUTH_READ_ONLY").as_deref() != Ok("1") {
            return Err(ProviderError::Auth(
                "readonly authentication required".into(),
            ));
        }
        let path = polaris_auth::store::default_path()
            .map_err(|_| ProviderError::Auth("authentication unavailable".into()))?;
        let credentials = polaris_auth::ensure_fresh(polaris_auth::ISSUER, &path)
            .await
            .map_err(|_| ProviderError::Auth("authentication unavailable".into()))?;
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

/// The agent loop can issue one physical request for this user turn only.
struct OnePerTurn {
    inner: Arc<dyn Provider>,
    calls: AtomicUsize,
}
impl OnePerTurn {
    fn request(&self, mut request: CompletionRequest) -> Result<CompletionRequest, ProviderError> {
        if self.calls.fetch_add(1, Ordering::SeqCst) != 0 {
            return Err(ProviderError::Budget("one request per turn".into()));
        }
        request.tools.clear();
        Ok(request)
    }
    fn response(response: CompletionResponse) -> Result<CompletionResponse, ProviderError> {
        if !response.tool_calls.is_empty() || !response.hosted_web_search.is_empty() {
            return Err(ProviderError::Budget("tools prohibited".into()));
        }
        let usage = response
            .usage
            .ok_or_else(|| ProviderError::Decode("usage missing".into()))?;
        if usage.input_tokens.checked_add(usage.output_tokens) != Some(usage.total_tokens)
            || usage.cached_tokens > usage.input_tokens
        {
            return Err(ProviderError::Decode("usage invalid".into()));
        }
        Ok(response)
    }
}
#[async_trait::async_trait]
impl Provider for OnePerTurn {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        Self::response(self.inner.complete(self.request(request)?).await?)
    }
    async fn complete_in_turn(
        &self,
        request: CompletionRequest,
        turn: &TurnContext,
    ) -> Result<CompletionResponse, ProviderError> {
        Self::response(
            self.inner
                .complete_in_turn(self.request(request)?, turn)
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

fn new_file(path: &Path) -> Result<File, String> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| "new result unavailable".into())
}
fn save(path: &Path, value: &Value) -> Result<(), String> {
    let mut file = new_file(path)?;
    serde_json::to_writer(&mut file, value).map_err(|_| "result encoding failed")?;
    file.write_all(b"\n")
        .and_then(|_| file.sync_all())
        .map_err(|_| "result persistence failed".into())
}
fn phase(value: &str) -> Phase {
    match value {
        "brainstorm" => Phase::Brainstorm,
        "specify" => Phase::Specify,
        "review" => Phase::Review,
        _ => Phase::General,
    }
}

fn workflow_for(case: &PublicCase, arm: &str) -> Option<SessionWorkflow> {
    if arm != "workflow" {
        return None;
    }
    let phase = phase(&case.phase);
    let mut phase_skills = BTreeMap::new();
    if phase != Phase::General {
        phase_skills.insert(phase, vec![format!("builtin:{}", case.phase)]);
    }
    Some(SessionWorkflow::new(WorkflowConfig {
        enabled: true,
        initial_phase: phase,
        always: vec!["builtin:workflow-core".into()],
        phase_skills,
    }))
}

async fn agent_turn(
    provider: Arc<dyn Provider>,
    session: &mut Session,
    audit: Arc<tokio::sync::Mutex<AuditLog>>,
    work: &Path,
    prompt_text: &str,
) -> Result<agent::AgentOutcome, String> {
    session.push_user(prompt_text);
    let always = prompt::assemble_always_on("", "preimplementation evaluation", &[]);
    let sandbox = SandboxPolicy::new(SandboxMode::ReadOnly, &[])
        .map_err(|_| "sandbox configuration failed")?;
    let mut gate = Gate::new(ApprovalPolicy::Never);
    let mut approver = Deny;
    let mut context = ToolContext {
        sandbox: &sandbox,
        helper: work,
        gate: &mut gate,
        approver: &mut approver,
    };
    let mut stop = StopTracker::new(2); // Agent increments before checking: this admits one pass.
    agent::run(
        provider.as_ref(),
        session,
        audit,
        &mut stop,
        &always,
        &[],
        &[],
        provider.clone(),
        1,
        0,
        None,
        &mut context,
    )
    .await
    .map_err(|error| match error {
        agent::AgentError::Provider(ProviderError::Auth(_)) => "authentication unavailable".into(),
        agent::AgentError::Provider(ProviderError::Http(_)) => "transport failed".into(),
        agent::AgentError::Provider(ProviderError::Budget(_)) => "request or protocol limit".into(),
        agent::AgentError::Provider(ProviderError::Decode(_)) => "response or usage invalid".into(),
        _ => "agent failed".into(),
    })
}

fn metrics(path: &Path) -> Result<Vec<Value>, String> {
    fs::read_to_string(path)
        .map_err(|_| "metrics missing")?
        .lines()
        .map(|line| serde_json::from_str(line).map_err(|_| "metrics invalid".into()))
        .collect()
}
fn cost_upper(input: u64, output: u64, cached: u64) -> Option<f64> {
    if cached > input {
        return None;
    }
    let (input_multiplier, output_multiplier) = if input > 272_000 {
        (2.0, 1.5)
    } else {
        (1.0, 1.0)
    };
    Some(
        (((input - cached) as f64 * 12.5 + cached as f64) * input_multiplier
            + output as f64 * 50.0 * output_multiplier)
            / 1_000_000.0,
    )
}
fn assess(outcome: &agent::AgentOutcome, metric: &Value) -> Value {
    let usage = outcome.usage;
    let input = metric
        .pointer("/usage/input_tokens")
        .and_then(Value::as_u64);
    let output = metric
        .pointer("/usage/output_tokens")
        .and_then(Value::as_u64);
    let cached = metric
        .pointer("/usage/cache_read_tokens")
        .and_then(Value::as_u64);
    let matches = input == Some(usage.input_tokens.into())
        && output == Some(usage.output_tokens.into())
        && cached == Some(usage.cached_tokens.into());
    let upper = input
        .zip(output)
        .zip(cached)
        .and_then(|((input, output), cached)| cost_upper(input, output, cached));
    let write_value = metric.pointer("/usage/cache_write_tokens");
    let cache_write = write_value.and_then(Value::as_u64);
    // The upper bound already prices every non-read input token as a write.
    // A missing split remains unknown; an impossible reported split fails.
    let split_valid = match write_value {
        None | Some(Value::Null) => true,
        _ => cache_write.is_some_and(|write| {
            input
                .zip(cached)
                .is_some_and(|(input, read)| read <= input && write <= input - read)
        }),
    };
    let exact = if split_valid {
        input
            .zip(output)
            .zip(cached)
            .zip(cache_write)
            .map(|(((i, o), r), w)| {
                let (im, om) = if i > 272_000 { (2.0, 1.5) } else { (1.0, 1.0) };
                (((i - r - w) as f64 * 10.0 + w as f64 * 12.5 + r as f64) * im
                    + o as f64 * 50.0 * om)
                    / 1_000_000.0
            })
    } else {
        None
    };
    let post_response_limit_pass = usage.output_tokens <= MAX_OUTPUT;
    let usage_total_consistent =
        usage.input_tokens.checked_add(usage.output_tokens) == Some(usage.total_tokens);
    let complete = matches
        && usage_total_consistent
        && split_valid
        && metric["model"] == MODEL
        && metric["effort"] == EFFORT
        && metric["outcome"] == "completed"
        && outcome.usage_report.reported_responses == 1
        && outcome.usage_report.missing_responses == 0
        && outcome.usage_report.failed_requests == 0
        && upper.is_some();
    json!({"answer":outcome.text,"usage":{"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,"cache_read_tokens":usage.cached_tokens,"cache_write_tokens":cache_write,"total_tokens":usage.total_tokens},"metrics":metric,"usage_total_consistent":usage_total_consistent,"elapsed_seconds":Value::Null,"request_model":metric["model"],"response_model_verified":false,"effort":metric["effort"],"physical_sends":1,"measurement_complete":complete,"post_response_limit_pass":post_response_limit_pass,"observed_cost_upper_usd":upper,"observed_cost_exact_usd":exact,"cache_write_split_known":cache_write.is_some(),"cache_write_split_valid":split_valid})
}

fn prior_cost(work: &Path) -> Result<f64, String> {
    let mut total = 0.0;
    for entry in fs::read_dir(work).map_err(|_| "result directory unavailable")? {
        let entry = entry.map_err(|_| "result directory unavailable")?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !(name.ends_with(".turn-1.result.json") || name.ends_with(".turn-2.result.json")) {
            continue;
        }
        let value: Value = serde_json::from_slice(
            &fs::read(entry.path()).map_err(|_| "prior result unavailable")?,
        )
        .map_err(|_| "prior result invalid")?;
        let upper = value
            .get("observed_cost_upper_usd")
            .and_then(Value::as_f64)
            .filter(|v| v.is_finite() && *v >= 0.0)
            .ok_or("prior cost unavailable")?;
        total += upper;
    }
    Ok(total)
}

async fn run(args: Args) -> Result<(), String> {
    let work = work_dir()?;
    let case = read_case(&args.case_path)?;
    if std::env::var_os("POLARIS_METRICS_PATH").map(PathBuf::from)
        != Some(work.join(format!("{}.metrics.jsonl", args.trial_id)))
    {
        return Err("metrics path mismatch".into());
    }
    if std::env::var("POLARIS_AUTH_READ_ONLY").as_deref() != Ok("1") {
        return Err("readonly authentication required".into());
    }
    save(
        &work.join(format!("{}.started.json", args.trial_id)),
        &json!({"trial_id":args.trial_id,"case_id":case.id,"arm":args.arm,"state":"claimed","max_physical_sends":2}),
    )?;
    let ledger = AttemptLedger::open_observed(
        &work.join("attempts.json"),
        AttemptContext {
            parent_id: Some(args.trial_id.clone()),
            ..Default::default()
        },
        AttemptBudget::new(attempt_cap(
            std::env::var("POLARIS_EVAL_MAX_REQUESTS").ok().as_deref(),
        )?)
        .map_err(|_| "ledger budget invalid")?,
    )
    .map_err(|_| "attempt ledger unavailable")?;
    let inner = CodexProvider::new(ENDPOINT_BASE.into(), MODEL.into(), Arc::new(ReadOnlyTokens))
        .map_err(|_| "provider configuration failed")?
        .with_attempt_ledger(ledger.clone())
        .with_visible_request_byte_limit(32_000);
    inner.set_effort(Some(EFFORT));
    let provider: Arc<dyn Provider> = Arc::new(inner);
    let mut session = Session {
        compaction_threshold: Some(usize::MAX),
        disable_files_md_auto_regenerate: true,
        ..Session::new()
    };
    session.workflow = workflow_for(&case, &args.arm);
    let audit = Arc::new(tokio::sync::Mutex::new(
        AuditLog::open(&work.join(format!("{}.audit.jsonl", args.trial_id)))
            .map_err(|_| "audit unavailable")?,
    ));
    let started = Instant::now();
    let mut turn_records = Vec::new();
    for (index, prompt_text) in case.prompts.iter().enumerate() {
        if prior_cost(&work)? >= MAX_COST_USD {
            return Err("cumulative observed cost limit reached".into());
        }
        let sends_before = ledger
            .snapshot()
            .iter()
            .filter(|record| record.parent_id.as_deref() == Some(args.trial_id.as_str()))
            .count();
        let provider: Arc<dyn Provider> = Arc::new(OnePerTurn {
            inner: provider.clone(),
            calls: AtomicUsize::new(0),
        });
        let turn_started = Instant::now();
        let outcome = tokio::time::timeout(
            Duration::from_secs(120),
            agent_turn(provider, &mut session, audit.clone(), &work, prompt_text),
        )
        .await;
        let path = work.join(format!("{}.turn-{}.result.json", args.trial_id, index + 1));
        let record = match outcome {
            Ok(Ok(outcome)) => {
                let rows = metrics(&work.join(format!("{}.metrics.jsonl", args.trial_id)))?;
                if rows.len() != index + 1 {
                    return Err("physical metric count mismatch".into());
                }
                let mut record = assess(&outcome, &rows[index]);
                record["elapsed_seconds"] = json!(turn_started.elapsed().as_secs_f64());
                record
            }
            Ok(Err(reason)) => {
                json!({"answer":Value::Null,"usage":Value::Null,"metrics":Value::Null,"elapsed_seconds":turn_started.elapsed().as_secs_f64(),"request_model":Value::Null,"effort":Value::Null,"physical_sends":0,"measurement_complete":false,"observed_cost_upper_usd":Value::Null,"error":reason})
            }
            Err(_) => {
                json!({"answer":Value::Null,"usage":Value::Null,"metrics":Value::Null,"elapsed_seconds":turn_started.elapsed().as_secs_f64(),"request_model":Value::Null,"effort":Value::Null,"physical_sends":0,"measurement_complete":false,"observed_cost_upper_usd":Value::Null,"error":"timeout"})
            }
        };
        let sends_after = ledger
            .snapshot()
            .iter()
            .filter(|record| record.parent_id.as_deref() == Some(args.trial_id.as_str()))
            .count();
        let mut record = record;
        record["physical_sends"] = json!(sends_after.saturating_sub(sends_before));
        save(&path, &record)?;
        if record["measurement_complete"] != true
            || record["post_response_limit_pass"] != true
            || prior_cost(&work)? > MAX_COST_USD
        {
            return Err("measurement stopped".into());
        }
        turn_records.push(record);
    }
    let physical_sends = ledger
        .snapshot()
        .iter()
        .filter(|record| record.parent_id.as_deref() == Some(args.trial_id.as_str()))
        .count();
    let complete = physical_sends == 2
        && turn_records
            .iter()
            .all(|record| record["measurement_complete"] == true);
    let result = json!({"trial_id":args.trial_id,"case_id":case.id,"arm":args.arm,"phase":case.phase,"turns":turn_records,"elapsed_seconds":started.elapsed().as_secs_f64(),"physical_sends":physical_sends,"measurement_complete":complete});
    save(
        &work.join(format!("{}.result.json", args.trial_id)),
        &result,
    )?;
    println!(
        "{}",
        json!({"trial_id":args.trial_id,"physical_sends":physical_sends,"measurement_complete":complete})
    );
    if !complete {
        return Err("measurement incomplete".into());
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let values: Vec<String> = std::env::args().skip(1).collect();
    let result = match parse_args(&values) {
        Ok(args) => run(args).await,
        Err(error) => Err(error),
    };
    if let Err(reason) = result {
        eprintln!("実装前評価を停止: {reason}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn continuation_can_only_reduce_attempt_cap() {
        assert_eq!(super::attempt_cap(None).unwrap(), 96);
        assert_eq!(super::attempt_cap(Some("92")).unwrap(), 92);
        for invalid in ["0", "97", "-1", "bad"] {
            assert!(super::attempt_cap(Some(invalid)).is_err());
        }
    }
    use super::*;
    use polaris_provider::{Usage, UsageReport};
    use std::sync::Mutex;
    #[test]
    fn arguments_reject_unsafe_trial_and_unknown_arm() {
        assert!(
            parse_args(&[
                "--trial-id".into(),
                "PRE01-baseline-r1".into(),
                "--case-path".into(),
                "/tmp/case.json".into(),
                "--arm".into(),
                "baseline".into()
            ])
            .is_ok()
        );
        assert!(
            parse_args(&[
                "--trial-id".into(),
                "../bad".into(),
                "--case-path".into(),
                "/tmp/case.json".into(),
                "--arm".into(),
                "baseline".into()
            ])
            .is_err()
        );
        assert!(
            parse_args(&[
                "--trial-id".into(),
                "ok".into(),
                "--case-path".into(),
                "/tmp/case.json".into(),
                "--arm".into(),
                "other".into()
            ])
            .is_err()
        );
    }
    #[test]
    fn one_per_turn_rejects_second_request_before_inner_send() {
        struct Fake;
        #[async_trait::async_trait]
        impl Provider for Fake {
            async fn complete(
                &self,
                _: CompletionRequest,
            ) -> Result<CompletionResponse, ProviderError> {
                Ok(CompletionResponse::default())
            }
        }
        let guard = OnePerTurn {
            inner: Arc::new(Fake),
            calls: AtomicUsize::new(0),
        };
        let request = || CompletionRequest {
            system: String::new(),
            messages: vec![],
            tools: vec![],
        };
        assert!(guard.request(request()).is_ok());
        assert!(guard.request(request()).is_err());
    }
    #[test]
    fn assessment_requires_complete_matching_usage_and_limit() {
        let usage = Usage {
            input_tokens: 20,
            output_tokens: 3,
            total_tokens: 23,
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
        let metric = json!({"model":MODEL,"effort":EFFORT,"outcome":"completed","usage":{"input_tokens":20,"output_tokens":3,"cache_read_tokens":0}});
        assert_eq!(assess(&outcome, &metric)["measurement_complete"], true);
        assert_eq!(
            assess(&outcome, &metric)["observed_cost_exact_usd"],
            Value::Null
        );
        let mut reported = metric.clone();
        reported["usage"]["cache_write_tokens"] = json!(10);
        let assessment = assess(&outcome, &reported);
        assert_eq!(assessment["measurement_complete"], true);
        assert_eq!(assessment["observed_cost_exact_usd"], json!(0.000375));
        assert_eq!(assessment["observed_cost_upper_usd"], json!(0.0004));
        reported["usage"]["cache_write_tokens"] = json!(21);
        assert_eq!(assess(&outcome, &reported)["measurement_complete"], false);
        assert_eq!(
            assess(
                &outcome,
                &json!({"model":MODEL,"effort":EFFORT,"outcome":"completed","usage":{"input_tokens":20,"output_tokens":3}})
            )["measurement_complete"],
            false
        );
    }
    #[tokio::test]
    async fn production_agent_loop_preserves_history_and_enables_only_workflow_skills() {
        struct Capture {
            requests: Mutex<Vec<CompletionRequest>>,
            calls: AtomicUsize,
        }
        #[async_trait::async_trait]
        impl Provider for Capture {
            async fn complete(
                &self,
                request: CompletionRequest,
            ) -> Result<CompletionResponse, ProviderError> {
                self.requests.lock().unwrap().push(request);
                let number = self.calls.fetch_add(1, Ordering::SeqCst);
                Ok(CompletionResponse {
                    text: if number == 0 {
                        "first assistant"
                    } else {
                        "second assistant"
                    }
                    .into(),
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
        for arm in ["baseline", "workflow"] {
            let temp = tempfile::tempdir().unwrap();
            let capture = Arc::new(Capture {
                requests: Mutex::new(vec![]),
                calls: AtomicUsize::new(0),
            });
            let case = PublicCase {
                id: "PRE01".into(),
                phase: "brainstorm".into(),
                prompts: vec!["first user".into(), "second user".into()],
            };
            let mut session = Session {
                compaction_threshold: Some(usize::MAX),
                disable_files_md_auto_regenerate: true,
                ..Session::new()
            };
            session.workflow = workflow_for(&case, arm);
            let audit = Arc::new(tokio::sync::Mutex::new(
                AuditLog::open(&temp.path().join("audit.jsonl")).unwrap(),
            ));
            for prompt_text in &case.prompts {
                let provider: Arc<dyn Provider> = Arc::new(OnePerTurn {
                    inner: capture.clone(),
                    calls: AtomicUsize::new(0),
                });
                agent_turn(
                    provider,
                    &mut session,
                    audit.clone(),
                    temp.path(),
                    prompt_text,
                )
                .await
                .unwrap();
            }
            let requests = capture.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert!(requests[0].tools.is_empty());
            assert!(
                requests[1]
                    .messages
                    .iter()
                    .any(|message| message.content == "first user")
            );
            assert!(
                requests[1]
                    .messages
                    .iter()
                    .any(|message| message.content == "first assistant")
            );
            assert!(
                requests[1]
                    .messages
                    .iter()
                    .any(|message| message.content == "second user")
            );
            assert_eq!(
                requests[1].system.contains("### builtin:workflow-core"),
                arm == "workflow"
            );
            assert_eq!(
                requests[1].system.contains("### builtin:brainstorm"),
                arm == "workflow"
            );
            assert_eq!(requests[1].system.contains("### builtin:implement"), false);
        }
    }
}
