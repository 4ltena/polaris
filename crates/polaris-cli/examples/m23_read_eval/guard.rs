//! Shared send, fee and tool admission guards for the parent and its one child.
use super::*;
use polaris_provider::{
    CompletionRequest, CompletionResponse, Message, Role as MessageRole, Usage,
    turn_affinity::TurnContext,
};
use std::{collections::BTreeMap, sync::Mutex};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Role {
    Root,
    Child,
}
impl Role {
    fn model(self) -> &'static str {
        match self {
            Self::Root => ROOT_MODEL,
            Self::Child => CHILD_MODEL,
        }
    }
}

#[derive(Default)]
struct State {
    halt: Option<&'static str>,
    in_flight: bool,
    admitted: u64,
    metric_count: usize,
    metric_ids: BTreeSet<String>,
    cost: u128,
}
pub(super) struct Shared {
    pub metrics: PathBuf,
    fee_cap: u64,
    state: Mutex<State>,
}
impl Shared {
    pub fn new(metrics: PathBuf, fee_cap: u64) -> Self {
        Self {
            metrics,
            fee_cap,
            state: Mutex::new(State::default()),
        }
    }
    pub fn halt(&self, reason: &'static str) {
        self.state
            .lock()
            .expect("guard lock")
            .halt
            .get_or_insert(reason);
    }
    pub fn ensure_running(&self) -> Result<(), ProviderError> {
        if self.state.lock().expect("guard lock").halt.is_some() {
            Err(budget("shared run stopped"))
        } else {
            Ok(())
        }
    }
    pub fn metric_count(&self) -> usize {
        self.state.lock().expect("guard lock").metric_count
    }
    pub fn cost(&self) -> u128 {
        self.state.lock().expect("guard lock").cost
    }
    pub fn report(&self, trial: &Mutex<Trial>) -> Value {
        let state = self.state.lock().expect("guard lock");
        let trial = trial.lock().expect("trial lock");
        json!({"halt":state.halt,"admitted_requests":state.admitted,"observed_metric_rows":state.metric_count,
            "known_cost_upper_microusd":state.cost,"fee_cap_microusd":self.fee_cap,
            "fee_scope":"known responses in this pair; unknown consumption is not zero",
            "cost_coverage_complete":state.halt.is_none(),"root_requests":trial.root_calls,"child_requests":trial.child_calls,
            "root_reads":trial.root_reads,"child_reads":trial.child_reads,"spawns":trial.spawns,
            "child_write_root_declared":trial.child_write_root_declared,
            "child_result_bytes_received_by_parent":trial.child_result_bytes})
    }
}

enum ExpectedTool {
    Read(String),
    Spawn,
}
#[derive(Default)]
pub(super) struct Trial {
    pub root_calls: u32,
    pub child_calls: u32,
    pub root_reads: u32,
    pub child_reads: u32,
    pub spawns: u32,
    child_write_root_declared: bool,
    child_started: Option<Instant>,
    child_result_bytes: usize,
    root_tools: BTreeMap<String, ExpectedTool>,
    child_tools: BTreeMap<String, ExpectedTool>,
}

pub(super) struct Guard {
    inner: Arc<dyn Provider>,
    role: Role,
    case: Case,
    arm: Arm,
    bounds: Bounds,
    shared: Arc<Shared>,
    trial: Arc<Mutex<Trial>>,
}
fn budget(reason: &'static str) -> ProviderError {
    ProviderError::Budget(reason.into())
}
fn received(error: ProviderError, usage: Option<Usage>) -> ProviderError {
    match (error, usage) {
        (error @ ProviderError::ReceivedUsage { .. }, _) => error,
        (source, Some(usage)) => ProviderError::ReceivedUsage {
            source: Box::new(source),
            usage,
        },
        (error, None) => error,
    }
}
fn usage_of(result: &Result<CompletionResponse, ProviderError>) -> Option<Usage> {
    match result {
        Ok(response) => response.usage,
        Err(ProviderError::ReceivedUsage { usage, .. }) => Some(*usage),
        Err(ProviderError::Observation {
            completed: Some(response),
            ..
        }) => response.usage,
        _ => None,
    }
}
struct Flight {
    shared: Arc<Shared>,
    finished: bool,
}
impl Drop for Flight {
    fn drop(&mut self) {
        let mut state = self.shared.state.lock().expect("guard lock");
        state.in_flight = false;
        if !self.finished {
            state.halt.get_or_insert("in-flight request cancelled");
        }
    }
}

impl Guard {
    pub fn new(
        inner: Arc<dyn Provider>,
        role: Role,
        case: Case,
        arm: Arm,
        bounds: Bounds,
        shared: Arc<Shared>,
        trial: Arc<Mutex<Trial>>,
    ) -> Self {
        Self {
            inner,
            role,
            case,
            arm,
            bounds,
            shared,
            trial,
        }
    }
    fn reject(&self, reason: &'static str) -> ProviderError {
        self.shared.halt(reason);
        budget(reason)
    }

    fn history(&self, messages: &[Message]) -> Result<(), ProviderError> {
        let mut trial = self.trial.lock().expect("trial lock");
        let expected = match self.role {
            Role::Root => &trial.root_tools,
            Role::Child => &trial.child_tools,
        };
        let mut child_bytes = None;
        for message in messages
            .iter()
            .filter(|message| message.role == MessageRole::Tool)
        {
            let Some(tool) = message
                .tool_call_id
                .as_ref()
                .and_then(|id| expected.get(id))
            else {
                return Err(budget("unrecognized tool result"));
            };
            match tool {
                ExpectedTool::Read(text) if message.content == *text => {}
                ExpectedTool::Read(_) => {
                    return Err(budget("read failed or source changed during execution"));
                }
                ExpectedTool::Spawn => {
                    let result: Value = serde_json::from_str(&message.content)
                        .map_err(|_| budget("child result malformed"))?;
                    let array = result
                        .as_array()
                        .filter(|array| array.len() == 1)
                        .ok_or_else(|| budget("child result count changed"))?;
                    if array[0]["ok"] != true
                        || array[0]["type"] != "file-reader"
                        || array[0]["result"]["path"].as_str() != self.case.path.to_str()
                        || message.content.len() > 12_288
                    {
                        return Err(budget("child failed or returned an unexpected result"));
                    }
                    child_bytes = Some(message.content.len());
                }
            }
        }
        if let Some(bytes) = child_bytes {
            trial.child_result_bytes = bytes;
        }
        Ok(())
    }

    fn prepare(
        &self,
        mut request: CompletionRequest,
    ) -> Result<(CompletionRequest, Flight), ProviderError> {
        self.shared.ensure_running()?;
        if self.inner.model().as_deref() != Some(self.role.model()) {
            return Err(self.reject("provider model changed"));
        }
        self.case
            .verify()
            .map_err(|_| self.reject("fixed fixture changed"))?;
        self.history(&request.messages)
            .map_err(|_| self.reject("prior tool execution failed"))?;
        let allows_spawn = self.role == Role::Root && self.arm == Arm::Reader;
        request
            .tools
            .retain(|tool| tool.name == "read" || (allows_spawn && tool.name == "spawn"));
        let names: BTreeSet<_> = request
            .tools
            .iter()
            .map(|tool| tool.name.as_str())
            .collect();
        let expected = if allows_spawn {
            BTreeSet::from(["read", "spawn"])
        } else {
            BTreeSet::from(["read"])
        };
        if names != expected {
            return Err(self.reject("expected arm-specific tool schema missing"));
        }
        let body = polaris_provider::codex::build_body(self.role.model(), &request, Some(EFFORT));
        if body.to_string().len() > self.bounds.request_bytes {
            return Err(self.reject("serialized request byte cap reached"));
        }
        let mut state = self.shared.state.lock().expect("guard lock");
        let mut trial = self.trial.lock().expect("trial lock");
        let limit = match self.role {
            Role::Root => self.bounds.root_sends,
            Role::Child => self.bounds.child_sends,
        };
        let count = match self.role {
            Role::Root => trial.root_calls,
            Role::Child => trial.child_calls,
        };
        if state.halt.is_some()
            || state.in_flight
            || state.cost >= u128::from(self.shared.fee_cap)
            || state.admitted >= self.bounds.physical_sends
            || count >= limit
            || trial.root_calls + trial.child_calls >= self.bounds.trial_sends
        {
            state
                .halt
                .get_or_insert("shared request or fee stop reached");
            return Err(budget("shared request or fee stop reached"));
        }
        if self.role == Role::Child {
            if trial.spawns != 1 {
                state.halt = Some("child outside accepted spawn");
                return Err(budget("child outside accepted spawn"));
            }
            let started = *trial.child_started.get_or_insert_with(Instant::now);
            if started.elapsed() >= Duration::from_secs(self.bounds.child_seconds.into()) {
                state.halt = Some("child deadline reached");
                return Err(budget("child deadline reached"));
            }
            trial.child_calls += 1;
        } else {
            trial.root_calls += 1;
        }
        state.admitted += 1;
        state.in_flight = true;
        Ok((
            request,
            Flight {
                shared: self.shared.clone(),
                finished: false,
            },
        ))
    }

    // Only host-defined static reasons are retained; model text and arguments
    // must not enter diagnostics when a response is rejected.
    fn tools(&self, response: &CompletionResponse) -> Result<(), &'static str> {
        if !response.hosted_web_search.is_empty()
            || !response.url_citations.is_empty()
            || response.tool_calls.len() > 1
            || response.text.len() > 24_576
        {
            return Err("unexpected hosted action or oversized response");
        }
        let mut trial = self.trial.lock().expect("trial lock");
        if response.tool_calls.is_empty() {
            let read = match self.role {
                Role::Root if self.arm == Arm::Reader => trial.child_result_bytes > 0,
                Role::Root => trial.root_reads > 0,
                Role::Child => trial.child_reads > 0,
            };
            if !read {
                return Err("answer without the required real read");
            }
            return Ok(());
        }
        let call = &response.tool_calls[0];
        if call.id.is_empty() || call.id.len() > 256 {
            return Err("invalid tool call identity");
        }
        let arguments = call
            .arguments
            .as_object()
            .ok_or("tool arguments are not an object")?;
        let expected = match call.name.as_str() {
            "read" if self.role == Role::Child || self.arm == Arm::Direct => {
                if arguments.len() != 3
                    || !arguments.contains_key("path")
                    || !arguments.contains_key("offset")
                    || !arguments.contains_key("limit")
                    || call.arguments["path"].as_str() != self.case.path.to_str()
                {
                    return Err("read escaped the fixed source path");
                }
                let offset = call.arguments["offset"]
                    .as_u64()
                    .filter(|offset| *offset < 1200)
                    .ok_or("invalid read offset")?;
                let limit = call.arguments["limit"]
                    .as_u64()
                    .filter(|limit| *limit > 0 && *limit <= self.bounds.read_lines as u64)
                    .ok_or("invalid read limit")?;
                self.case
                    .verify()
                    .map_err(|_| "source changed before read")?;
                // Preview the exact bounded native reader result. The production agent still executes read.
                // This catches errors before execution and lets the next request reject a raced/failed read.
                let text =
                    polaris_tools::read::read(&self.case.path, offset as usize, limit as usize)
                        .map_err(|_| "fixture read unavailable")?;
                if text.len() > self.bounds.read_output_bytes {
                    return Err("read output exceeds frozen byte bound");
                }
                match self.role {
                    Role::Root => trial.root_reads += 1,
                    Role::Child => trial.child_reads += 1,
                }
                ExpectedTool::Read(text)
            }
            "spawn" if self.role == Role::Root && self.arm == Arm::Reader => {
                let tasks = call.arguments["tasks"]
                    .as_array()
                    .filter(|tasks| tasks.len() == 1)
                    .ok_or("one child task required")?;
                let task = tasks[0].as_object().ok_or("invalid child task")?;
                if arguments.len() != 1 {
                    return Err("unexpected spawn fields");
                }
                if trial.spawns != 0 {
                    return Err("second spawn prohibited");
                }
                if !task.contains_key("type") {
                    return Err("child task omitted type");
                }
                if tasks[0]["type"] != "file-reader" {
                    return Err("unexpected child type");
                }
                if tasks[0]["task"] != self.case.child_task(&self.bounds) {
                    return Err("child task differs from frozen prompt");
                }
                // The advertised schema offers `write_root` beside `type` and
                // `task`, and the real root model attached it on 2026-09-20.
                // `file-reader` is `AgentAccess::Read`, and for those
                // `resolve_subagent_sandbox` restricts to ReadOnly without
                // reading the field, so the shipped path accepts this call.
                // Refusing it measured a restriction of this harness rather
                // than the behaviour under test; record it and carry on.
                let declared_write_root = task.contains_key("write_root");
                if task.len() != 2 + usize::from(declared_write_root) {
                    return Err("unexpected child task fields");
                }
                trial.child_write_root_declared |= declared_write_root;
                trial.spawns += 1;
                ExpectedTool::Spawn
            }
            _ => return Err("tool prohibited for this arm"),
        };
        let tools = match self.role {
            Role::Root => &mut trial.root_tools,
            Role::Child => &mut trial.child_tools,
        };
        if tools.insert(call.id.clone(), expected).is_some() {
            return Err("tool identity reused");
        }
        Ok(())
    }

    fn observe(
        &self,
        result: &Result<CompletionResponse, ProviderError>,
    ) -> Result<(), &'static str> {
        let usage = usage_of(result).ok_or("response usage missing")?;
        let rows = accounting::read_metrics(&self.shared.metrics)
            .map_err(|_| "physical metric missing or invalid")?;
        let mut state = self.shared.state.lock().expect("guard lock");
        if rows.len() != state.metric_count + 1 {
            return Err("physical metric count mismatch");
        }
        let row = rows.last().expect("one new row");
        let cost = accounting::assess_row(row, self.role.model(), usage)
            .map_err(|_| "physical usage invalid")?;
        let id = row["attempt_id"]
            .as_str()
            .expect("validated metric identity");
        if !state.metric_ids.insert(id.into()) {
            return Err("duplicate physical metric");
        }
        state.metric_count += 1;
        state.cost += cost.upper;
        if usage.output_tokens > self.bounds.observed_output_stop {
            return Err("observed output stop exceeded");
        }
        if state.cost >= u128::from(self.shared.fee_cap) {
            return Err("observed fee stop reached");
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Provider for Guard {
    async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Err(self.reject("unscoped completion, summary or compaction prohibited"))
    }
    async fn complete_in_turn(
        &self,
        request: CompletionRequest,
        turn: &TurnContext,
    ) -> Result<CompletionResponse, ProviderError> {
        let (request, mut flight) = self.prepare(request)?;
        let remaining = if self.role == Role::Child {
            Duration::from_secs(self.bounds.child_seconds.into()).saturating_sub(
                self.trial
                    .lock()
                    .expect("trial lock")
                    .child_started
                    .expect("child admitted")
                    .elapsed(),
            )
        } else {
            Duration::from_secs(self.bounds.trial_seconds.into())
        };
        let result =
            match tokio::time::timeout(remaining, self.inner.complete_in_turn(request, turn)).await
            {
                Ok(result) => result,
                Err(_) => Err(budget("provider wall-clock limit reached")),
            };
        flight.finished = true;
        let usage = usage_of(&result);
        let observation = self.observe(&result);
        match result {
            Err(error) => {
                self.shared.halt("provider failed");
                Err(received(error, usage))
            }
            Ok(response) => {
                if let Err(reason) = observation.and_then(|()| self.tools(&response)) {
                    self.shared.halt(reason);
                    return Err(received(budget(reason), usage));
                }
                Ok(response)
            }
        }
    }
    fn model(&self) -> Option<String> {
        Some(self.role.model().into())
    }
}
