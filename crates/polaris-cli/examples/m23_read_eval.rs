//! Opt-in M23 comparison through the production agent/read/spawn path.
//! Default mode only prints the frozen plan; live mode requires an explicit fee stop.
//! Output/fee stops are observations after a response, not endpoint billing caps.

#[path = "m23_read_eval/accounting.rs"]
mod accounting;
#[path = "m23_read_eval/guard.rs"]
mod guard;
#[path = "m23_read_eval/input.rs"]
mod input;
#[cfg(test)]
#[path = "m23_read_eval/tests.rs"]
mod tests;

use clap::Parser;
use input::{Args, Arm, Bounds, Case, Plan};
use polaris_core::{
    agent::{self, ToolContext},
    approval::{ApprovalPolicy, Approver, Decision, Gate},
    audit::AuditLog,
    conversation_state::content_hash,
    measurement::price,
    prompt,
    session::Session,
    stop::StopTracker,
};
use polaris_provider::{
    Provider, ProviderError, TokenSource, UsageMeter,
    attempts::{AttemptBudget, AttemptContext, AttemptLedger},
    codex::{CodexProvider, ENDPOINT_BASE},
    role::RoleProvider,
};
use polaris_sandbox::{SandboxMode, SandboxPolicy};
use serde_json::{Value, json};
use std::{
    collections::{BTreeSet, HashMap},
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
    time::{Duration, Instant},
};

const ROOT_MODEL: &str = "gpt-6-astra";
const CHILD_MODEL: &str = "gpt-5.6-luna";
const EFFORT: &str = "xhigh";

#[cfg(not(unix))]
fn save_new(_path: &Path, _value: &Value) -> Result<(), String> {
    Err("native evaluation is unavailable on this platform".into())
}

#[cfg(unix)]
fn save_new(path: &Path, value: &Value) -> Result<(), String> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|_| "result already exists or cannot be created")?;
    serde_json::to_writer(&mut file, value).map_err(|_| "result encoding failed")?;
    file.write_all(b"\n")
        .and_then(|()| file.sync_all())
        .map_err(|_| "result persistence failed".into())
}

#[path = "eval_support/auth.rs"]
mod eval_auth;
#[cfg(test)]
use eval_auth::FixedTokens;
#[cfg(test)]
use polaris_provider::Token;

fn tokens(path: &Path) -> Result<Arc<dyn TokenSource>, String> {
    eval_auth::tokens(path, EFFORT)
}

fn live_provider(
    model: &str,
    tokens: Arc<dyn TokenSource>,
    ledger: &AttemptLedger,
    bounds: &Bounds,
) -> Result<Arc<dyn Provider>, String> {
    let provider = CodexProvider::new(ENDPOINT_BASE.into(), model.into(), tokens)
        .map_err(|_| "provider initialization failed")?
        .with_attempt_ledger(ledger.clone())
        .with_visible_request_byte_limit(bounds.request_bytes);
    // RequestCaps is intentionally absent: this endpoint's output contract is unverified.
    provider.set_effort(Some(EFFORT));
    Ok(Arc::new(provider))
}

fn reader(bounds: &Bounds) -> Result<polaris_skills::AgentType, String> {
    let mut reader = polaris_skills::builtin::agent_types()
        .into_iter()
        .find(|agent| agent.name == "file-reader")
        .ok_or("bundled file-reader missing")?;
    if reader.tier.name() != "dwarf"
        || reader.allowed_tools != ["read"]
        || reader.access != polaris_skills::AgentAccess::Read
        || reader.continuation
        || reader.wall_seconds != 120
        || reader.max_turns != 6
    {
        return Err("bundled file-reader contract changed".into());
    }
    reader.wall_seconds = bounds.child_seconds;
    reader.max_turns = bounds.child_sends + 1;
    Ok(reader)
}

struct Deny;
impl Approver for Deny {
    fn ask(&mut self, _: &str) -> Decision {
        Decision::Deny
    }
}

struct TrialRun {
    outcome: Result<agent::AgentOutcome, String>,
    usage: polaris_provider::UsageReport,
    by_model: std::collections::BTreeMap<String, polaris_provider::Usage>,
    session: Session,
}

async fn run_agent(
    case: &Case,
    arm: Arm,
    bounds: &Bounds,
    root: Arc<dyn Provider>,
    child: Arc<dyn Provider>,
    audit_path: &Path,
) -> Result<TrialRun, String> {
    if root.model().as_deref() != Some(ROOT_MODEL) || child.model().as_deref() != Some(CHILD_MODEL)
    {
        return Err("effective model mismatch".into());
    }
    let pool: Arc<dyn Provider> = Arc::new(RoleProvider::tiers_only(
        root.clone(),
        HashMap::from([("dwarf".into(), child)]),
    ));
    if pool
        .resolve_role("file-reader")
        .map_err(|_| "role routing failed")?
        .is_some()
        || pool
            .resolve_tier("dwarf")
            .map_err(|_| "tier routing failed")?
            .and_then(|p| p.model())
            .as_deref()
            != Some(CHILD_MODEL)
    {
        return Err("file-reader must use the fixed dwarf provider".into());
    }
    let meter = UsageMeter::default();
    let root = meter.wrap(root);
    let pool: Arc<dyn Provider> = Arc::new(meter.wrap(pool));
    let mut session = Session {
        compaction_threshold: Some(usize::MAX),
        disable_files_md_auto_regenerate: true,
        ..Session::new()
    };
    session.push_user(&case.prompt(arm, bounds));
    let always = prompt::assemble_always_on("", "M23 isolated reading evaluation", &[])
        .with_project_root(&case.project)
        .map_err(|_| "invalid project root")?;
    let sandbox = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).map_err(|_| "invalid sandbox")?;
    let mut gate = Gate::new(ApprovalPolicy::Never);
    let mut deny = Deny;
    let mut ctx = ToolContext {
        sandbox: &sandbox,
        helper: &case.project,
        gate: &mut gate,
        approver: &mut deny,
    };
    let audit = Arc::new(tokio::sync::Mutex::new(
        AuditLog::open(audit_path).map_err(|_| "audit unavailable")?,
    ));
    let mut stop = StopTracker::new(bounds.root_sends + 1);
    let types = match arm {
        Arm::Direct => Vec::new(),
        Arm::Reader => vec![reader(bounds)?],
    };
    let result = tokio::time::timeout(
        Duration::from_secs(bounds.trial_seconds.into()),
        agent::run(
            &root,
            &mut session,
            audit,
            &mut stop,
            &always,
            &[],
            &types,
            pool,
            1,
            1,
            None,
            &mut ctx,
        ),
    )
    .await;
    let outcome = match result {
        Ok(Ok(outcome)) => Ok(outcome),
        Ok(Err(_)) => Err("agent stopped; inspect bounded guard and physical evidence".into()),
        Err(_) => Err("trial wall-clock limit reached".into()),
    };
    Ok(TrialRun {
        outcome,
        usage: meter.snapshot(),
        by_model: meter.by_model(),
        session,
    })
}

async fn live(args: &Args, plan: Plan) -> Result<(), String> {
    input::live_environment(args)?;
    let rates_path = args.work_dir.join("rates.json");
    input::canonical_file(&rates_path)?;
    if fs::metadata(&rates_path)
        .map_err(|_| "frozen rates unavailable")?
        .len()
        > 65_536
        || content_hash(&fs::read(&rates_path).map_err(|_| "frozen rates unavailable")?)
            != content_hash(price::TABLE.as_bytes())
    {
        return Err("frozen rates differ from the binary's embedded rates".into());
    }
    input::claim_output(&args.output)?;
    let manifest = plan.manifest(args);
    save_new(&args.output.join("plan.json"), &manifest)?;
    save_new(
        &args.output.join("started.json"),
        &json!({"state":"started","plan_sha256":content_hash(manifest.to_string().as_bytes())}),
    )?;
    let ledger = AttemptLedger::open_observed(
        &args.output.join("attempts.json"),
        AttemptContext::default(),
        AttemptBudget::new(args.bounds.physical_sends).map_err(|_| "invalid physical cap")?,
    )
    .map_err(|_| "durable attempt ledger unavailable")?;
    let token_source = tokens(
        args.auth_file
            .as_ref()
            .ok_or("explicit auth-file required")?,
    )?;
    let root = live_provider(ROOT_MODEL, token_source.clone(), &ledger, &args.bounds)?;
    let child = live_provider(CHILD_MODEL, token_source, &ledger, &args.bounds)?;
    let shared = Arc::new(guard::Shared::new(
        args.output.join("metrics.jsonl"),
        args.fee_cap_microusd.ok_or("explicit fee stop required")?,
    ));
    let mut results = Vec::new();
    for entry in &plan.schedule {
        let case = &plan.cases[entry.case_index];
        case.verify()?;
        shared.ensure_running().map_err(|_| "shared run stopped")?;
        let before: BTreeSet<_> = ledger
            .snapshot()
            .iter()
            .map(|row| row.attempt_id.clone())
            .collect();
        let first_metric = shared.metric_count();
        let trial = Arc::new(std::sync::Mutex::new(guard::Trial::default()));
        save_new(
            &args.output.join(format!("{}.started.json", entry.id)),
            &entry.json(case),
        )?;
        let wrap = |inner, role| -> Arc<dyn Provider> {
            Arc::new(guard::Guard::new(
                inner,
                role,
                case.clone(),
                entry.arm,
                args.bounds.clone(),
                shared.clone(),
                trial.clone(),
            ))
        };
        let started = Instant::now();
        let observed = run_agent(
            case,
            entry.arm,
            &args.bounds,
            wrap(root.clone(), guard::Role::Root),
            wrap(child.clone(), guard::Role::Child),
            &args.output.join(format!("{}.audit.jsonl", entry.id)),
        )
        .await?;
        let attempts: Vec<_> = ledger
            .snapshot()
            .into_iter()
            .filter(|row| !before.contains(&row.attempt_id))
            .collect();
        let rows = accounting::read_metrics(&shared.metrics).unwrap_or_default();
        let rows = rows.get(first_metric..).unwrap_or_default();
        let reconciliation = accounting::reconcile(&attempts, rows, &observed.usage);
        let score = observed
            .outcome
            .as_ref()
            .ok()
            .map(|outcome| case.score(&outcome.text));
        let outcome_consistent = observed.outcome.as_ref().is_ok_and(|outcome| {
            outcome.usage_report == observed.usage && outcome.usage == observed.usage.usage
        });
        let complete =
            reconciliation.is_ok() && outcome_consistent && shared.ensure_running().is_ok();
        let exact_fee_complete = reconciliation
            .as_ref()
            .is_ok_and(|value| value["exact_fee_complete"] == true);
        let answer = observed
            .outcome
            .as_ref()
            .ok()
            .map(|outcome| outcome.text.as_str());
        let result = json!({
            "schema_version":1,"trial":entry.json(case),"observation_complete":complete,
            "measurement_complete":complete && exact_fee_complete,"exact_fee_complete":exact_fee_complete,
            "observation_source":"codex_http_metrics_and_durable_attempt_ledger",
            "answer":answer,"score":score,"usage_report":observed.usage,"usage_by_model":observed.by_model,
            "physical_sends":attempts.len(),"attempts":attempts,"metrics":rows,
            "elapsed_seconds":started.elapsed().as_secs_f64(),"guard":shared.report(&trial),
            "accounting_error":reconciliation.as_ref().err(),"accounting":reconciliation.as_ref().ok(),"outcome_accounting_matches":outcome_consistent,
            "known_pair_cost_upper_microusd":shared.cost(),"cost_coverage_complete":complete,
            "error":observed.outcome.as_ref().err(),"root_messages":observed.session.messages,
            "response_model_verified":false,"endpoint_output_cap_verified":false
        });
        save_new(
            &args.output.join(format!("{}.result.json", entry.id)),
            &result,
        )?;
        results.push(result);
        if !complete {
            shared.halt("trial incomplete");
            return Err(
                "measurement stopped; existing output cannot be resumed automatically".into(),
            );
        }
    }
    save_new(
        &args.output.join("complete.json"),
        &json!({"schema_version":1,"state":"complete","trials":results.len(),"physical_sends":ledger.snapshot().len(),"observed_cost_upper_microusd":shared.cost(),"price_table_sha256":content_hash(price::TABLE.as_bytes())}),
    )?;
    println!(
        "{}",
        json!({"state":"complete","output":args.output,"trials":results.len()})
    );
    Ok(())
}

#[cfg(unix)]
fn main() {
    let args = Args::parse();
    let result = (|| {
        args.validate()?;
        let plan = Plan::load(&args)?;
        if !args.live {
            println!(
                "{}",
                serde_json::to_string_pretty(&plan.manifest(&args))
                    .map_err(|_| "plan encoding failed")?
            );
            return Ok(());
        }
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|_| "runtime unavailable")?
            .block_on(live(&args, plan))
    })();
    if let Err(reason) = result {
        eprintln!("M23比較を停止: {reason}");
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
