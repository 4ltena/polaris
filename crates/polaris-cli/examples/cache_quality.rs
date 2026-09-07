//! Offline-testable driver for the cache-affinity quality matrix.
//!
//! The parent runner owns fixtures and cases. This program only runs one
//! supplied case against production `Session`/`agent::run` plumbing.

use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Component, Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Instant,
};

use polaris_core::AgentEvent;
use polaris_core::{
    agent::{self, ToolContext},
    approval::{ApprovalPolicy, Approver, Decision, Gate},
    audit::AuditLog,
    prompt,
    session::Session,
    stop::StopTracker,
};
use polaris_provider::{
    CompletionRequest, CompletionResponse, Provider, ProviderError, Token, TokenSource,
    codex::{CodexProvider, ENDPOINT_BASE},
    turn_affinity::TurnContext,
};
use polaris_sandbox::{SandboxMode, SandboxPolicy};
use serde_json::{Value, json};

const MODEL: &str = "gpt-6-astra";
const EFFORT: &str = "medium";
const CASE_SECONDS: u64 = 540;

#[derive(Debug)]
struct Case {
    id: String,
    skill_count: usize,
    tool_count: usize,
    examples: Vec<Example>,
    turns: Vec<Turn>,
}

#[derive(Debug)]
struct Example {
    user: String,
    assistant: String,
}

#[derive(Debug)]
struct Turn {
    prompt: String,
    expected: Value,
    required_tools: Vec<RequiredTool>,
    updates: BTreeMap<String, String>,
}

#[derive(Debug)]
struct RequiredTool {
    name: String,
    arguments: Value,
    min_calls: usize,
}

fn one() -> usize {
    1
}

fn object(value: &Value) -> Result<&serde_json::Map<String, Value>, String> {
    value
        .as_object()
        .ok_or_else(|| "case input is invalid".into())
}

fn string(field: Option<&Value>) -> Result<String, String> {
    field
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| "case input is invalid".into())
}

fn count(field: Option<&Value>) -> Result<usize, String> {
    field
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| "case input is invalid".into())
}

fn parse_required_tools(field: Option<&Value>) -> Result<Vec<RequiredTool>, String> {
    let Some(field) = field else {
        return Ok(Vec::new());
    };
    field
        .as_array()
        .ok_or_else(|| "case input is invalid".to_string())?
        .iter()
        .map(|value| {
            let object = object(value)?;
            Ok(RequiredTool {
                name: string(object.get("name"))?,
                arguments: object
                    .get("arguments")
                    .cloned()
                    .ok_or_else(|| "case input is invalid".to_string())?,
                min_calls: object
                    .get("min_calls")
                    .map(|value| count(Some(value)))
                    .transpose()?
                    .unwrap_or_else(one),
            })
        })
        .collect()
}

fn parse_case(value: Value) -> Result<Case, String> {
    let fields = object(&value)?;
    let examples = match fields.get("examples") {
        None => Vec::new(),
        Some(Value::Array(values)) => values
            .iter()
            .map(|value| {
                let object = object(value)?;
                Ok(Example {
                    user: string(object.get("user"))?,
                    assistant: string(object.get("assistant"))?,
                })
            })
            .collect::<Result<Vec<_>, String>>()?,
        _ => return Err("case input is invalid".into()),
    };
    let turns = fields
        .get("turns")
        .and_then(Value::as_array)
        .ok_or_else(|| "case input is invalid".to_string())?
        .iter()
        .map(|value| {
            let object = object(value)?;
            let updates = match object.get("updates") {
                None => BTreeMap::new(),
                Some(Value::Object(updates)) => updates
                    .iter()
                    .map(|(path, contents)| Ok((path.clone(), string(Some(contents))?)))
                    .collect::<Result<BTreeMap<_, _>, String>>()?,
                _ => return Err("case input is invalid".into()),
            };
            Ok(Turn {
                prompt: string(object.get("prompt"))?,
                expected: object
                    .get("expected")
                    .cloned()
                    .ok_or_else(|| "case input is invalid".to_string())?,
                required_tools: parse_required_tools(object.get("required_tools"))?,
                updates,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok(Case {
        id: string(fields.get("id"))?,
        skill_count: count(fields.get("skill_count"))?,
        tool_count: count(fields.get("tool_count"))?,
        examples,
        turns,
    })
}

struct Args {
    case: PathBuf,
    fixture: PathBuf,
    helper: PathBuf,
    output: PathBuf,
}

fn usage() -> &'static str {
    "usage: cache_quality --case ABS_CASE_JSON --fixture ABS_FIXTURE --helper ABS_PRODUCTION_POLARIS --output ABS_OUTPUT_JSONL"
}

fn args() -> Result<Args, String> {
    let mut values = std::env::args_os().skip(1);
    let mut case = None;
    let mut fixture = None;
    let mut helper = None;
    let mut output = None;
    while let Some(flag) = values.next() {
        match flag.to_str() {
            Some("--case") => {
                case = Some(
                    values
                        .next()
                        .map(PathBuf::from)
                        .ok_or("--case requires a path")?,
                )
            }
            Some("--fixture") => {
                fixture = Some(
                    values
                        .next()
                        .map(PathBuf::from)
                        .ok_or("--fixture requires a path")?,
                )
            }
            Some("--helper") => {
                helper = Some(
                    values
                        .next()
                        .map(PathBuf::from)
                        .ok_or("--helper requires a path")?,
                )
            }
            Some("--output") => {
                output = Some(
                    values
                        .next()
                        .map(PathBuf::from)
                        .ok_or("--output requires a path")?,
                )
            }
            _ => return Err(usage().to_string()),
        }
    }
    let absolute = |value: Option<PathBuf>, flag: &str| match value {
        Some(path) if path.is_absolute() => Ok(path),
        _ => Err(format!("{flag} must be an absolute path")),
    };
    Ok(Args {
        case: absolute(case, "--case")?,
        fixture: absolute(fixture, "--fixture")?,
        helper: absolute(helper, "--helper")?,
        output: absolute(output, "--output")?,
    })
}

fn validate_case(case: &Case, fixture: &Path) -> Result<(), String> {
    if case.id.is_empty()
        || !matches!(case.skill_count, 0 | 3 | 30)
        || !matches!(case.tool_count, 0 | 3 | 6)
        || !matches!(case.examples.len(), 0 | 1 | 3)
        || !(case.turns.len() == 1 || case.turns.len() == 36)
    {
        return Err("case bounds are invalid".into());
    }
    let root = fixture
        .canonicalize()
        .map_err(|_| "fixture is unavailable".to_string())?;
    let skill_root = root.join(".polaris").join("skills");
    let skill_metadata = fs::symlink_metadata(&skill_root)
        .map_err(|_| "fixture skill root is unavailable".to_string())?;
    if skill_metadata.file_type().is_symlink() || !skill_metadata.is_dir() {
        return Err("fixture skill root is invalid".into());
    }
    let discovered = polaris_skills::discover_in(&[skill_root]);
    if !discovered.skipped.is_empty() || discovered.skills.len() != case.skill_count {
        return Err("fixture skill count does not match case".into());
    }
    for turn in &case.turns {
        if !turn.required_tools.iter().all(|tool| {
            tool.min_calls > 0
                && tool.arguments.is_object()
                && matches!(
                    tool.name.as_str(),
                    "read" | "bash" | "skill" | "write" | "edit" | "spawn"
                )
        }) {
            return Err("required tool contract is invalid".into());
        }
        for relative in turn.updates.keys() {
            validate_update_path(&root, relative)?;
        }
    }
    Ok(())
}

fn validate_update_path(root: &Path, relative: &str) -> Result<PathBuf, String> {
    let path = Path::new(relative);
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path
            .components()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("update path is not a relative normal file".into());
    }
    let mut cursor = root.to_path_buf();
    for part in path.components() {
        cursor.push(part.as_os_str());
        let metadata = fs::symlink_metadata(&cursor)
            .map_err(|_| "update target is unavailable".to_string())?;
        if metadata.file_type().is_symlink() {
            return Err("update path contains a link".into());
        }
    }
    let target = cursor
        .canonicalize()
        .map_err(|_| "update target is unavailable".to_string())?;
    if !target.starts_with(root) || !target.is_file() {
        return Err("update target is outside fixture or not a file".into());
    }
    Ok(target)
}

fn apply_updates(root: &Path, updates: &BTreeMap<String, String>) -> Result<(), String> {
    for (relative, contents) in updates {
        let target = validate_update_path(root, relative)?;
        fs::write(target, contents).map_err(|_| "could not apply fixture update".to_string())?;
    }
    Ok(())
}

struct ReadOnlyTokens {
    store: PathBuf,
}

#[async_trait::async_trait]
impl TokenSource for ReadOnlyTokens {
    async fn token(&self) -> Result<Token, ProviderError> {
        let credentials = polaris_auth::ensure_fresh(polaris_auth::ISSUER, &self.store)
            .await
            .map_err(|_| ProviderError::Auth("authentication unavailable".into()))?;
        Ok(Token {
            effort: Some(EFFORT.into()),
            access_token: credentials.access_token,
            account_id: credentials.account_id,
        })
    }

    async fn refreshed(&self) -> Result<Token, ProviderError> {
        Err(ProviderError::Auth(
            "authentication refresh is disabled".into(),
        ))
    }
}

/// Keeps the production definitions in their production order, while making
/// forbidden model calls fail before `agent::run` can dispatch a tool.
struct ToolSubset {
    inner: Arc<dyn Provider>,
    allowed: Vec<String>,
    current: AtomicUsize,
    total: AtomicUsize,
    total_limit: usize,
}

impl ToolSubset {
    fn new(
        inner: Arc<dyn Provider>,
        tool_count: usize,
        total_limit: usize,
    ) -> Result<Self, String> {
        let all = polaris_tools::all_specs();
        if all.len() != 6 {
            return Err("production tool definition count changed".into());
        }
        let allowed = match tool_count {
            0 => Vec::new(),
            3 => all
                .iter()
                .filter(|tool| matches!(tool.name, "read" | "bash" | "skill"))
                .map(|tool| tool.name.to_string())
                .collect(),
            6 => all.iter().map(|tool| tool.name.to_string()).collect(),
            _ => return Err("tool count is invalid".into()),
        };
        if tool_count == 3 && allowed != ["read", "bash", "skill"] {
            return Err("production tool order changed".into());
        }
        Ok(Self {
            inner,
            allowed,
            current: AtomicUsize::new(0),
            total: AtomicUsize::new(0),
            total_limit,
        })
    }

    fn reset_turn(&self) {
        self.current.store(0, Ordering::SeqCst);
    }

    fn requests(&self) -> usize {
        self.current.load(Ordering::SeqCst)
    }

    fn total_requests(&self) -> usize {
        self.total.load(Ordering::SeqCst)
    }

    fn prepare(&self, mut request: CompletionRequest) -> Result<CompletionRequest, ProviderError> {
        let current = self.current.fetch_add(1, Ordering::SeqCst) + 1;
        let total = self.total.fetch_add(1, Ordering::SeqCst) + 1;
        if current > 4 || total > self.total_limit {
            return Err(ProviderError::Decode("quality request cap reached".into()));
        }
        request
            .tools
            .retain(|tool| self.allowed.iter().any(|name| name == tool.name));
        Ok(request)
    }

    fn check_response(
        &self,
        response: CompletionResponse,
    ) -> Result<CompletionResponse, ProviderError> {
        if response.tool_calls.iter().any(|call| {
            !self.allowed.iter().any(|name| name == &call.name)
                || matches!(call.name.as_str(), "spawn" | "write" | "edit")
        }) {
            return Err(ProviderError::Decode(
                "quality tool call is not permitted".into(),
            ));
        }
        Ok(response)
    }
}

#[async_trait::async_trait]
impl Provider for ToolSubset {
    async fn complete(
        &self,
        request: CompletionRequest,
    ) -> Result<CompletionResponse, ProviderError> {
        let request = self.prepare(request)?;
        self.check_response(self.inner.complete(request).await?)
    }

    async fn complete_in_turn(
        &self,
        request: CompletionRequest,
        turn: &TurnContext,
    ) -> Result<CompletionResponse, ProviderError> {
        let request = self.prepare(request)?;
        self.check_response(self.inner.complete_in_turn(request, turn).await?)
    }

    fn set_model(&self, model: &str) {
        self.inner.set_model(model);
    }
    fn set_effort(&self, effort: Option<&str>) {
        self.inner.set_effort(effort);
    }
}

struct NeverAsk;
impl Approver for NeverAsk {
    fn ask(&mut self, _: &str) -> Decision {
        Decision::Deny
    }
}

fn strict_json(text: &str, expected: &Value) -> Result<(), String> {
    let actual: Value =
        serde_json::from_str(text).map_err(|_| "answer is not one JSON value".to_string())?;
    if &actual == expected {
        Ok(())
    } else {
        Err("answer does not exactly match expected JSON".into())
    }
}

#[derive(Clone)]
struct ObservedTool {
    name: String,
    arguments: Value,
    success: bool,
}

fn current_turn_tools(
    messages: &[polaris_provider::Message],
    start: usize,
    events: &[AgentEvent],
) -> Result<Vec<ObservedTool>, String> {
    let mut calls = Vec::new();
    let mut completed = std::collections::BTreeSet::new();
    for message in &messages[start..] {
        if !message.tool_calls.is_empty() {
            calls.extend(message.tool_calls.iter());
        }
        if let Some(id) = &message.tool_call_id {
            completed.insert(id.as_str());
        }
    }
    if calls
        .iter()
        .any(|call| !completed.contains(call.id.as_str()))
    {
        return Err("tool call has no paired result".into());
    }
    let finished: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolFinished {
                name, detail, ok, ..
            } => Some((name, detail, *ok)),
            _ => None,
        })
        .collect();
    if calls.len() != finished.len() {
        return Err("tool completion record is incomplete".into());
    }
    calls
        .into_iter()
        .zip(finished)
        .map(|(call, (name, detail, success))| {
            let arguments = serde_json::from_str(detail)
                .map_err(|_| "tool completion arguments are invalid".to_string())?;
            if call.name != *name || call.arguments != arguments {
                return Err("tool completion does not match call".into());
            }
            Ok(ObservedTool {
                name: call.name.clone(),
                arguments,
                success,
            })
        })
        .collect()
}

fn subset_matches(actual: &Value, required: &Value) -> bool {
    match (actual, required) {
        (Value::Object(actual), Value::Object(required)) => required.iter().all(|(key, value)| {
            actual
                .get(key)
                .is_some_and(|found| subset_matches(found, value))
        }),
        (actual, required) => actual == required,
    }
}

fn judge_tools(calls: &[ObservedTool], required: &[RequiredTool]) -> Result<(), String> {
    if calls.iter().any(|call| !call.success) {
        return Err("a tool call returned an error".into());
    }
    let mut offset = 0;
    for need in required {
        let mut found = 0;
        while offset < calls.len() && found < need.min_calls {
            let call = &calls[offset];
            offset += 1;
            if call.success
                && call.name == need.name
                && subset_matches(&call.arguments, &need.arguments)
            {
                found += 1;
            }
        }
        if found != need.min_calls {
            return Err("required successful tool sequence was not observed".into());
        }
    }
    Ok(())
}

fn write_row(output: &mut File, row: Value) -> Result<(), String> {
    serde_json::to_writer(&mut *output, &row)
        .map_err(|_| "could not serialize result".to_string())?;
    output
        .write_all(b"\n")
        .map_err(|_| "could not write result".to_string())?;
    output
        .flush()
        .map_err(|_| "could not flush result".to_string())
}

fn error_row(output: &mut File, id: Option<&str>, reason: &str) {
    let _ = write_row(output, json!({"type":"error", "id":id, "reason":reason}));
}

async fn run_case(case: Case, args: &Args, output: &mut File) -> Result<(), String> {
    if std::env::var("POLARIS_AUTH_READ_ONLY").ok().as_deref() != Some("1")
        || std::env::var("POLARIS_MODEL").ok().as_deref() != Some(MODEL)
        || std::env::var("POLARIS_CACHE_PREFIX").ok().as_deref() != Some("compact")
        || !matches!(
            std::env::var("POLARIS_TURN_AFFINITY").ok().as_deref(),
            Some("off" | "on")
        )
    {
        return Err("required measurement environment is not set".into());
    }
    let fixture = args
        .fixture
        .canonicalize()
        .map_err(|_| "fixture is unavailable".to_string())?;
    let output_parent = args
        .output
        .parent()
        .ok_or_else(|| "output parent is unavailable".to_string())?
        .canonicalize()
        .map_err(|_| "output parent is unavailable".to_string())?;
    if args.output.starts_with(&fixture) || output_parent.starts_with(&fixture) {
        return Err("output must be outside fixture".into());
    }
    let skills = polaris_skills::discover_in(&[fixture.join(".polaris").join("skills")]);
    let always_on = prompt::assemble_always_on("", "benchmark; filemap=missing", &skills.skills);
    let store = polaris_auth::store::default_path()
        .map_err(|_| "authentication store is unavailable".to_string())?;
    if store.starts_with(&fixture) {
        return Err("authentication state must be outside fixture".into());
    }
    let raw: Arc<dyn Provider> = Arc::new(
        CodexProvider::new(
            ENDPOINT_BASE.into(),
            MODEL.into(),
            Arc::new(ReadOnlyTokens { store }),
        )
        .map_err(|_| "provider is unavailable".to_string())?,
    );
    raw.set_effort(Some(EFFORT));
    let provider = Arc::new(ToolSubset::new(
        raw,
        case.tool_count,
        if case.turns.len() == 36 { 60 } else { 4 },
    )?);
    let sandbox = SandboxPolicy::new(SandboxMode::ReadOnly, &[])
        .map_err(|_| "read-only sandbox is unavailable".to_string())?;
    let audit_path = args.output.with_extension("audit.jsonl");
    if audit_path.starts_with(&fixture) {
        return Err("audit log must be outside fixture".into());
    }
    let audit = Arc::new(tokio::sync::Mutex::new(
        AuditLog::open(&audit_path).map_err(|_| "audit log is unavailable".to_string())?,
    ));
    let mut session = Session::new();
    session.compaction_threshold = Some(usize::MAX);
    for example in &case.examples {
        session.push_user(&example.user);
        session.push_assistant(&example.assistant, Vec::new());
    }
    let started = Instant::now();
    for (index, turn) in case.turns.iter().enumerate() {
        apply_updates(&fixture, &turn.updates)?;
        let before = serde_json::to_value(&session.messages)
            .map_err(|_| "could not inspect history".to_string())?;
        let before_len = session.messages.len();
        session.push_user(&turn.prompt);
        provider.reset_turn();
        let mut stop = StopTracker::new(4);
        let mut gate = Gate::new(ApprovalPolicy::Never);
        let mut approver = NeverAsk;
        let mut context = ToolContext {
            sandbox: &sandbox,
            helper: &args.helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        let outcome = agent::run(
            provider.as_ref(),
            &mut session,
            audit.clone(),
            &mut stop,
            &always_on,
            &skills.skills,
            &[],
            provider.clone(),
            1,
            0,
            Some(event_tx),
            &mut context,
        )
        .await
        .map_err(|_| "agent turn did not complete".to_string())?;
        let mut events = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            events.push(event);
        }
        let after = serde_json::to_value(&session.messages)
            .map_err(|_| "could not inspect history".to_string())?;
        let history_preserved = match (before.as_array(), after.as_array()) {
            (Some(before), Some(after)) => after.starts_with(before),
            _ => false,
        };
        let calls = current_turn_tools(&session.messages, before_len + 1, &events)?;
        let quality = strict_json(&outcome.text, &turn.expected)
            .and_then(|_| judge_tools(&calls, &turn.required_tools));
        let row = json!({
            "type":"turn", "id": case.id, "turn": index + 1, "text": outcome.text, "error": null,
            "quality_pass": quality.is_ok(), "quality_reason": quality.as_ref().err(),
            "history_messages_before": before_len,
            "history_before_includes_current_user": false,
            "history_messages_after_user": before_len + 1,
            "history_messages_after": session.messages.len(),
            "history_preserved": history_preserved,
            "tool_calls": calls.into_iter().map(|call| json!({"name":call.name,"arguments":call.arguments,"success":call.success})).collect::<Vec<_>>(),
            "requests": provider.requests(), "request_delta": provider.requests(), "requests_case_total": provider.total_requests(),
            "usage": {"input":outcome.usage.input_tokens,"output":outcome.usage.output_tokens,"cache":outcome.usage.cached_tokens,"total":outcome.usage.total_tokens},
            "elapsed_seconds": started.elapsed().as_secs_f64(),
            "tool_memory": "off",
        });
        write_row(output, row)?;
        if !history_preserved {
            return Err("history prefix was not preserved".into());
        }
        if quality.is_err() {
            return Err("quality gate failed".into());
        }
    }
    write_row(
        output,
        json!({"type":"summary", "id":case.id, "tool_memory":"off", "elapsed_seconds":started.elapsed().as_secs_f64()}),
    )
}

#[tokio::main]
async fn main() {
    let args = match args() {
        Ok(args) => args,
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    };
    let mut output = match OpenOptions::new()
        .create(true)
        .append(true)
        .open(&args.output)
    {
        Ok(file) => file,
        Err(_) => {
            eprintln!("could not open output");
            std::process::exit(1);
        }
    };
    let parsed = fs::read(&args.case)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|value| parse_case(value).ok());
    let result = match parsed {
        Some(case) => match validate_case(&case, &args.fixture) {
            Ok(()) => tokio::time::timeout(
                std::time::Duration::from_secs(CASE_SECONDS),
                run_case(case, &args, &mut output),
            )
            .await
            .map_err(|_| "case deadline exceeded".to_string())
            .and_then(|result| result),
            Err(error) => Err(error),
        },
        None => Err("case input is invalid".into()),
    };
    if let Err(error) = result {
        // Every error entering this branch is a fixed driver label; provider,
        // filesystem and credential error contents are mapped away above.
        error_row(&mut output, None, &error);
        eprintln!("quality driver failed");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct MockProvider {
        response: std::sync::Mutex<CompletionResponse>,
        tools_seen: std::sync::Mutex<Vec<String>>,
        in_turn_calls: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Provider for MockProvider {
        async fn complete(
            &self,
            request: CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            *self.tools_seen.lock().unwrap() = request
                .tools
                .into_iter()
                .map(|tool| tool.name.to_string())
                .collect();
            Ok(self.response.lock().unwrap().clone())
        }

        async fn complete_in_turn(
            &self,
            request: CompletionRequest,
            _: &TurnContext,
        ) -> Result<CompletionResponse, ProviderError> {
            self.in_turn_calls.fetch_add(1, Ordering::SeqCst);
            self.complete(request).await
        }
    }

    fn request() -> CompletionRequest {
        CompletionRequest {
            system: String::new(),
            messages: Vec::new(),
            tools: polaris_tools::all_specs(),
        }
    }

    struct SequenceProvider {
        responses: std::sync::Mutex<Vec<CompletionResponse>>,
    }

    #[async_trait::async_trait]
    impl Provider for SequenceProvider {
        async fn complete(
            &self,
            _: CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| ProviderError::Decode("missing mock response".into()))
        }
    }

    #[test]
    fn strict_judge_rejects_wrong_extra_and_type_changes() {
        let expected = json!({"ok":true,"n":1});
        assert!(strict_json(r#"{"ok":true,"n":1}"#, &expected).is_ok());
        assert!(strict_json(r#"{"ok":true,"n":1,"extra":0}"#, &expected).is_err());
        assert!(strict_json(r#"{"ok":true,"n":"1"}"#, &expected).is_err());
        assert!(strict_json(r#"{"ok":false,"n":1}"#, &expected).is_err());
    }

    #[test]
    fn tool_sequence_is_current_turn_ordered_and_rejects_errors() {
        let calls = vec![
            ObservedTool {
                name: "read".into(),
                arguments: json!({"path":"old.md"}),
                success: true,
            },
            ObservedTool {
                name: "skill".into(),
                arguments: json!({"q":"needle"}),
                success: true,
            },
        ];
        let required = vec![RequiredTool {
            name: "skill".into(),
            arguments: json!({"q":"needle"}),
            min_calls: 1,
        }];
        assert!(judge_tools(&calls, &required).is_ok());
        let stale = vec![
            RequiredTool {
                name: "read".into(),
                arguments: json!({"path":"old.md"}),
                min_calls: 1,
            },
            RequiredTool {
                name: "skill".into(),
                arguments: json!({"q":"needle"}),
                min_calls: 1,
            },
        ];
        assert!(judge_tools(&calls[1..], &stale).is_err());
        let failed = vec![ObservedTool {
            name: "skill".into(),
            arguments: json!({"q":"needle"}),
            success: false,
        }];
        assert!(judge_tools(&failed, &required).is_err());
    }

    #[test]
    fn update_path_rejects_links_and_before_auth_bounds() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("real.txt"), "old").unwrap();
        fs::create_dir_all(dir.path().join(".polaris/skills")).unwrap();
        std::os::unix::fs::symlink(dir.path().join("real.txt"), dir.path().join("link.txt"))
            .unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert!(validate_update_path(&root, "real.txt").is_ok());
        assert!(validate_update_path(&root, "link.txt").is_err());
        let invalid = Case {
            id: "x".into(),
            skill_count: 2,
            tool_count: 0,
            examples: vec![],
            turns: vec![],
        };
        assert!(validate_case(&invalid, dir.path()).is_err());
        let count_mismatch = Case {
            id: "x".into(),
            skill_count: 3,
            tool_count: 0,
            examples: vec![],
            turns: vec![Turn {
                prompt: "p".into(),
                expected: json!(null),
                required_tools: vec![],
                updates: BTreeMap::new(),
            }],
        };
        assert!(validate_case(&count_mismatch, dir.path()).is_err());
    }

    #[test]
    fn long_case_shape_preserves_all_prior_messages() {
        let mut session = Session::new();
        session.compaction_threshold = Some(usize::MAX);
        for turn in 0..36 {
            let before = serde_json::to_value(&session.messages).unwrap();
            session.push_user(&format!("u{turn}"));
            session.push_assistant(&format!("a{turn}"), Vec::new());
            assert!(
                serde_json::to_value(&session.messages)
                    .unwrap()
                    .as_array()
                    .unwrap()
                    .starts_with(before.as_array().unwrap())
            );
        }
        assert_eq!(session.messages.len(), 72);
    }

    #[tokio::test]
    async fn subset_filters_definitions_and_rejects_unadvertised_or_mutating_calls() {
        let mock = Arc::new(MockProvider {
            response: std::sync::Mutex::new(CompletionResponse::default()),
            tools_seen: std::sync::Mutex::new(Vec::new()),
            in_turn_calls: AtomicUsize::new(0),
        });
        let subset = ToolSubset::new(mock.clone(), 3, 4).unwrap();
        subset
            .complete_in_turn(request(), &TurnContext::new())
            .await
            .unwrap();
        assert_eq!(
            *mock.tools_seen.lock().unwrap(),
            vec!["read", "bash", "skill"]
        );
        assert_eq!(mock.in_turn_calls.load(Ordering::SeqCst), 1);
        *mock.response.lock().unwrap() = CompletionResponse {
            tool_calls: vec![polaris_provider::ToolCall {
                id: "x".into(),
                name: "unknown".into(),
                arguments: json!({}),
            }],
            ..Default::default()
        };
        assert!(subset.complete(request()).await.is_err());
        *mock.response.lock().unwrap() = CompletionResponse {
            tool_calls: vec![polaris_provider::ToolCall {
                id: "x".into(),
                name: "write".into(),
                arguments: json!({}),
            }],
            ..Default::default()
        };
        let all = ToolSubset::new(mock, 6, 4).unwrap();
        assert!(all.complete(request()).await.is_err());
    }

    #[tokio::test]
    async fn thirty_six_actual_agent_runs_keep_history_and_one_request_per_turn() {
        let responses = (0..36)
            .map(|turn| CompletionResponse {
                text: json!({"turn": turn}).to_string(),
                ..Default::default()
            })
            .collect();
        let raw: Arc<dyn Provider> = Arc::new(SequenceProvider {
            responses: std::sync::Mutex::new(responses),
        });
        let provider = Arc::new(ToolSubset::new(raw, 0, 60).unwrap());
        let always_on = prompt::assemble_always_on("", "benchmark; filemap=missing", &[]);
        let sandbox = SandboxPolicy::new(SandboxMode::ReadOnly, &[]).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let audit = Arc::new(tokio::sync::Mutex::new(
            AuditLog::open(&dir.path().join("audit.jsonl")).unwrap(),
        ));
        let mut session = Session::new();
        session.compaction_threshold = Some(usize::MAX);
        for turn in 0..36 {
            let before = serde_json::to_value(&session.messages).unwrap();
            let before_len = session.messages.len();
            session.push_user(&format!("turn {turn}"));
            provider.reset_turn();
            let mut stop = StopTracker::new(4);
            let mut gate = Gate::new(ApprovalPolicy::Never);
            let mut approver = NeverAsk;
            let mut context = ToolContext {
                sandbox: &sandbox,
                helper: Path::new("/bin/true"),
                gate: &mut gate,
                approver: &mut approver,
            };
            let outcome = agent::run(
                provider.as_ref(),
                &mut session,
                audit.clone(),
                &mut stop,
                &always_on,
                &[],
                &[],
                provider.clone(),
                1,
                0,
                None,
                &mut context,
            )
            .await
            .unwrap();
            assert_eq!(provider.requests(), 1, "turn {turn} request delta");
            assert!(strict_json(&outcome.text, &json!({"turn": 35 - turn})).is_ok());
            let after = serde_json::to_value(&session.messages).unwrap();
            assert!(
                after
                    .as_array()
                    .unwrap()
                    .starts_with(before.as_array().unwrap()),
                "turn {turn} history prefix"
            );
            assert_eq!(before_len + 2, session.messages.len());
        }
        assert_eq!(provider.total_requests(), 36);
    }
}
