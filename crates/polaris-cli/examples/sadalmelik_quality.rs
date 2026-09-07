//! Offline run-loop fixture driver.  Live Codex is deliberately blocked.
use async_trait::async_trait;
use polaris_core::{
    agent::{self, ToolContext},
    approval::{ApprovalPolicy, Approver, Decision, Gate},
    audit::AuditLog,
    prompt,
    session::Session,
    stop::StopTracker,
};
use polaris_provider::{
    CompletionRequest, CompletionResponse, Message, Provider, ProviderError,
    turn_affinity::TurnContext,
};
use polaris_sandbox::{SandboxMode, SandboxPolicy};
use serde_json::{Value, json};
use std::{
    fs,
    io::Write,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
const MODEL: &str = "gpt-6-astra";
const EFFORT: &str = "medium";
struct Args {
    case: PathBuf,
    fixture: PathBuf,
    output: PathBuf,
    live: bool,
}
fn args() -> Result<Args, String> {
    let mut v = std::env::args_os().skip(1);
    let (mut c, mut f, mut o) = (None, None, None);
    let mut live = false;
    while let Some(x) = v.next() {
        match x.to_str() {
            Some("--case") => c = v.next().map(PathBuf::from),
            Some("--fixture") => f = v.next().map(PathBuf::from),
            Some("--output") => o = v.next().map(PathBuf::from),
            Some("--live") => live = true,
            _ => {
                return Err(
                    "usage: sadalmelik_quality --case ABS --fixture ABS --output ABS [--live]"
                        .into(),
                );
            }
        }
    }
    let a = |p: Option<PathBuf>| -> Result<PathBuf, String> {
        match p {
            Some(x) if x.is_absolute() => Ok(x),
            _ => Err("all paths must be absolute".into()),
        }
    };
    Ok(Args {
        case: a(c)?,
        fixture: a(f)?,
        output: a(o)?,
        live,
    })
}
fn obj(v: &Value) -> Result<&serde_json::Map<String, Value>, String> {
    v.as_object().ok_or_else(|| "invalid case object".into())
}
fn text<'a>(o: &'a serde_json::Map<String, Value>, k: &str) -> Result<&'a str, String> {
    o.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("missing {k}"))
}
struct Fake {
    replies: Mutex<Vec<CompletionResponse>>,
    allowed: Vec<String>,
    calls: AtomicUsize,
    limit: usize,
}
impl Fake {
    fn new(replies: Vec<CompletionResponse>, n: u64, limit: usize) -> Result<Self, String> {
        let all = polaris_tools::all_specs();
        if all.len() != 6 {
            return Err("production tool definitions drifted".into());
        };
        let allowed = match n {
            0 => vec![],
            3 => all
                .iter()
                .filter(|t| matches!(t.name, "read" | "bash" | "skill"))
                .map(|t| t.name.to_string())
                .collect(),
            6 => all.iter().map(|t| t.name.to_string()).collect(),
            _ => return Err("invalid tool count".into()),
        };
        Ok(Self {
            replies: Mutex::new(replies),
            allowed,
            calls: AtomicUsize::new(0),
            limit,
        })
    }
    fn reset(&self) {
        self.calls.store(0, Ordering::SeqCst)
    }
    fn answer(&self, mut r: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        let n = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        if n > self.limit {
            return Err(ProviderError::Budget("fixture request cap reached".into()));
        };
        r.tools.retain(|t| self.allowed.iter().any(|x| x == t.name));
        self.replies
            .lock()
            .unwrap()
            .pop()
            .ok_or_else(|| ProviderError::Decode("fake response exhausted".into()))
    }
}
#[async_trait]
impl Provider for Fake {
    async fn complete(&self, r: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.answer(r)
    }
    async fn complete_in_turn(
        &self,
        r: CompletionRequest,
        _: &TurnContext,
    ) -> Result<CompletionResponse, ProviderError> {
        self.answer(r)
    }
}
struct Never;
impl Approver for Never {
    fn ask(&mut self, _: &str) -> Decision {
        Decision::Deny
    }
}
type FixtureFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = std::io::Result<T>> + Send + 'a>>;
struct OfflineMemory;
impl polaris_core::conversation_memory::StrictSummaryProvider for OfflineMemory {
    fn summarize<'a>(
        &'a self,
        request: polaris_core::conversation_memory::SummaryRequest,
    ) -> FixtureFuture<'a, String> {
        Box::pin(async move {
            Ok(json!({"facts":[format!("source {}",request.source_turn_id)],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[request.source_turn_id]}).to_string())
        })
    }
}
struct OfflineEmbedding(polaris_core::conversation_memory::EmbeddingModel);
impl polaris_core::conversation_memory::StrictEmbedder for OfflineEmbedding {
    fn metadata(&self) -> &polaris_core::conversation_memory::EmbeddingModel {
        &self.0
    }
    fn embed_passage<'a>(&'a self, _: &'a str) -> FixtureFuture<'a, Vec<f32>> {
        Box::pin(async { Ok(vec![1.0; 384]) })
    }
    fn embed_query<'a>(&'a self, _: &'a str) -> FixtureFuture<'a, Vec<Vec<f32>>> {
        Box::pin(async { Ok(vec![vec![1.0; 384]]) })
    }
}

async fn run(a: Args) -> Result<(), String> {
    use polaris_core::{
        conversation_state::HistoryMode,
        session_store::PersistedSession,
        workflow::{Phase, SessionWorkflow, WorkflowConfig},
    };
    if a.live {
        let caps = polaris_provider::attempts::RequestCaps {
            max_input_tokens: 32_000,
            max_output_tokens: 4096,
            max_hosted_actions: 0,
            endpoint_contract_verified: false,
        };
        caps.validate().map_err(|e| e.to_string())?;
        return Err("実接続の上限契約が未確認です".into());
    }
    let envelope: Value = serde_json::from_slice(&fs::read(&a.case).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    let trial = obj(&envelope)?;
    if text(trial, "model")? != MODEL || text(trial, "effort")? != EFFORT {
        return Err("model contract drifted".into());
    }
    let mode = text(trial, "mode")?;
    if !matches!(
        mode,
        "baseline" | "workflow" | "strict10" | "workflow-strict10"
    ) {
        return Err("unsupported fixture mode".into());
    }
    let strict = mode.contains("strict10");
    let workflow = mode.contains("workflow");
    let contract = obj(trial.get("contract").ok_or("contract missing")?)?;
    if text(contract, "kind")? != "conversation" {
        return Err("conversation fixtures required".into());
    }
    let case = obj(contract.get("case").ok_or("case missing")?)?;
    let turns = case
        .get("turns")
        .and_then(Value::as_array)
        .ok_or("turns missing")?;
    if !matches!(turns.len(), 1 | 12 | 36) {
        return Err("turn count drifted".into());
    }
    let runtime = obj(trial.get("runtime").ok_or("runtime missing")?)?;
    let cap = runtime
        .get("max_primary_requests_per_turn")
        .and_then(Value::as_u64)
        .ok_or("cap missing")? as usize;
    let stop: u32 = runtime
        .get("stop_tracker_limit")
        .and_then(Value::as_u64)
        .ok_or("stop missing")?
        .try_into()
        .map_err(|_| "invalid stop limit")?;
    let skills = polaris_skills::discover_in(&[a.fixture.join(".polaris/skills")]);
    if skills.skills.len()
        != case
            .get("skill_count")
            .and_then(Value::as_u64)
            .ok_or("skill count missing")? as usize
    {
        return Err("fixture skill count mismatch".into());
    }
    let replies = turns
        .iter()
        .rev()
        .map(|turn| {
            Ok(CompletionResponse {
                text: serde_json::to_string(obj(turn)?.get("expected").ok_or("expected missing")?)
                    .map_err(|e| e.to_string())?,
                ..Default::default()
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    let provider = Arc::new(Fake::new(
        replies,
        case.get("tool_count")
            .and_then(Value::as_u64)
            .ok_or("tool count missing")?,
        cap,
    )?);
    let always = prompt::assemble_always_on("", "sadalmelik offline fixture", &skills.skills);
    let sandbox = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).map_err(|e| e.to_string())?;
    let audit = Arc::new(tokio::sync::Mutex::new(
        AuditLog::open(&a.output.with_extension("audit.jsonl")).map_err(|e| e.to_string())?,
    ));
    let mut output = fs::File::create(&a.output).map_err(|e| e.to_string())?;
    let mut session = Session::new();
    session.compaction_threshold = Some(usize::MAX);
    for example in case
        .get("examples")
        .and_then(Value::as_array)
        .ok_or("examples missing")?
    {
        let example = obj(example)?;
        session.examples.push(Message::user(text(example, "user")?));
        session
            .examples
            .push(Message::assistant(text(example, "assistant")?));
    }
    let workflow_config = WorkflowConfig {
        enabled: workflow,
        initial_phase: Phase::Implement,
        always: vec!["builtin:workflow-core".into()],
        phase_skills: [
            (Phase::Implement, vec!["builtin:implement".into()]),
            (Phase::Review, vec!["builtin:review".into()]),
        ]
        .into(),
    };
    if workflow {
        session.workflow = Some(SessionWorkflow::new(workflow_config.clone()));
    }
    let data = a.output.with_extension("data");
    let database = data.join("memory.sqlite3");
    if strict || workflow {
        fs::create_dir_all(&data).map_err(|e| e.to_string())?;
        let saved = PersistedSession::create(
            &data,
            &database,
            "offline-fixture",
            if strict {
                HistoryMode::Strict10
            } else {
                HistoryMode::Legacy
            },
            session.workflow.as_ref(),
        )
        .map_err(|e| e.to_string())?;
        session.attach(saved).map_err(|e| e.to_string())?;
    }
    if strict {
        session.strict_history = Some(Arc::new(
            polaris_core::conversation_memory::StrictHistory::new(
                Arc::new(OfflineMemory),
                Arc::new(OfflineEmbedding(
                    polaris_core::conversation_memory::EmbeddingModel {
                        model: "offline-fixture".into(),
                        revision: "1".into(),
                        dimension: 384,
                    },
                )),
            ),
        ));
    }
    for (index, turn) in turns.iter().enumerate() {
        if index == 18
            && let Some(saved) = session.persistence.clone()
        {
            let current = saved.snapshot().map_err(|e| e.to_string())?;
            let replacement = if trial.get("task").and_then(Value::as_str) == Some("fork-phase") {
                saved
                    .fork(&polaris_core::conversation_state::content_hash(
                        b"offline-fixture",
                    ))
                    .map_err(|e| e.to_string())?
            } else {
                PersistedSession::open(
                    &data,
                    &database,
                    "offline-fixture",
                    &current.state.session_id,
                )
                .map_err(|e| e.to_string())?
            };
            session.workflow = if workflow {
                Some(
                    replacement
                        .restore_workflow(workflow_config.clone())
                        .map_err(|e| e.to_string())?,
                )
            } else {
                None
            };
            session.attach(replacement).map_err(|e| e.to_string())?;
            session.recover_history().await.map_err(|e| e.to_string())?;
        }
        if workflow && index == 18 {
            session
                .workflow
                .as_ref()
                .unwrap()
                .request_phase(Phase::Review)?;
            session.checkpoint().map_err(|e| e.to_string())?;
        }
        let turn = obj(turn)?;
        let before = session.messages.len();
        session.push_user(text(turn, "prompt")?);
        provider.reset();
        let mut tracker = StopTracker::new(stop);
        let mut gate = Gate::new(ApprovalPolicy::Never);
        let mut approver = Never;
        let mut context = ToolContext {
            sandbox: &sandbox,
            helper: &a.fixture,
            gate: &mut gate,
            approver: &mut approver,
        };
        let result = agent::run(
            provider.as_ref(),
            &mut session,
            audit.clone(),
            &mut tracker,
            &always,
            &skills.skills,
            &[],
            provider.clone(),
            1,
            0,
            None,
            &mut context,
        )
        .await
        .map_err(|e| e.to_string())?;
        session.check_persistence().map_err(|e| e.to_string())?;
        if strict
            && session
                .messages
                .iter()
                .filter(|m| m.role == polaris_provider::Role::User)
                .count()
                > 10
        {
            return Err("strict10 window exceeded".into());
        }
        let answer: Value = serde_json::from_str(&result.text).map_err(|e| e.to_string())?;
        if Some(&answer) != turn.get("expected") {
            return Err("offline response contract mismatch".into());
        }
        let row = json!({"id":text(trial,"id")?,"turn":index+1,"model":MODEL,"effort":EFFORT,"answer":answer,"scope_pass":true,"requests":provider.calls.load(Ordering::SeqCst),"offline_fake":true,"history_messages_before":before,"history_messages_after":session.messages.len(),"example_messages":session.examples.len(),"mode":mode});
        writeln!(output, "{row}").map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    let result = match args() {
        Ok(args) => run(args).await,
        Err(error) => Err(error),
    };
    if let Err(error) = result {
        eprintln!("{error}");
        std::process::exit(1);
    }
}
