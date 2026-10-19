//! Offline scripted regressions through the real agent, read and spawn paths.
use super::*;
use polaris_provider::{
    CompletionRequest, CompletionResponse, ToolCall, Usage, UsageReport,
    attempts::{AttemptKind, AttemptRecord, AttemptStatus, BudgetReservation, UsageObservation},
    turn_affinity::TurnContext,
};
use std::{
    collections::VecDeque,
    sync::{
        Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};

const SENTINEL: &str = "RAW_SENTINEL_ONLY_THE_READING_AGENT_SEES";
fn bounds() -> Bounds {
    Bounds {
        root_sends: 5,
        child_sends: 5,
        trial_sends: 10,
        physical_sends: 15,
        request_bytes: 131_072,
        read_lines: 300,
        read_output_bytes: 24_576,
        observed_output_stop: 4096,
        trial_seconds: 240,
        child_seconds: 120,
    }
}
fn fixture() -> (tempfile::TempDir, Case) {
    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().canonicalize().unwrap();
    fs::create_dir(project.join(".polaris")).unwrap();
    let source = format!("// {SENTINEL}\npub const VALUE: u32 = 7;\n");
    let path = project.join("source.rs");
    fs::write(&path, &source).unwrap();
    let case = Case {
        id: "R01".into(),
        path,
        project,
        hash: content_hash(source.as_bytes()),
        question: "VALUEを答える".into(),
        expected: json!({"value":7}),
        evidence: json!({"value":{"line":2,"text":"pub const VALUE: u32 = 7;"}}),
        requires_gap: false,
    };
    case.verify().unwrap();
    (temp, case)
}
fn usage() -> Usage {
    Usage {
        input_tokens: 100,
        output_tokens: 10,
        total_tokens: 110,
        cached_tokens: 0,
    }
}
fn response(text: String, calls: Vec<ToolCall>) -> CompletionResponse {
    CompletionResponse {
        text,
        tool_calls: calls,
        usage: Some(usage()),
        ..Default::default()
    }
}
fn read(case: &Case) -> CompletionResponse {
    response(
        String::new(),
        vec![ToolCall {
            id: "read-1".into(),
            name: "read".into(),
            arguments: json!({"path":case.path,"offset":0,"limit":2}),
        }],
    )
}
fn spawn(case: &Case, limits: &Bounds) -> CompletionResponse {
    response(
        String::new(),
        vec![ToolCall {
            id: "spawn-1".into(),
            name: "spawn".into(),
            arguments: json!({"tasks":[{"type":"file-reader","task":case.child_task(limits)}]}),
        }],
    )
}
fn child_answer(case: &Case) -> CompletionResponse {
    response(
        json!({"path":case.path,"findings":[{"line":2,"text":"VALUEは7"}],"gaps":[]}).to_string(),
        vec![],
    )
}
fn answer() -> CompletionResponse {
    response(
        json!({"answer":{"value":7},"evidence":{"value":2},"gaps":[]}).to_string(),
        vec![],
    )
}

fn row(model: &str, id: &str, usage: Usage) -> Value {
    json!({"schema_version":1,"provider":"synthetic_no_network","attempt_id":id,"model":model,"effort":EFFORT,"outcome":"completed",
        "usage":{"input_tokens":usage.input_tokens,"output_tokens":usage.output_tokens,"cache_read_tokens":usage.cached_tokens,"cache_write_tokens":null,"reasoning_tokens":3}})
}
#[cfg(unix)]
struct Scripted {
    model: &'static str,
    steps: Mutex<VecDeque<Result<CompletionResponse, ProviderError>>>,
    requests: Mutex<Vec<CompletionRequest>>,
    calls: AtomicUsize,
    metrics: PathBuf,
}
#[cfg(unix)]
impl Scripted {
    fn new(
        model: &'static str,
        metrics: &Path,
        steps: Vec<Result<CompletionResponse, ProviderError>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            model,
            steps: Mutex::new(steps.into()),
            requests: Mutex::new(vec![]),
            calls: AtomicUsize::new(0),
            metrics: metrics.into(),
        })
    }
}
#[cfg(unix)]
#[async_trait::async_trait]
impl Provider for Scripted {
    async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        panic!("unscoped model call")
    }
    async fn complete_in_turn(
        &self,
        request: CompletionRequest,
        _: &TurnContext,
    ) -> Result<CompletionResponse, ProviderError> {
        let index = self.calls.fetch_add(1, Ordering::SeqCst);
        self.requests.lock().unwrap().push(request);
        let result = self
            .steps
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected model request");
        let known = match &result {
            Ok(response) => response.usage,
            Err(ProviderError::ReceivedUsage { usage, .. }) => Some(*usage),
            _ => None,
        };
        let mut metric = row(
            self.model,
            &format!("{}-{index}", self.model),
            known.unwrap_or_default(),
        );
        if known.is_none() {
            metric["usage"] = Value::Null;
        }
        if result.is_err() {
            metric["outcome"] = json!("decode_error");
        }
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600)
            .open(&self.metrics)
            .unwrap();
        writeln!(file, "{metric}").unwrap();
        result
    }
    fn model(&self) -> Option<String> {
        Some(self.model.into())
    }
}

#[cfg(unix)]
struct Rig {
    shared: Arc<guard::Shared>,
    trial: Arc<Mutex<guard::Trial>>,
    root: Arc<Scripted>,
    child: Arc<Scripted>,
    root_guard: Arc<dyn Provider>,
    child_guard: Arc<dyn Provider>,
}
#[cfg(unix)]
fn rig(
    case: &Case,
    arm: Arm,
    limits: &Bounds,
    root_steps: Vec<Result<CompletionResponse, ProviderError>>,
    child_steps: Vec<Result<CompletionResponse, ProviderError>>,
) -> Rig {
    let metrics = case.project.join("metrics.jsonl");
    let shared = Arc::new(guard::Shared::new(metrics.clone(), 40_000_000));
    let trial = Arc::new(Mutex::new(guard::Trial::default()));
    let root = Scripted::new(ROOT_MODEL, &metrics, root_steps);
    let child = Scripted::new(CHILD_MODEL, &metrics, child_steps);
    let root_guard = Arc::new(guard::Guard::new(
        root.clone(),
        guard::Role::Root,
        case.clone(),
        arm,
        limits.clone(),
        shared.clone(),
        trial.clone(),
    ));
    let child_guard = Arc::new(guard::Guard::new(
        child.clone(),
        guard::Role::Child,
        case.clone(),
        arm,
        limits.clone(),
        shared.clone(),
        trial.clone(),
    ));
    Rig {
        shared,
        trial,
        root,
        child,
        root_guard,
        child_guard,
    }
}
#[cfg(unix)]
async fn execute(case: &Case, arm: Arm, limits: &Bounds, rig: &Rig) -> TrialRun {
    run_agent(
        case,
        arm,
        limits,
        rig.root_guard.clone(),
        rig.child_guard.clone(),
        &case.project.join("audit.jsonl"),
    )
    .await
    .unwrap()
}

/// A read-only child type may carry `write_root`, because the shipped path
/// ignores it: `resolve_subagent_sandbox` restricts an `AgentAccess::Read`
/// type to ReadOnly and never looks at the field (`spawn.rs`). The advertised
/// schema offers `write_root` alongside `type` and `task`, and on 2026-09-20
/// the real root model attached it. Refusing it here failed a call production
/// accepts and left the arm unmeasurable for a reason that is not the question
/// being measured, so the field is recorded rather than rejected.
#[tokio::test]
#[cfg(unix)]
async fn a_read_only_child_task_may_declare_write_root_because_production_ignores_it() {
    let (_temp, case) = fixture();
    let limits = bounds();
    let mut first = spawn(&case, &limits);
    first.tool_calls[0].arguments["tasks"][0]["write_root"] = json!(case.project);
    let rig = rig(
        &case,
        Arm::Reader,
        &limits,
        vec![Ok(first), Ok(answer())],
        vec![Ok(read(&case)), Ok(child_answer(&case))],
    );
    let run = execute(&case, Arm::Reader, &limits, &rig).await;
    let outcome = run
        .outcome
        .expect("write_root must not halt a read-only child");
    assert_eq!(case.score(&outcome.text)["pass"], true);
    let diagnostic = rig.shared.report(&rig.trial);
    assert_eq!(diagnostic["halt"], serde_json::Value::Null);
    assert_eq!(diagnostic["spawns"], 1);
    assert_eq!(diagnostic["child_write_root_declared"], true);
}

#[tokio::test]
#[cfg(unix)]
async fn real_agent_read_and_spawn_keep_source_out_of_parent_and_account_once() {
    for arm in [Arm::Direct, Arm::Reader] {
        let (_temp, case) = fixture();
        let limits = bounds();
        let (root_steps, child_steps) = if arm == Arm::Direct {
            (vec![Ok(read(&case)), Ok(answer())], vec![])
        } else {
            (
                vec![Ok(spawn(&case, &limits)), Ok(answer())],
                vec![Ok(read(&case)), Ok(child_answer(&case))],
            )
        };
        let rig = rig(&case, arm, &limits, root_steps, child_steps);
        let run = execute(&case, arm, &limits, &rig).await;
        let outcome = run.outcome.unwrap();
        let count = if arm == Arm::Direct { 2 } else { 4 };
        assert_eq!(run.usage.reported_responses, count);
        assert_eq!(run.usage.usage.input_tokens, count as u32 * 100);
        assert_eq!(outcome.usage_report, run.usage);
        assert_eq!(case.score(&outcome.text)["pass"], true);
        let root = rig.root.requests.lock().unwrap();
        assert_eq!(root.len(), 2);
        let expected_tools: &[&str] = match arm {
            Arm::Direct => &["read"],
            Arm::Reader => &["read", "spawn"],
        };
        for request in root.iter() {
            assert_eq!(
                request
                    .tools
                    .iter()
                    .map(|tool| tool.name.as_str())
                    .collect::<Vec<_>>(),
                expected_tools,
                "root request tools must match {arm:?} on every turn"
            );
        }
        assert!(
            !root[0]
                .messages
                .iter()
                .any(|message| message.content.contains("expected_answer"))
        );
        assert_eq!(
            root[1]
                .messages
                .iter()
                .any(|message| message.content.contains(SENTINEL)),
            arm == Arm::Direct
        );
        if arm == Arm::Reader {
            let child = rig.child.requests.lock().unwrap();
            assert_eq!(child.len(), 2);
            assert!(
                child[1]
                    .messages
                    .iter()
                    .any(|message| message.content.contains(SENTINEL))
            );
            assert_eq!(child[0].tools.len(), 1);
            assert_eq!(child[0].tools[0].name, "read");
            assert!(
                rig.shared.report(&rig.trial)["child_result_bytes_received_by_parent"]
                    .as_u64()
                    .unwrap()
                    < 12_288
            );
        }
    }
}

#[tokio::test]
#[cfg(unix)]
async fn child_schema_retry_shares_budget_and_never_reloads_parent_history() {
    let (_temp, case) = fixture();
    let limits = bounds();
    let rig = rig(
        &case,
        Arm::Reader,
        &limits,
        vec![Ok(spawn(&case, &limits)), Ok(answer())],
        vec![
            Ok(read(&case)),
            Ok(response("{}".into(), vec![])),
            Ok(child_answer(&case)),
        ],
    );
    let run = execute(&case, Arm::Reader, &limits, &rig).await;
    assert!(run.outcome.is_ok());
    assert_eq!(rig.child.calls.load(Ordering::SeqCst), 3);
    assert_eq!(run.usage.reported_responses, 5);
    let child = rig.child.requests.lock().unwrap();
    assert!(
        child[2]
            .messages
            .iter()
            .any(|message| message.content.contains("did not match"))
    );
}

#[tokio::test]
#[cfg(unix)]
async fn child_limit_missing_usage_error_and_output_stop_block_next_parent_send() {
    for fault in ["cap", "missing", "error", "output"] {
        let (_temp, case) = fixture();
        let mut limits = bounds();
        let mut child_read = read(&case);
        let step = match fault {
            "cap" => {
                limits.trial_sends = 2;
                Ok(child_read)
            }
            "missing" => {
                child_read.usage = None;
                Ok(child_read)
            }
            "error" => Err(ProviderError::ReceivedUsage {
                source: Box::new(ProviderError::Decode("synthetic refusal".into())),
                usage: usage(),
            }),
            "output" => {
                child_read.usage = Some(Usage {
                    output_tokens: 4097,
                    total_tokens: 4197,
                    ..usage()
                });
                Ok(child_read)
            }
            _ => unreachable!(),
        };
        let rig = rig(
            &case,
            Arm::Reader,
            &limits,
            vec![Ok(spawn(&case, &limits))],
            vec![step],
        );
        let run = execute(&case, Arm::Reader, &limits, &rig).await;
        assert!(run.outcome.is_err(), "{fault}");
        assert_eq!(
            rig.root.calls.load(Ordering::SeqCst),
            1,
            "no second parent send: {fault}"
        );
        assert_eq!(rig.child.calls.load(Ordering::SeqCst), 1, "{fault}");
        assert!(rig.shared.ensure_running().is_err());
        // The error after a known child response is counted once, even when spawn renders it.
        assert_eq!(
            run.usage.reported_responses,
            if fault == "missing" { 1 } else { 2 },
            "{fault}"
        );
        assert_eq!(
            run.usage.usage.input_tokens,
            if fault == "missing" { 100 } else { 200 },
            "{fault}"
        );
        if fault == "error" {
            // Root's successful spawn response (1750) and the child's rejected
            // known response (37) both cost money; neither is counted twice.
            assert_eq!(rig.shared.cost(), 1787);
            assert_eq!(rig.shared.metric_count(), 2);
            let report = rig.shared.report(&rig.trial);
            assert_eq!(report["known_cost_upper_microusd"], 1787);
            assert_eq!(report["cost_coverage_complete"], false);
        }
        if fault == "missing" {
            assert_eq!(rig.shared.cost(), 1750);
            assert_eq!(
                rig.shared.report(&rig.trial)["cost_coverage_complete"],
                false
            );
        }
    }
}

#[tokio::test]
#[cfg(unix)]
async fn invalid_child_schema_twice_blocks_parent_even_without_provider_error() {
    let (_temp, case) = fixture();
    let limits = bounds();
    let rig = rig(
        &case,
        Arm::Reader,
        &limits,
        vec![Ok(spawn(&case, &limits))],
        vec![
            Ok(read(&case)),
            Ok(response("{}".into(), vec![])),
            Ok(response("{}".into(), vec![])),
        ],
    );
    let run = execute(&case, Arm::Reader, &limits, &rig).await;
    assert!(run.outcome.is_err());
    assert_eq!(rig.root.calls.load(Ordering::SeqCst), 1);
    assert_eq!(run.usage.reported_responses, 4);
}

#[tokio::test]
#[cfg(unix)]
async fn invalid_tool_paths_and_spawns_are_rejected_before_execution_with_usage() {
    for fault in [
        "answer-file",
        "write",
        "wrong-type",
        "wrong-task",
        "extra-field",
        "two-tasks",
        "no-type",
        "extra-task-field",
        "second-spawn",
        "direct-spawn",
        "unbounded-read",
    ] {
        let (_temp, case) = fixture();
        let limits = bounds();
        let arm = if matches!(
            fault,
            "answer-file" | "write" | "unbounded-read" | "direct-spawn"
        ) {
            Arm::Direct
        } else {
            Arm::Reader
        };
        let mut first = if arm == Arm::Direct {
            read(&case)
        } else {
            spawn(&case, &limits)
        };
        match fault {
            "direct-spawn" => first = spawn(&case, &limits),
            "answer-file" => {
                first.tool_calls[0].arguments["path"] = json!(case.project.join("cases.json"))
            }
            "write" => {
                first.tool_calls[0].name = "write".into();
                first.tool_calls[0].arguments = json!({"path":case.path,"content":"bad"});
            }
            "wrong-type" => {
                first.tool_calls[0].arguments["tasks"][0]["type"] = json!("file-inspector")
            }
            "wrong-task" => first.tool_calls[0].arguments["tasks"][0]["task"] = json!(SENTINEL),
            "extra-field" => first.tool_calls[0].arguments["untrusted"] = json!(SENTINEL),
            // The advertised schema calls `type` optional and says it is
            // matched from the task, so a model may leave it out. With the
            // judge disabled there is nothing to match it, and the arm cannot
            // run — a different defect from sending one field too many.
            "no-type" => {
                first.tool_calls[0].arguments["tasks"][0]
                    .as_object_mut()
                    .unwrap()
                    .remove("type");
            }
            "extra-task-field" => {
                first.tool_calls[0].arguments["tasks"][0]["note"] = json!(SENTINEL)
            }
            "two-tasks" => {
                let task = first.tool_calls[0].arguments["tasks"][0].clone();
                first.tool_calls[0].arguments["tasks"] = json!([task, task]);
            }
            "unbounded-read" => {
                first.tool_calls[0]
                    .arguments
                    .as_object_mut()
                    .unwrap()
                    .remove("limit");
            }
            _ => {}
        }
        let (root_steps, child_steps) = if fault == "second-spawn" {
            let mut second = spawn(&case, &limits);
            second.tool_calls[0].id = "spawn-2".into();
            (
                vec![Ok(first), Ok(second)],
                vec![Ok(read(&case)), Ok(child_answer(&case))],
            )
        } else {
            (vec![Ok(first)], vec![])
        };
        let rig = rig(&case, arm, &limits, root_steps, child_steps);
        let run = execute(&case, arm, &limits, &rig).await;
        assert!(run.outcome.is_err(), "{fault}");
        let reason = match fault {
            "answer-file" => "read escaped the fixed source path",
            "write" | "direct-spawn" => "tool prohibited for this arm",
            "wrong-type" => "unexpected child type",
            "wrong-task" => "child task differs from frozen prompt",
            "extra-field" => "unexpected spawn fields",
            "no-type" => "child task omitted type",
            "extra-task-field" => "unexpected child task fields",
            "two-tasks" => "one child task required",
            "second-spawn" => "second spawn prohibited",
            "unbounded-read" => "read escaped the fixed source path",
            _ => unreachable!(),
        };
        let diagnostic = rig.shared.report(&rig.trial);
        assert_eq!(diagnostic["halt"], reason, "{fault}");
        assert!(!diagnostic.to_string().contains(SENTINEL));
        assert_eq!(
            rig.child.calls.load(Ordering::SeqCst),
            if fault == "second-spawn" { 2 } else { 0 }
        );
        assert_eq!(
            run.usage.reported_responses,
            if fault == "second-spawn" { 4 } else { 1 }
        );
        case.verify().unwrap();
    }
}

fn attempt(model: &str, id: &str) -> AttemptRecord {
    let child = model == CHILD_MODEL;
    let usage = usage();
    AttemptRecord {
        logical_id: format!("logical-{id}"),
        attempt_id: id.into(),
        parent_id: Some(if child { "spawn:file-reader" } else { "trial" }.into()),
        kind: if child {
            AttemptKind::Child
        } else {
            AttemptKind::Completion
        },
        model: model.into(),
        started_at_ms: 1,
        finished_at_ms: Some(2),
        status: AttemptStatus::Succeeded,
        usage: UsageObservation::Known {
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            total_tokens: usage.total_tokens,
            cached_tokens: usage.cached_tokens,
        },
        retry_reason: None,
        reservation: BudgetReservation::default(),
    }
}
#[test]
fn physical_reconciliation_uses_both_models_and_rejects_duplicate_missing_or_contradictory_rows() {
    let attempts = vec![attempt(ROOT_MODEL, "root"), attempt(CHILD_MODEL, "child")];
    let rows = vec![
        row(ROOT_MODEL, "metric-root", usage()),
        row(CHILD_MODEL, "metric-child", usage()),
    ];
    let report = UsageReport {
        usage: Usage {
            input_tokens: 200,
            output_tokens: 20,
            total_tokens: 220,
            cached_tokens: 0,
        },
        reported_responses: 2,
        missing_responses: 0,
        failed_requests: 0,
    };
    let assessed = accounting::reconcile(&attempts, &rows, &report).unwrap();
    assert_eq!(assessed["exact_cost_microusd"], Value::Null);
    assert_eq!(assessed["upper_cost_microusd"], 1787);
    assert_eq!(assessed["by_model"][CHILD_MODEL]["physical_sends"], 1);
    for fault in [
        "duplicate",
        "missing",
        "cache",
        "reasoning",
        "model",
        "outcome",
    ] {
        let mut changed = rows.clone();
        match fault {
            "duplicate" => changed[1]["attempt_id"] = changed[0]["attempt_id"].clone(),
            "missing" => {
                changed.pop();
            }
            "cache" => changed[1]["usage"]["cache_write_tokens"] = json!(101),
            "reasoning" => changed[1]["usage"]["reasoning_tokens"] = json!(11),
            "model" => changed[1]["model"] = json!(ROOT_MODEL),
            "outcome" => changed[1]["outcome"] = json!("decode_error"),
            _ => unreachable!(),
        }
        assert!(
            accounting::reconcile(&attempts, &changed, &report).is_err(),
            "{fault}"
        );
    }
    let mut doubled = report;
    doubled.usage.input_tokens += 100;
    assert!(accounting::reconcile(&attempts, &rows, &doubled).is_err());
    for model in [ROOT_MODEL, CHILD_MODEL] {
        let mut metric = row(model, "exact", usage());
        metric["usage"]["cache_write_tokens"] = json!(70);
        let cost = accounting::assess_row(&metric, model, usage()).unwrap();
        assert!(
            cost.exact
                .is_some_and(|exact| cost.lower <= exact && exact <= cost.upper)
        );
    }
}

#[test]
fn default_is_no_send_and_caps_cannot_expand() {
    let args = Args::try_parse_from([
        "m23_read_eval",
        "--work-dir",
        "/tmp/work",
        "--cases",
        "/tmp/work/cases.json",
        "--output",
        "/tmp/work/out",
    ])
    .unwrap();
    assert!(!args.live);
    assert!(args.auth_file.is_none());
    assert!(args.fee_cap_microusd.is_none());
    let mut limits = bounds();
    limits.physical_sends = 16;
    assert!(limits.validate().is_err());
    limits = bounds();
    limits.child_sends = 6;
    assert!(limits.validate().is_err());
    limits = bounds();
    limits.request_bytes = 131_073;
    assert!(limits.validate().is_err());
}

#[test]
fn websocket_refresh_environment_and_started_restart_are_refused() {
    let env = HashMap::from([
        ("POLARIS_AUTH_READ_ONLY", "1"),
        ("POLARIS_METRICS_PATH", "/tmp/metrics.jsonl"),
        ("POLARIS_READ_OUTPUT_BYTES", "24576"),
    ]);
    let check = |env: &HashMap<&str, &str>| {
        input::check_environment(
            |key| env.get(key).map(|value| value.to_string()),
            Path::new("/tmp/metrics.jsonl"),
            24_576,
        )
    };
    check(&env).unwrap();
    let mut changed = env.clone();
    changed.insert("POLARIS_WS", "on");
    assert!(check(&changed).is_err());
    let mut changed = env;
    changed.insert("POLARIS_AUTH_READ_ONLY", "0");
    assert!(check(&changed).is_err());
    #[cfg(unix)]
    {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().canonicalize().unwrap().join("output");
        input::claim_output(&out).unwrap();
        save_new(&out.join("started.json"), &json!({"state":"started"})).unwrap();
        assert!(input::claim_output(&out).is_err());
        let budget = AttemptBudget::new(15).unwrap();
        let ledger_path = out.join("attempts.json");
        let ledger =
            AttemptLedger::open_observed(&ledger_path, AttemptContext::default(), budget).unwrap();
        let mut saved = ledger.persisted();
        drop(ledger);
        let mut running = attempt(ROOT_MODEL, "interrupted");
        running.status = AttemptStatus::Running;
        running.finished_at_ms = None;
        saved.records.push(running);
        fs::write(&ledger_path, serde_json::to_vec(&saved).unwrap()).unwrap();
        let restored =
            AttemptLedger::open_observed(&ledger_path, AttemptContext::default(), budget).unwrap();
        assert!(restored.persisted().halted);
    }
}

#[tokio::test]
async fn refresh_and_unscoped_completion_never_reach_a_model() {
    let tokens = FixedTokens {
        token: Token {
            access_token: "synthetic".into(),
            account_id: "synthetic".into(),
            effort: Some(EFFORT.into()),
        },
        expires_at: u64::MAX,
    };
    assert!(tokens.refreshed().await.is_err());
    #[cfg(unix)]
    {
        let (_temp, case) = fixture();
        let limits = bounds();
        let rig = rig(&case, Arm::Direct, &limits, vec![], vec![]);
        assert!(
            rig.root_guard
                .complete(CompletionRequest {
                    system: String::new(),
                    messages: vec![],
                    tools: vec![]
                })
                .await
                .is_err()
        );
        assert_eq!(rig.root.calls.load(Ordering::SeqCst), 0);
        assert!(rig.shared.ensure_running().is_err());
    }
}
