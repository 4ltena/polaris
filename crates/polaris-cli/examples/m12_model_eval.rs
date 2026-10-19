//! Opt-in M12 input comparison through durable sessions and the production agent.
//! Default mode validates and prints a plan without authentication, writes or sends.
#[path = "eval_support/accounting.rs"]
mod accounting;
#[path = "eval_support/auth.rs"]
mod eval_auth;
#[path = "m12_model_eval/guard.rs"]
mod guard;
#[path = "m12_model_eval/input.rs"]
mod input;
#[cfg(test)]
#[path = "m12_model_eval/tests.rs"]
mod tests;

use clap::Parser;
use input::{Args, Case};
use polaris_core::{
    agent::{self, ToolContext},
    approval::{ApprovalPolicy, Approver, Decision, Gate},
    audit::AuditLog,
    conversation_state::{HistoryMode, content_hash},
    input_preparation::{InputPreparationMode, prepare},
    measurement::price,
    prompt,
    session::Session,
    session_store::PersistedSession,
    stop::StopTracker,
};
use polaris_provider::{
    Provider, ProviderError, UsageMeter,
    attempts::{AttemptBudget, AttemptContext, AttemptKind, AttemptLedger},
    codex::{CodexProvider, ENDPOINT_BASE},
};
use polaris_sandbox::{SandboxMode, SandboxPolicy};
use serde_json::{Value, json};
use std::{
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const ROOT_MODEL: &str = "gpt-6-astra";
const EFFORT: &str = "xhigh";
const REQUEST_BYTES: usize = 131_072;

#[cfg(unix)]
fn save_new(path: &Path, value: &Value) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| "evidence already exists or cannot be created")?;
    serde_json::to_writer(&mut file, value).map_err(|_| "evidence encoding failed")?;
    file.write_all(b"\n")
        .and_then(|()| file.sync_all())
        .map_err(|_| "evidence persistence failed".into())
}

fn mode_name(mode: InputPreparationMode) -> &'static str {
    match mode {
        InputPreparationMode::Off => "off",
        InputPreparationMode::RepeatRefs => "repeat_refs",
    }
}

fn projection(case: &Case, mode: InputPreparationMode) -> Result<(String, Value), String> {
    let prepared = prepare(&case.input, mode);
    let reason = if mode == InputPreparationMode::Off {
        "off"
    } else {
        &case.expected_projection
    };
    if prepared.metrics.reason.as_str() != reason {
        return Err("projection differs from fixed expectation".into());
    }
    let sent = match prepared.projection {
        Some(projected) if projected.reconstruct().as_deref() == Some(&case.input) => {
            projected.encoded
        }
        Some(_) => return Err("input reconstruction failed".into()),
        None => case.input.clone(),
    };
    Ok((
        sent,
        json!({"source_sha256":case.input_hash,"preparation":prepared.metrics}),
    ))
}

fn plan(args: &Args, case: &Case) -> Result<Value, String> {
    let mut plan = case.plan(args);
    let conditions = case.modes().into_iter().map(|mode| {
        let (sent, preparation) = projection(case, mode)?;
        Ok(json!({"mode":mode_name(mode),"user_sha256":content_hash(sent.as_bytes()),
            "user_bytes":sent.len(),"user_reference_tokens":polaris_core::budget::count_tokens(&sent),
            "preparation":preparation}))
    }).collect::<Result<Vec<Value>, String>>()?;
    plan["conditions"] = json!(conditions);
    plan["response_format"] = json!(case.instructions());
    Ok(plan)
}

struct Deny;
impl Approver for Deny {
    fn ask(&mut self, _: &str) -> Decision {
        Decision::Deny
    }
}

struct Trial {
    outcome: Result<agent::AgentOutcome, &'static str>,
    usage: polaris_provider::UsageReport,
    raw_verified: bool,
}

async fn run_agent(
    case: &Case,
    mode: InputPreparationMode,
    provider: Arc<dyn Provider>,
    directory: &Path,
) -> Result<Trial, String> {
    fs::create_dir(directory).map_err(|_| "trial storage exists or is unavailable")?;
    let storage = PersistedSession::create(
        directory,
        &directory.join("memory.sqlite"),
        "m12-synthetic",
        HistoryMode::Legacy,
        None,
    )
    .map_err(|_| "durable session unavailable")?;
    let mut session = Session {
        compaction_threshold: Some(usize::MAX),
        disable_files_md_auto_regenerate: true,
        input_preparation: mode,
        ..Session::new()
    };
    session
        .attach(storage)
        .map_err(|_| "session attach failed")?;
    session.push_user(&case.input);
    session
        .check_persistence()
        .map_err(|_| "raw input persistence failed")?;
    let persistence = session
        .persistence
        .as_ref()
        .ok_or("persistence missing")?
        .clone();
    let before = persistence.snapshot().map_err(|_| "raw snapshot failed")?;
    if before.events.len() != 1 || before.events[0].message.content != case.input {
        return Err("original input was not saved exactly once".into());
    }
    let original = serde_json::to_value(&before.events).map_err(|_| "snapshot encoding failed")?;
    let meter = UsageMeter::default();
    let provider = meter.wrap(provider);
    let sandbox =
        SandboxPolicy::new(SandboxMode::ReadOnly, &[]).map_err(|_| "sandbox unavailable")?;
    let mut gate = Gate::new(ApprovalPolicy::Never);
    let mut deny = Deny;
    let mut context = ToolContext {
        sandbox: &sandbox,
        helper: directory,
        gate: &mut gate,
        approver: &mut deny,
    };
    let always = prompt::assemble_always_on("", "M12 synthetic input evaluation", &[]);
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        agent::run(
            &provider,
            &mut session,
            Arc::new(tokio::sync::Mutex::new(
                AuditLog::open(&directory.join("audit.jsonl")).map_err(|_| "audit unavailable")?,
            )),
            &mut StopTracker::new(2),
            &always,
            &[],
            &[],
            Arc::new(guard::NoChildren),
            1,
            1,
            None,
            &mut context,
        ),
    )
    .await;
    let outcome = match result {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(_)) => Err("agent failed; inspect physical evidence"),
        Err(_) => Err("trial deadline reached"),
    };
    let reopened = PersistedSession::open(
        &persistence.data_root,
        &persistence.database,
        &before.state.project_id,
        &before.state.session_id,
    )
    .and_then(|store| store.snapshot());
    let raw_verified = reopened.is_ok_and(|snapshot| {
        snapshot.events.len() >= before.events.len()
            && serde_json::to_value(&snapshot.events[..before.events.len()])
                .ok()
                .as_ref()
                == Some(&original)
    }) && session
        .messages
        .first()
        .is_some_and(|message| message.content == case.input);
    Ok(Trial {
        outcome,
        usage: meter.snapshot(),
        raw_verified,
    })
}

#[cfg(unix)]
fn live_environment(args: &Args) -> Result<(), String> {
    if std::env::var("POLARIS_AUTH_READ_ONLY").as_deref() != Ok("1")
        || std::env::var("POLARIS_WS").as_deref() != Ok("off")
        || std::env::var_os("POLARIS_METRICS_PATH").map(PathBuf::from)
            != Some(args.output.join("metrics.jsonl"))
        || [
            "POLARIS_CACHE_NAMESPACE",
            "POLARIS_CACHE_MODE",
            "POLARIS_CACHE_PREFIX",
            "POLARIS_CACHE_PACING",
            "POLARIS_TURN_AFFINITY",
        ]
        .iter()
        .any(|key| std::env::var_os(key).is_some())
    {
        return Err("fixed readonly HTTP measurement environment required".into());
    }
    let rates = args.work_dir.join("rates.json");
    if rates.canonicalize().ok().as_deref() != Some(&rates)
        || !fs::symlink_metadata(&rates).is_ok_and(|m| m.is_file() && m.len() <= 65_536)
        || content_hash(&fs::read(rates).map_err(|_| "rates unavailable")?)
            != content_hash(price::TABLE.as_bytes())
    {
        return Err("frozen rates differ from embedded rates".into());
    }
    Ok(())
}

#[cfg(unix)]
async fn live(args: &Args, case: &Case) -> Result<(), String> {
    live_environment(args)?;
    let plan = plan(args, case)?;
    // An exclusive mkdir also arbitrates direct concurrent drivers.
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&args.output)
        .map_err(|_| "output exists or cannot be claimed")?;
    save_new(&args.output.join("plan.json"), &plan)?;
    save_new(
        &args.output.join("started.json"),
        &json!({"state":"started",
        "plan_sha256":content_hash(plan.to_string().as_bytes())}),
    )?;
    let ledger = AttemptLedger::open_observed(
        &args.output.join("attempts.json"),
        AttemptContext::default(),
        AttemptBudget::new(2).map_err(|_| "invalid send cap")?,
    )
    .map_err(|_| "attempt ledger unavailable")?;
    let tokens = eval_auth::tokens(
        args.auth_file.as_deref().ok_or("auth file required")?,
        EFFORT,
    )?;
    let provider = CodexProvider::new(ENDPOINT_BASE.into(), ROOT_MODEL.into(), tokens)
        .map_err(|_| "provider initialization failed")?
        .with_attempt_ledger(ledger.clone())
        .with_visible_request_byte_limit(REQUEST_BYTES);
    provider.set_effort(Some(EFFORT));
    let provider: Arc<dyn Provider> = Arc::new(provider);
    let mut cost = 0u128;
    let fee = args.fee_cap_microusd.ok_or("fee stop required")?;
    for mode in case.modes() {
        let current = Case::load(args)?;
        if current.case_file_hash != case.case_file_hash {
            return Err("frozen cases changed".into());
        }
        let (expected, preparation) = projection(case, mode)?;
        let id = format!("{}-{}", case.id, mode_name(mode));
        let first_attempt = ledger.snapshot().len();
        let first_metric = if first_attempt == 0 {
            0
        } else {
            accounting::read_metrics(&args.output.join("metrics.jsonl"))?.len()
        };
        let remaining = u128::from(fee)
            .checked_sub(cost)
            .filter(|remaining| *remaining > 0)
            .ok_or("observed fee stop reached")? as u64;
        let guard = Arc::new(guard::Guard::new(
            provider.clone(),
            expected,
            case.instructions(),
            args.output.join("metrics.jsonl"),
            first_metric,
            remaining,
        ));
        let start = Instant::now();
        let trial = run_agent(case, mode, guard.clone(), &args.output.join(&id)).await?;
        let attempts = ledger.snapshot();
        let attempts = attempts.get(first_attempt..).unwrap_or_default();
        let rows = accounting::read_metrics(&args.output.join("metrics.jsonl")).unwrap_or_default();
        let rows = rows.get(first_metric..).unwrap_or_default();
        let accounting = accounting::reconcile(
            attempts,
            rows,
            &trial.usage,
            EFFORT,
            &[(AttemptKind::Completion, ROOT_MODEL)],
        );
        let evidence = guard.evidence.lock().expect("evidence lock");
        let consistent = trial.outcome.as_ref().is_ok_and(|outcome| {
            outcome.usage_report == trial.usage && outcome.usage == trial.usage.usage
        });
        let complete = attempts.len() == 1
            && accounting.is_ok()
            && consistent
            && trial.raw_verified
            && evidence.error.is_none()
            && evidence.request.is_some();
        let exact = accounting
            .as_ref()
            .is_ok_and(|value| value["exact_fee_complete"] == true);
        let score = trial
            .outcome
            .as_ref()
            .ok()
            .map(|outcome| case.score(&outcome.text));
        let response_path = format!("{id}.response.json");
        save_new(
            &args.output.join(&response_path),
            &json!({"response":evidence.response}),
        )?;
        let result = json!({"schema_version":1,"case":case.id,"mode":mode_name(mode),
            "observation_complete":complete,"measurement_complete":complete && exact,
            "score":score,"physical_sends":attempts.len(),"usage_report":trial.usage,
            "accounting":accounting.as_ref().ok(),"accounting_error":accounting.as_ref().err(),
            "attempts":attempts,"metrics":rows,"request":evidence.request,"preparation":preparation,
            "raw_original_verified_after_reopen":trial.raw_verified,"output_diagnostics":evidence.diagnostics,
            "response_file":response_path,"response_sha256":content_hash(&fs::read(args.output.join(&response_path))
                .map_err(|_| "response evidence unavailable")?),
            "known_cost_upper_microusd":evidence.known_cost_upper,"error":evidence.error,
            "provider_error":evidence.provider_error,
            "outcome_error":trial.outcome.as_ref().err(),"elapsed_seconds":start.elapsed().as_secs_f64(),
            "endpoint_output_cap_verified":false,"fee_ceiling_is_strict_billing_limit":false,
            "response_model_verified":false});
        save_new(&args.output.join(format!("{id}.result.json")), &result)?;
        if !complete {
            return Err("pair stopped; automatic retry prohibited".into());
        }
        cost += accounting.as_ref().expect("complete accounting")["upper_cost_microusd"]
            .as_u64()
            .ok_or("fee aggregate overflow")? as u128;
    }
    save_new(
        &args.output.join("complete.json"),
        &json!({"state":"complete","trials":2,
        "physical_sends":ledger.snapshot().len(),"observed_cost_upper_microusd":cost,
        "price_table_sha256":content_hash(price::TABLE.as_bytes())}),
    )?;
    println!(
        "{}",
        json!({"state":"complete","trials":2,"physical_sends":ledger.snapshot().len()})
    );
    Ok(())
}

#[cfg(unix)]
fn main() {
    let args = Args::parse();
    let result = (|| {
        args.validate()?;
        let case = Case::load(&args)?;
        if !args.live {
            println!("{}", plan(&args, &case)?);
            return Ok(());
        }
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "runtime unavailable")?
            .block_on(live(&args, &case))
    })();
    if let Err(reason) = result {
        eprintln!("M12比較を停止: {reason}");
        std::process::exit(1);
    }
}

#[cfg(not(unix))]
fn main() -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "native evaluation is unavailable on this platform",
    ))
}
