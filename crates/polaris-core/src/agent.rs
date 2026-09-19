//! The agent loop. Returns the body text at the point tool calls stop.
//!
//! When an assistant turn calls a tool, that turn itself is recorded to the
//! session with its `tool_calls` before each tool is executed. OpenAI's
//! round-trip protocol requires this assistant message to already be
//! present in the history before the following `role: "tool"` messages —
//! get the order wrong and the next turn's send is rejected by the API.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use polaris_provider::{CompletionRequest, Provider};
use tokio::sync::Mutex;

use crate::audit::{AuditLog, Record};
use crate::session::Session;
use crate::stop::{StopReason, StopTracker};

mod mcp_dispatch;
mod skill_catalog;
mod workflow_control;

#[derive(Debug, thiserror::Error)]
pub enum AgentError {
    #[error("MCP execution stopped: {0}")]
    Mcp(#[from] crate::mcp::runtime::Error),
    #[error("MCP操作の結果を確認できません。外部の状態を確認してください。自動では再実行しません")]
    McpUnconfirmed,
    #[error("desktop event delivery stopped: {0}")]
    Desktop(crate::desktop_events::EventFailure),
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
/// `gate` and `approver` guard writes, edits, and shell commands. Shell
/// approval is independent of the write-target predicate.
pub struct ToolContext<'a> {
    pub sandbox: &'a polaris_sandbox::SandboxPolicy,
    pub helper: &'a Path,
    pub gate: &'a mut crate::approval::Gate,
    pub approver: &'a mut dyn crate::approval::Approver,
}

/// A test-only approver for fixtures that do not exercise user denial.
#[cfg(test)]
pub(crate) struct AutoApprove;

#[cfg(test)]
impl crate::approval::Approver for AutoApprove {
    fn ask(&mut self, _reason: &str) -> crate::approval::Decision {
        crate::approval::Decision::Allow
    }
}

/// What a successful turn produced: the final text, and the token usage
/// accumulated across every `provider.complete()` call the turn made
/// (a turn that used a tool calls the provider more than once). A
/// response whose `usage` came back `None` is recorded as missing in
/// `usage_report`. Known totals include summaries and children; missing
/// usage is never evidence of zero consumption.
#[derive(Debug)]
pub struct AgentOutcome {
    pub text: String,
    pub usage: polaris_provider::Usage,
    pub usage_report: polaris_provider::UsageReport,
}

/// Pass in `always_on` as something [`crate::prompt::assemble_always_on`]
/// has already assembled. It is not assembled inside the loop, so that the
/// core keeps assembly bounded. A trusted refresh capability replaces only
/// project rules at a request boundary; unchanged effective rules keep the prefix.
///
/// This takes [`crate::prompt::AlwaysOn`] rather than a string and a
/// `Vec<ToolSpec>` taken separately, so that what gets loaded every turn
/// cannot be built outside this type. If the caller could append to an
/// already-assembled string, the ceiling on the always-on context would be
/// left up to how the caller happens to write it.
///
/// `skills` is not loaded into the always-on context. It's only consulted
/// when the `skill` tool is invoked.
///
/// `audit` is shared rather than borrowed exclusively: the `spawn` tool
/// runs subagents whose own loops write to the very same log (that is what
/// `Record::caller` distinguishes), so the handle has to be reachable from
/// inside a tool call. It is locked around each individual `record()` and
/// never held across a turn — hold it for a whole loop and the first
/// `spawn` call would wait forever on a lock its own caller holds.
///
/// `agent_types` and `provider_pool` exist only for that same `spawn`
/// arm: they are what a wave of subagents is resolved and run against.
/// A subagent is handed an empty `agent_types` (depth is fixed at 1), and
/// its tool list never contains `spawn` in the first place.
#[allow(clippy::too_many_arguments)]
pub async fn run(
    provider: &dyn Provider,
    session: &mut Session,
    audit: Arc<Mutex<AuditLog>>,
    stop: &mut StopTracker,
    always_on: &crate::prompt::AlwaysOn,
    skills: &[polaris_skills::Skill],
    agent_types: &[polaris_skills::AgentType],
    provider_pool: Arc<dyn Provider>,
    spawn_concurrency: usize,
    spawn_write_concurrency: usize,
    events: Option<tokio::sync::mpsc::UnboundedSender<crate::events::AgentEvent>>,
    ctx: &mut ToolContext<'_>,
) -> Result<AgentOutcome, AgentError> {
    run_with_mcp(
        provider,
        session,
        audit,
        stop,
        always_on,
        skills,
        agent_types,
        provider_pool,
        spawn_concurrency,
        spawn_write_concurrency,
        events,
        ctx,
        None,
    )
    .await
}

/// [`run`] with a local MCP runtime from `McpPort::local`. The caller keeps the
/// runtime outside this future and shuts it down on the same Tokio runtime.
#[allow(clippy::too_many_arguments)]
pub async fn run_with_mcp(
    provider: &dyn Provider,
    session: &mut Session,
    audit: Arc<Mutex<AuditLog>>,
    stop: &mut StopTracker,
    always_on: &crate::prompt::AlwaysOn,
    skills: &[polaris_skills::Skill],
    agent_types: &[polaris_skills::AgentType],
    provider_pool: Arc<dyn Provider>,
    spawn_concurrency: usize,
    spawn_write_concurrency: usize,
    events: Option<tokio::sync::mpsc::UnboundedSender<crate::events::AgentEvent>>,
    ctx: &mut ToolContext<'_>,
    mcp: Option<&mut crate::mcp::runtime::RunRuntime>,
) -> Result<AgentOutcome, AgentError> {
    session.memory_observation = None;
    session.check_persistence()?;
    let skill_query = real_user_query(session);
    session.request_prompt = Some(always_on.clone());
    run_loop(
        provider,
        session,
        audit,
        stop,
        always_on.system(),
        always_on.tools(),
        skills,
        agent_types,
        provider_pool,
        spawn_concurrency,
        spawn_write_concurrency,
        "root",
        &skill_query,
        false,
        events.map(Into::into),
        ctx,
        mcp,
    )
    .await
}

/// Runs with a bounded desktop event sink. Cancellation stops polling the
/// provider/agent future; it does not prove remote inference or a running
/// synchronous tool has terminated. The controller must keep those distinct.
#[allow(clippy::too_many_arguments)]
pub async fn run_desktop(
    provider: &dyn Provider,
    session: &mut Session,
    skill_query: &str,
    audit: Arc<Mutex<AuditLog>>,
    stop: &mut StopTracker,
    always_on: &crate::prompt::AlwaysOn,
    skills: &[polaris_skills::Skill],
    agent_types: &[polaris_skills::AgentType],
    provider_pool: Arc<dyn Provider>,
    spawn_concurrency: usize,
    spawn_write_concurrency: usize,
    events: crate::desktop_events::DesktopEventSink,
    ctx: &mut ToolContext<'_>,
    mcp: Option<&mut crate::mcp::runtime::RunRuntime>,
) -> Result<AgentOutcome, AgentError> {
    session.memory_observation = None;
    let control = events.control();
    session.request_prompt = Some(always_on.clone());
    if let Some(reason) = control.failure() {
        return Err(AgentError::Desktop(reason));
    }
    let execution = run_loop(
        provider,
        session,
        audit,
        stop,
        always_on.system(),
        always_on.tools(),
        skills,
        agent_types,
        provider_pool,
        spawn_concurrency,
        spawn_write_concurrency,
        "root",
        skill_query,
        true,
        Some(crate::desktop_events::EventSink::Bounded(events)),
        ctx,
        mcp,
    );
    tokio::select! {
        biased;
        _ = control.cancelled() => Err(AgentError::Desktop(control.failure().unwrap_or(crate::desktop_events::EventFailure::Cancelled))),
        // Once the agent has observed and persisted completion, preserve its
        // result and usage even if the display disconnects in that same poll.
        result = execution => result,
    }
}

pub(crate) fn check_event_delivery(
    events: Option<&crate::desktop_events::EventSink>,
) -> Result<(), AgentError> {
    if let Some(crate::desktop_events::EventSink::Bounded(sink)) = events
        && let Some(reason) = sink.control().failure()
    {
        return Err(AgentError::Desktop(reason));
    }
    Ok(())
}

fn real_user_query(session: &Session) -> String {
    session
        .messages
        .iter()
        .rev()
        .find(|message| message.role == polaris_provider::Role::User)
        .map(|message| message.content.clone())
        .unwrap_or_default()
}

/// The most directories one tool call may set `files.md` regeneration
/// going for. Each target is a full serial subagent run — a provider round
/// trip the model's tool result waits on — so an unbounded count is not a
/// performance detail but a hang: `mkdir -p` chains, an archive
/// extraction, or a `git clone` can drop hundreds of directories in at
/// once. This number is set to comfortably cover a normal nested `mkdir`
/// while still bounding the pathological case; what exceeds it is dropped
/// and said so in the audit log (see the call site).
const MAX_REGENERATION_TARGETS_PER_CALL: usize = 32;

/// Both halves of the `examples` ceiling. The token figure matches what a
/// retrieved memory is allowed to weigh, since the two sit side by side in
/// every request. The count is the half tokens cannot express: the
/// evaluation path adds user/assistant pairs, and a large number of short
/// pairs is as much a drift in what the request teaches as one long one.
const MAX_EXAMPLES: usize = 20;
const MAX_EXAMPLE_TOKENS: usize = 768;

/// Asks the judge which documented files this request is about, and
/// returns the section naming them.
///
/// Returns `None` whenever the route cannot be taken — no judge, no map,
/// an unreadable map, or a judge that would not answer. None of those is a
/// failure of the run: without a section the model falls back to reading
/// the map itself, which is exactly what it did before this existed. The
/// reason goes to the audit log so a run that quietly stopped routing can
/// be told apart from one where nothing matched.
///
/// The map is read through `symlink_metadata` first, on the same footing
/// as `constitution::environment_block`: a `docs/filemap.md` that is a link
/// is not followed, so a repository cannot point this at a file outside
/// itself and have its contents summarised into the prompt.
async fn filemap_route(
    session: &Session,
    root: Option<&std::path::Path>,
    query: &str,
    audit: &Arc<Mutex<AuditLog>>,
    caller: &str,
) -> Result<Option<String>, AgentError> {
    let Some(decider) = session.decider.clone() else {
        return Ok(None);
    };
    let Some(root) = root else {
        return Ok(None);
    };
    let path = root.join("docs/filemap.md");
    let readable = matches!(std::fs::symlink_metadata(&path), Ok(meta) if meta.is_file());
    if !readable {
        return Ok(None);
    }
    let Ok(body) = std::fs::read_to_string(&path) else {
        return Ok(None);
    };
    let entries = crate::filemap::parse(&body);
    let phase = session
        .workflow
        .as_ref()
        .map(|workflow| workflow.state().phase.as_str())
        .unwrap_or("");
    let (section, result) =
        match crate::filemap::route(decider.as_ref(), &entries, query, phase).await {
            Ok(picked) if picked.is_empty() => (None, "no_match".to_owned()),
            Ok(picked) => {
                let count = picked.len().min(crate::filemap::MAX_PICKS);
                (crate::filemap::section(&picked), format!("picked={count}"))
            }
            Err(refused) => (None, format!("{refused:?}")),
        };
    audit.lock().await.record(&Record {
        tool: "filemap-route",
        detail: &format!("entries={}", entries.len()),
        sandbox: None,
        target: None,
        result: &result,
        caller,
    })?;
    Ok(section)
}

/// Asks the judge which settled questions this request repeats.
///
/// Sits beside the filemap route and costs the same shape of call. The
/// section it produces is advice: it names what looks already decided and
/// leaves the model to judge, because a false match here would stop work
/// that should happen while a false miss only leaves things as they are.
async fn decided_route(
    session: &Session,
    root: Option<&std::path::Path>,
    query: &str,
    audit: &Arc<Mutex<AuditLog>>,
    caller: &str,
) -> Result<Option<String>, AgentError> {
    let Some(decider) = session.decider.clone() else {
        return Ok(None);
    };
    let Some(root) = root else {
        return Ok(None);
    };
    let entries = crate::decisions::load(root);
    if entries.is_empty() {
        return Ok(None);
    }
    let (section, result) = match crate::decisions::route(decider.as_ref(), &entries, query).await {
        Ok(matched) if matched.is_empty() => (None, "no_match".to_owned()),
        Ok(matched) => {
            let count = matched.len().min(crate::decisions::MAX_SHOWN);
            (
                crate::decisions::section(&matched),
                format!("matched={count}"),
            )
        }
        Err(refused) => (None, format!("{refused:?}")),
    };
    audit.lock().await.record(&Record {
        tool: "decided-route",
        detail: &format!("entries={}", entries.len()),
        sandbox: None,
        target: None,
        result: &result,
        caller,
    })?;
    Ok(section)
}

/// Trims `changes` so that at most `cap` directories can be regenerated
/// from it, and reports how many entries were dropped.
///
/// New directories are kept first: a brand new directory has no `files.md`
/// at all, whereas a new file in an existing directory only refreshes one
/// that already exists. When something has to be dropped, dropping the
/// refresh is the smaller loss.
///
/// The cap counts detected entries, not the targets
/// `files_md::targets_to_regenerate` derives from them — that function may
/// additionally pick up a parent directory per entry, so the true number of
/// subagent runs can exceed `cap`, by a bounded factor rather than the
/// unbounded one this exists to prevent. Counting entries here keeps the
/// cap enforced before any subagent is started, which is the property that
/// matters, without duplicating that function's target-selection rules.
fn cap_changes(
    mut changes: crate::dir_watch::DirChanges,
    cap: usize,
) -> (crate::dir_watch::DirChanges, usize) {
    let total = changes.new_dirs.len() + changes.new_files_in_existing_dirs.len();
    if total <= cap {
        return (changes, 0);
    }
    changes.new_dirs.truncate(cap);
    let remaining = cap - changes.new_dirs.len();
    changes.new_files_in_existing_dirs.truncate(remaining);
    (changes, total - cap)
}

/// Flush before a consumer can observe files.md, and before the next model
/// request. The provider pool already carries the root's usage meter.
async fn flush_directory_changes(
    pending: &mut crate::dir_watch::DirChanges,
    agent_types: &[polaris_skills::AgentType],
    provider_pool: Arc<dyn Provider>,
    audit: Arc<Mutex<AuditLog>>,
    workflow: Option<crate::spawn::ChildWorkflow>,
    ctx: &mut ToolContext<'_>,
) {
    if pending.new_dirs.is_empty() && pending.new_files_in_existing_dirs.is_empty() {
        return;
    }
    let mut changes = std::mem::take(pending);
    // This legacy regeneration entry cannot receive the parent's gate.
    // Refuse its unattended work rather than silently substituting Never.
    if let Err(reason) = ctx.gate.check_unattended("automatic files.md regeneration") {
        let _ = audit.lock().await.record(&Record {
            tool: "files-md-writer",
            detail: "automatic regeneration refused",
            sandbox: Some(ctx.sandbox),
            target: None,
            result: &reason,
            caller: "harness",
        });
        return;
    }

    changes.new_dirs.sort();
    changes.new_dirs.dedup();
    changes.new_files_in_existing_dirs.sort();
    changes.new_files_in_existing_dirs.dedup();
    // Break the async type cycle through the files-md-writer's run_loop.
    Box::pin(crate::files_md::regenerate_for_changes_scoped(
        &changes,
        agent_types,
        provider_pool,
        audit,
        ctx.sandbox,
        ctx.helper,
        workflow,
    ))
    .await;
}

/// Called immediately before a `bash` / `write` / `edit` call. Decides
/// what range of the filesystem to watch, takes the "before" snapshot on
/// the spot, and returns the pair — the range and the snapshot have to be
/// produced together, since the "after" scan must cover exactly the same
/// range or the diff is meaningless.
///
/// This is only ever reached for `caller == "root"`, and that guard is a
/// correctness condition rather than an optimization. The regeneration
/// this feeds runs the `files-md-writer` subagent, whose own loop calls
/// `write` to create `files.md`. Let that subagent's tool calls be watched
/// too and the file it just wrote is seen by the next diff as "a new file
/// in an existing directory" — which regenerates the very same directory
/// again. `files_md::targets_to_regenerate` already refuses `files.md`
/// itself as a trigger, but any *other* file the subagent happened to
/// touch would still re-enter the loop; keeping the whole mechanism off
/// for non-root callers stops it before the diff is even taken.
fn pre_call_snapshot(
    tool_name: &str,
    call: &polaris_provider::ToolCall,
    ctx: &ToolContext<'_>,
) -> (crate::dir_watch::ScanScope, crate::dir_watch::DirSnapshot) {
    let scope = match tool_name {
        // What a shell command will touch cannot be predicted, so the
        // whole writable root is scanned. Outside workspace-write there is
        // no root to scan: read-only has none by construction, and
        // full-access would mean scanning the entire filesystem twice per
        // call, which is not a cost this feature may impose.
        "bash" => match ctx.sandbox.mode() {
            polaris_sandbox::SandboxMode::WorkspaceWrite => {
                // Even with several writable roots, this watches only the
                // first. Essentially every real run has exactly one, and
                // multi-root support is left as a future extension rather
                // than paid for on every single `bash` call.
                match ctx.sandbox.writable_roots().first() {
                    Some(root) => crate::dir_watch::ScanScope::Recursive(root.clone()),
                    None => crate::dir_watch::ScanScope::None,
                }
            }
            _ => crate::dir_watch::ScanScope::None,
        },
        // `write` / `edit` resolve to exactly one path (the same "path"
        // argument the audit record's `target` is built from), so only the
        // directories immediately around it need watching, one level deep.
        //
        // Both the target's parent *and* that parent's own parent are
        // watched. `polaris_sandbox`'s helper runs `create_dir_all` before
        // writing, so a `write` to `sub/a.txt` genuinely creates `sub` on
        // disk — and a directory never appears inside its own listing, so
        // watching `sub` alone can never report `sub` as new. The new file
        // would land in `new_files_in_existing_dirs` instead, and
        // `targets_to_regenerate` drops that because `sub/files.md` does
        // not exist yet: the directory would silently never get a
        // `files.md`, and no later `bash` call could recover it either
        // (`sub` is in the "before" snapshot by then, so it never looks new
        // again). Catching exactly this case is the reason the spec chose
        // filesystem diffing over parsing `mkdir` out of command strings.
        //
        // The path is anchored to the working directory first, through the
        // very same `polaris_tools::predicate::absolutize` the tools' own
        // path resolution uses, and against the same base (`current_dir`) —
        // the actual write happens in a child process that inherits that
        // cwd and resolves the relative path against it, so watching
        // anywhere else would watch a directory the write never touches.
        //
        // Without this, a bare filename — `{"path": "README.md"}`, which is
        // simply how a model names a new top-level file — has
        // `Path::parent() == Some("")`, `read_dir("")` fails, and both
        // snapshots come back empty: the diff is empty every time and the
        // hook is a silent no-op for the most ordinary case there is.
        "write" | "edit" => {
            let parent = call.arguments["path"]
                .as_str()
                .map(Path::new)
                .map(|p| match std::env::current_dir() {
                    Ok(cwd) => polaris_tools::predicate::absolutize(&cwd, p),
                    // No cwd to anchor to. An absolute path is still
                    // usable as-is; a relative one is not, and the branch
                    // below turns its empty parent into `None`.
                    Err(_) => p.to_path_buf(),
                })
                .and_then(|p| p.parent().map(Path::to_path_buf))
                .filter(|p| !p.as_os_str().is_empty());
            match parent {
                Some(parent) => {
                    let mut dirs = vec![parent.clone()];
                    // The grandparent, so that a parent the write itself
                    // created shows up as a new entry somewhere. Skipped
                    // when there is none (the filesystem root) or when it
                    // is the same directory, which would only scan twice.
                    if let Some(grandparent) = parent.parent()
                        && grandparent != parent
                        && !grandparent.as_os_str().is_empty()
                    {
                        dirs.push(grandparent.to_path_buf());
                    }
                    crate::dir_watch::ScanScope::Shallow(dirs)
                }
                None => crate::dir_watch::ScanScope::None,
            }
        }
        _ => crate::dir_watch::ScanScope::None,
    };
    let before = crate::dir_watch::snapshot_for_scope(&scope);
    (scope, before)
}

/// ルートの `run` と subagent 実行(Task 9)の両方が使う、ターン取りの
/// 中核。`system`/`tools` を `AlwaysOn` からではなく直接受け取るのは、
/// subagent の型ごとに異なるシステムプロンプトとツール部分集合を、
/// ルート用に不変設計された `AlwaysOn` に混ぜないため — `prompt.rs` の
/// `AlwaysOn` のdoc commentを参照。
#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_loop(
    provider: &dyn Provider,
    session: &mut Session,
    audit: Arc<Mutex<AuditLog>>,
    stop: &mut StopTracker,
    system: &str,
    tools: &[polaris_tools::ToolSpec],
    skills: &[polaris_skills::Skill],
    agent_types: &[polaris_skills::AgentType],
    provider_pool: Arc<dyn Provider>,
    spawn_concurrency: usize,
    spawn_write_concurrency: usize,
    caller: &str,
    skill_query: &str,
    skills_are_frozen: bool,
    events: Option<crate::desktop_events::EventSink>,
    ctx: &mut ToolContext<'_>,
    mut mcp: Option<&mut crate::mcp::runtime::RunRuntime>,
) -> Result<AgentOutcome, AgentError> {
    // A desktop runtime needs the owner's transcript and event control; a local
    // runtime approves through `ctx` and records results in the session.
    let mcp_control = match (mcp.as_deref(), &events) {
        (None, _) => None,
        (Some(runtime), _)
            if runtime.is_local() && caller == "root" && session.desktop_transcript.is_none() =>
        {
            None
        }
        (Some(runtime), Some(crate::desktop_events::EventSink::Bounded(events)))
            if !runtime.is_local() && caller == "root" && session.desktop_transcript.is_some() =>
        {
            Some(events.control())
        }
        _ => {
            return Err(crate::mcp::runtime::Error::Owner(
                crate::execution_owner::McpCallError::Invalid,
            )
            .into());
        }
    };
    // Resolve once at the real user boundary; tool continuations retain this
    // exact snapshot. The guard releases the turn on errors and cancellation.
    session.check_persistence()?;
    let mut workflow_turn = session
        .workflow
        .as_ref()
        .filter(|workflow| workflow.config.enabled)
        .map(|workflow| {
            if skills_are_frozen
                || matches!(&events, Some(crate::desktop_events::EventSink::Bounded(_)))
            {
                // The desktop owner supplies a frozen, validated skill snapshot.
                // Never reopen host paths while running an isolated root or child.
                workflow.begin_turn(skills)
            } else {
                workflow.begin_turn_from_disk(skills)
            }
        })
        .transpose()
        .map_err(std::io::Error::other)?;
    if let Some(turn) = &mut workflow_turn {
        workflow_control::pending(session, turn).await?;
        if !workflow_control::adaptive(session) {
            turn.refresh_manual_profile(&session.workflow.as_ref().expect("workflow").gates)
                .map_err(std::io::Error::other)?;
        }
    }
    let is_adaptive = workflow_control::adaptive(session);
    let lookup_skills = workflow_turn
        .as_ref()
        .map(|turn| turn.skill_corpus())
        .unwrap_or(skills);
    let mut optional_skills = tools
        .iter()
        .any(|tool| tool.name == "skill")
        .then(|| {
            workflow_turn
                .as_ref()
                .map(|turn| turn.catalog_entries())
                .unwrap_or_else(|| crate::workflow::skill_catalog_entries(skills))
        })
        .map(|entries| skill_catalog::Catalog::new(entries, skill_query.into()));
    // Once per run, before the first request: the query does not change
    // within a run, so asking the judge again each iteration would buy the
    // same answer at the same cost.
    if let (Some(catalog), Some(decider)) = (optional_skills.as_mut(), &session.decider) {
        catalog
            .narrow(
                decider.as_ref(),
                session.light_model.as_deref(),
                std::future::pending(),
            )
            .await;
    }
    // `examples` rides on every request in the run and is the one part of
    // it with no ceiling of its own: `Session.examples` is a public `Vec`
    // with no configuration path, so a caller can assign anything. Refuse
    // here, before a single request is assembled, rather than truncating —
    // a half-sent set of examples teaches the model the wrong thing more
    // quietly than a failure does. Nothing in the loop writes `examples`,
    // so checking once covers the run.
    if session.examples.len() > MAX_EXAMPLES {
        return Err(std::io::Error::other(format!(
            "examplesが{MAX_EXAMPLES}件を超えました（{}件）",
            session.examples.len()
        ))
        .into());
    }
    let example_tokens = crate::compaction::session_tokens(&session.examples);
    if example_tokens > MAX_EXAMPLE_TOKENS {
        return Err(std::io::Error::other(format!(
            "examplesが{MAX_EXAMPLE_TOKENS}tokensを超えました（{example_tokens}tokens）"
        ))
        .into());
    }
    let base_system = system;
    let mut request_tools = tools.to_vec();
    // Never advertise `spawn` into an empty catalogue. It costs tokens on
    // every request, and with no type to resolve against, each of the
    // three dispatch branches ends before a provider call — two of them
    // reach `Stopped` after three tries, so the tool does not merely fail,
    // it ends the run. Shipped types (`polaris_skills::builtin`) mean the
    // CLI never gets here; a desktop whose bundle lost `Resources/agents`
    // still can.
    if agent_types.is_empty() {
        request_tools.retain(|tool| tool.name != "spawn");
    }
    let mcp_setup = session.mcp_setup.clone().filter(|_| caller == "root");
    if mcp_setup.is_some() {
        request_tools.push(polaris_tools::mcp_setup_spec());
    }
    // The bundled setting commands, on the same root-only footing: a child
    // run never gets to change the project's configuration.
    let setting = session.setting.clone().filter(|_| caller == "root");
    if setting.is_some() {
        request_tools.push(polaris_tools::setting_spec());
    }
    // The state window is offered to any run with an owner port, so reading
    // the current state does not depend on the workflow being enabled.
    let state_tool = is_adaptive || session.desktop_workflow.is_some();
    if state_tool {
        request_tools.push(polaris_tools::state_spec());
        crate::budget::validate_state_schema()?;
    }
    let workflow_system = workflow_turn
        .as_ref()
        .map(|turn| format!("{system}\n{}", turn.body));
    let system = workflow_system.as_deref().unwrap_or(system);
    session.checkpoint()?;
    if session.desktop_workflow.is_some()
        && let Some(turn) = &workflow_turn
    {
        session.publish_workflow(&turn.state, None).await?;
    }
    if let Some(turn) = &workflow_turn {
        audit.lock().await.record(&Record {
            tool: "workflow",
            detail: turn.state.phase.as_str(),
            sandbox: None,
            target: None,
            result: &format!(
                "manifest={} skills={} changed={}",
                turn.state.skill_manifest_hash,
                turn.ids.join(","),
                turn.changed_skill_ids.join(",")
            ),
            caller,
        })?;
        if let Some(tx) = &events {
            let _ = tx.send(crate::events::AgentEvent::WorkflowResolved {
                phase: turn.state.phase.to_string(),
                skill_ids: turn.ids.clone(),
                changed_skill_ids: turn.changed_skill_ids.clone(),
                manifest_hash: turn.state.skill_manifest_hash.clone(),
            });
        }
    }
    let summary_before = session
        .summary_usage
        .as_ref()
        .map(|meter| meter.snapshot())
        .unwrap_or_default();
    let historical_evidence = session.prepare_history().await;
    if let Some(observation) = &session.memory_observation {
        audit.lock().await.record(&Record {
            tool: "conversation-memory",
            detail: "preparation",
            sandbox: None,
            target: None,
            result: &observation.audit_detail(),
            caller,
        })?;
        if let Some(tx) = &events {
            let _ = tx.send(crate::events::AgentEvent::MemoryPrepared(
                observation.clone(),
            ));
        }
    }
    let historical_evidence = historical_evidence?;
    let strict_history = session.uses_strict_history()?;
    let child_workflow = session
        .workflow
        .as_ref()
        .filter(|workflow| workflow.config.enabled)
        .map(|workflow| crate::spawn::ChildWorkflow {
            workflow: workflow.clone(),
            skills: skills.to_vec(),
        });
    // Both paths share one meter: direct calls (including summaries), and
    // the pool used by spawn/files.md children. Never add child totals to
    // this snapshot; their actual calls have already been observed here.
    let meter = polaris_provider::UsageMeter::default();
    let metered_provider = meter.wrap(provider);
    let provider: &dyn Provider = &metered_provider;
    let provider_pool: Arc<dyn Provider> = Arc::new(meter.wrap(provider_pool));
    // The judge that chooses the tools also answers whether one of them is
    // worth troubling the person about. One registration and one question
    // shape for both: the gate asks the same "役に立つか" of a single
    // operation that preselection asks of every candidate. It is consulted
    // only where an approval question was already going to be asked, and it
    // may only refuse.
    if let Some(decider) = session.decider.clone() {
        ctx.gate
            .advise(crate::approval::Advisor::new(decider.clone(), skill_query));
        // And, separately, the severity classifier (M18). It needs two
        // things to do anything at all: a judge here, and a level the
        // person wrote down. Registering it with `AutoApproval::Off` — the
        // default — leaves every question exactly where it was, so the
        // registration itself grants nothing.
        ctx.gate.classify(crate::approval::Classifier::new(
            decider,
            session.approval_auto,
        ));
    }
    if let Some(runtime) = mcp.as_deref_mut() {
        let phase = session
            .workflow
            .as_ref()
            .map(|workflow| workflow.state().phase.as_str())
            .unwrap_or("");
        if let Some(status) = runtime
            .prepare_local_preselection(skill_query, phase, &meter)
            .await
        {
            audit.lock().await.record(&Record {
                tool: "mcp-preselection",
                detail: status.as_str(),
                sandbox: None,
                target: None,
                result: status.as_str(),
                caller,
            })?;
        }
    }
    // Route the request to a handful of files, once, before the first
    // request goes out.
    //
    // Sits here rather than behind a tool because a tool call costs a full
    // round trip of the main model — the whole history re-sent — to ask a
    // question the judge answers for a fraction of a cent. Computed once
    // and held constant for the run, so it joins the prompt at the same
    // place the workflow profile and the skill catalogue do without moving
    // the prefix between iterations.
    //
    // Root only. A subagent is given its task directly and has no map to
    // search, and paying for a second route on every spawned child would
    // cost more than the read it replaces.
    let (filemap_section, decided_section) = if caller == "root" {
        let root = ctx.sandbox.writable_roots().first().cloned();
        (
            filemap_route(session, root.as_deref(), skill_query, &audit, caller).await?,
            decided_route(session, root.as_deref(), skill_query, &audit, caller).await?,
        )
    } else {
        (None, None)
    };
    // Every invocation owns its context. Child agents invoke run_loop
    // independently and cannot inherit transport state from their parent.
    let turn_context = polaris_provider::turn_affinity::TurnContext::new();
    loop {
        check_event_delivery(events.as_ref())?;
        if is_adaptive && let Some(turn) = &workflow_turn {
            workflow_control::pending(session, turn).await?;
        }
        let current_profile = if is_adaptive {
            workflow_turn
                .as_ref()
                .map(|turn| turn.profile(&session.workflow.as_ref().expect("workflow").gates))
                .transpose()
                .map_err(std::io::Error::other)?
        } else {
            None
        };
        let request_system = if let Some(profile) = &current_profile {
            format!("{base_system}\n{}", profile.body)
        } else {
            system.to_string()
        };
        let empty_injected = Vec::new();
        let injected = current_profile
            .as_ref()
            .map(|profile| &profile.state.skills)
            .or_else(|| workflow_turn.as_ref().map(|turn| &turn.state.skills))
            .unwrap_or(&empty_injected);
        let catalog = optional_skills
            .as_mut()
            .map(|catalog| catalog.project(injected));
        let request_system = if let Some(catalog) = &catalog {
            if catalog.text.is_empty() {
                request_system
            } else {
                format!("{request_system}\n{}", catalog.text)
            }
        } else {
            request_system
        };
        // The routed files come last, so the sections that answer to their
        // own ceilings keep the order they had before this existed.
        let request_system = match &filemap_section {
            Some(section) => {
                format!("{request_system}\n{}\n{section}", crate::filemap::HEADING)
            }
            None => request_system,
        };
        let request_system = match &decided_section {
            Some(section) => {
                format!("{request_system}\n{}\n{section}", crate::decisions::HEADING)
            }
            None => request_system,
        };
        // M6-3c: instructions that arrived during the run join the
        // conversation at this boundary, in arrival order, before the next
        // request is built. The owner marks them as taken in the same save.
        if let Some(owner) = session.desktop_workflow.clone() {
            for text in owner.take_instructions().await.unwrap_or_default() {
                session.push_user(&text);
            }
            session.check_persistence()?;
        }
        // Call unconditionally every turn. If this were only called on
        // error, a call pattern that never triggers an error would never
        // trip MaxTurns, and the loop could run forever.
        if let Some(r) = stop.observe_turn() {
            return Err(AgentError::Stopped(r));
        }
        // A no-op for the root tracker (`StopTracker::new` never sets a
        // wall-clock deadline), and meaningful for a subagent tracker
        // built with `StopTracker::with_wall_seconds` (Task 9).
        if let Some(r) = stop.observe_wall_clock() {
            return Err(AgentError::Stopped(r));
        }

        if let Some(memory) = session.tool_memory.clone() {
            let report = memory
                .retain(
                    &mut session.messages,
                    session.decider.as_deref(),
                    skill_query,
                )
                .await;
            if report.stored > 0 || report.failed > 0 {
                audit.lock().await.record(&Record {
                    tool: "tool-memory",
                    detail: "retain",
                    sandbox: None,
                    target: None,
                    result: &format!(
                        "stored={} failed={} history_bytes_removed={}",
                        report.stored, report.failed, report.bytes_removed
                    ),
                    caller,
                })?;
            }
        }

        // The request is finished here, before the decision that reads it,
        // so that one value is both what gets counted and what gets sent.
        // Counting a separate reconstruction is what let the two drift: the
        // decision under-counted and an over-threshold request went out
        // uncompacted.
        //
        // The root's system prompt is rebuilt from a fresh read of the
        // project rules rather than reusing the one assembled before the
        // turn, so the reread happens here too.
        let refreshed_base = match &session.request_prompt {
            Some(prompt) => Some(prompt.system_for_request()?),
            None => None,
        };
        // The always-on floor is checked on its own, before the parts that
        // carry their own budgets are added: the workflow profile, the
        // skill catalogue and the selected MCP schemas each answer to their
        // own ceiling, and folding them in here would report a breach of a
        // budget none of them belongs to. What this catches is the bundled
        // prompt and tool definitions growing past what they are allowed —
        // a user's oversized rules are cut by `cap()` long before they
        // reach this, so this is not the check for those.
        // `system` has the workflow turn folded into it by this point, and
        // `base_system` is the same string before that; the turn belongs to
        // the workflow's budget, not this one.
        let floor = crate::budget::always_on_tokens(
            refreshed_base.as_deref().unwrap_or(base_system),
            tools,
        );
        if floor > crate::budget::BUDGET_LIMIT {
            return Err(std::io::Error::other(format!(
                "常時contextが{}tokensを超えました（{floor}tokens）",
                crate::budget::BUDGET_LIMIT
            ))
            .into());
        }
        let mut sending_system = match refreshed_base {
            Some(mut refreshed) => {
                if let Some(profile) = &current_profile {
                    refreshed.push('\n');
                    refreshed.push_str(&profile.body);
                } else if let Some(turn) = &workflow_turn {
                    refreshed.push('\n');
                    refreshed.push_str(&turn.body);
                }
                if let Some(catalog) = &catalog
                    && !catalog.text.is_empty()
                {
                    refreshed.push('\n');
                    refreshed.push_str(&catalog.text);
                }
                // This branch rebuilds the prompt from scratch, so every
                // section added above has to be added here too. Anything
                // left out is silently dropped for root runs only — the
                // one configuration where it was meant to apply.
                if let Some(section) = &filemap_section {
                    refreshed.push('\n');
                    refreshed.push_str(crate::filemap::HEADING);
                    refreshed.push('\n');
                    refreshed.push_str(section);
                }
                if let Some(section) = &decided_section {
                    refreshed.push('\n');
                    refreshed.push_str(crate::decisions::HEADING);
                    refreshed.push('\n');
                    refreshed.push_str(section);
                }
                refreshed
            }
            None => request_system,
        };
        // Resolving the preselection is what turns picks into schemas, so
        // it happens before the decision rather than after it. `freeze`
        // settles the selection for this iteration and must run once.
        if let Some(runtime) = mcp.as_deref_mut() {
            mcp_dispatch::refresh(runtime, mcp_control.as_ref()).await?;
        }
        let mcp_request = mcp.as_deref_mut().map(mcp_dispatch::prepare).transpose()?;
        let mut sending_tools = request_tools.clone();
        if let Some(prepared) = &mcp_request {
            sending_tools.extend_from_slice(&prepared.tools);
            if !prepared.metadata.is_empty() {
                sending_system.push('\n');
                sending_system.push_str(&prepared.metadata);
            }
        }
        let total_tokens = crate::budget::always_on_tokens(&sending_system, &sending_tools)
            + crate::compaction::session_tokens(&session.examples)
            // The memory evidence is deliberately absent. It exists only
            // under strict10/structured10, and compaction is disabled under
            // exactly those modes, so whenever this decision runs the
            // evidence is None. Counting it would be unreachable.
            + crate::compaction::session_tokens(&session.messages);
        if !strict_history
            && total_tokens
                >= session
                    .compaction_threshold
                    .unwrap_or(crate::compaction::COMPACTION_THRESHOLD)
        {
            match session.compact_automatically(provider).await {
                Ok(Some(report)) => {
                    // Only a completed compaction starts a new transport
                    // context; a no-op or failure retains this turn's state.
                    turn_context.clear();
                    if let Some(tx) = &events {
                        let _ = tx.send(crate::events::AgentEvent::HistoryCompacted {
                            messages_before: report.messages_before,
                            messages_after: report.messages_after,
                            tokens_before: report.tokens_before as u32,
                            tokens_after: report.tokens_after as u32,
                        });
                    }
                }
                Ok(None) => {}
                Err(_) => {
                    // Compaction is a best-effort optimization the turn
                    // doesn't depend on. A failure here (rate limit,
                    // transient error) must not block the turn itself —
                    // send this turn's full history rather than
                    // permanently bricking every subsequent turn on the
                    // same failure (session.messages stays over
                    // COMPACTION_THRESHOLD, so without this every future
                    // turn — including `/compact` itself — would retry
                    // and fail the same way until `/clear`/`/new`).
                }
            }
        }

        session.check_persistence()?;
        let mut request_messages = session.examples.clone();
        if let Some(evidence) = &historical_evidence {
            request_messages.push(evidence.clone());
        }
        request_messages.extend_from_slice(&session.messages);
        check_event_delivery(events.as_ref())?;
        let observer = events
            .as_ref()
            .map(crate::desktop_events::EventSink::response_observer)
            .transpose()
            .map_err(AgentError::Desktop)?
            .flatten();
        // Built before the compaction decision and carried here unchanged.
        // Anything appended past this point is weight the decision never
        // saw.
        let request = CompletionRequest {
            system: sending_system,
            messages: request_messages,
            tools: sending_tools,
        };
        {
            if let Some(catalog) = &catalog {
                audit.lock().await.record(&Record {
                    tool: "skill-catalog",
                    detail: "automatic_optional",
                    sandbox: None,
                    target: None,
                    result: &catalog.audit_result(),
                    caller,
                })?;
            }
            let counts = crate::budget::request_context_tokens(&request);
            audit.lock().await.record(&Record {
                tool: "workflow_context",
                detail: "reference_tokens",
                sandbox: None,
                target: None,
                result: &serde_json::to_string(&counts).map_err(std::io::Error::other)?,
                caller,
            })?;
        }
        if let Some(tx) = events.as_ref() {
            let _ = tx.send(crate::events::AgentEvent::ModelRequestStarted {
                turn: u64::from(stop.turns()),
            });
        }
        let res = match observer.as_ref() {
            Some(observer) => {
                provider
                    .complete_in_turn_with_observer(request, &turn_context, Some(observer))
                    .await
            }
            None => provider.complete_in_turn(request, &turn_context).await,
        };
        if let Some(tx) = events.as_ref() {
            let _ = tx.send(crate::events::AgentEvent::ModelRequestFinished {
                turn: u64::from(stop.turns()),
                ok: res.is_ok(),
            });
        }
        let res = res?;

        if res.tool_calls.is_empty() {
            let display_text = res.display_text();
            session.push_message(
                polaris_provider::Message::assistant(&res.text)
                    .with_reasoning(res.reasoning)
                    .with_hosted_web_search(res.hosted_web_search, res.url_citations),
                false,
            );
            session.check_persistence()?;
            let usage_report = include_summary_usage(
                meter.snapshot(),
                summary_before,
                session
                    .summary_usage
                    .as_ref()
                    .map(|meter| meter.snapshot())
                    .unwrap_or_default(),
            );
            return Ok(AgentOutcome {
                text: display_text,
                usage: usage_report.usage,
                usage_report,
            });
        }

        // Before sending the tool results, record this assistant turn
        // itself to the history with its tool_calls. Skip this and push
        // only the tool results, and on the next send the API side has
        // nothing to match "which call this result answers" against — a
        // real OpenAI endpoint rejects it (tests against a mock never
        // inspect the wire format, so they cannot detect this omission).
        session.push_message(
            polaris_provider::Message::assistant_with_tool_calls(&res.text, res.tool_calls.clone())
                .with_reasoning(res.reasoning)
                .with_hosted_web_search(res.hosted_web_search, res.url_citations),
            false,
        );
        session.check_persistence()?;

        let mut pending_changes = crate::dir_watch::DirChanges::default();
        let transitions = res
            .tool_calls
            .iter()
            .filter(|call| call.name == "state" && call.arguments["op"] == "transition")
            .count();
        let mut deferred_transition = None;
        let mut siblings_ok = true;
        for call in &res.tool_calls {
            if let Some(setup) = mcp_setup.as_ref()
                && call.name == "mcp_setup"
            {
                check_event_delivery(events.as_ref())?;
                mcp_dispatch::event(events.as_ref(), call, None);
                let cancelled = async {
                    match &events {
                        Some(crate::desktop_events::EventSink::Bounded(events)) => {
                            events.control().cancelled().await
                        }
                        _ => std::future::pending().await,
                    }
                };
                let outcome = setup
                    .call(&call.arguments, cancelled, ctx.gate, &mut *ctx.approver)
                    .await;
                let ok = outcome.is_ok();
                let body = outcome.unwrap_or_else(|e| e);
                session.push_tool_result(&call.id, &body);
                session.check_persistence()?;
                audit.lock().await.record(&Record {
                    tool: &call.name,
                    detail: &call.arguments.to_string(),
                    sandbox: None,
                    target: None,
                    result: &body,
                    caller,
                })?;
                mcp_dispatch::event(events.as_ref(), call, Some((&body, ok)));
                siblings_ok &= ok;
                if ok {
                    stop.observe_success();
                } else if let Some(reason) = stop.observe_error(&body) {
                    return Err(AgentError::Stopped(reason));
                }
                continue;
            }
            if let Some(tool) = setting.as_ref()
                && call.name == "setting"
            {
                check_event_delivery(events.as_ref())?;
                mcp_dispatch::event(events.as_ref(), call, None);
                let outcome = tool
                    .call(&call.arguments, ctx.sandbox, ctx.gate, &mut *ctx.approver)
                    .await;
                let ok = outcome.is_ok();
                let body = outcome.unwrap_or_else(|reason| reason);
                session.push_tool_result(&call.id, &body);
                session.check_persistence()?;
                audit.lock().await.record(&Record {
                    tool: &call.name,
                    detail: &call.arguments.to_string(),
                    sandbox: None,
                    target: None,
                    result: &body,
                    caller,
                })?;
                mcp_dispatch::event(events.as_ref(), call, Some((&body, ok)));
                siblings_ok &= ok;
                if ok {
                    stop.observe_success();
                } else if let Some(reason) = stop.observe_error(&body) {
                    return Err(AgentError::Stopped(reason));
                }
                continue;
            }
            if let Some(runtime) = mcp.as_deref_mut()
                && call.name.starts_with("mcp_")
            {
                check_event_delivery(events.as_ref())?;
                let generic_tool = call.name != "mcp_catalog";
                if generic_tool
                    && let Some(workflow) = &mut session.workflow
                    && let Some(verification) = &mut workflow.gates.verification
                {
                    verification.stale = true;
                }
                mcp_dispatch::event(events.as_ref(), call, None);
                check_event_delivery(events.as_ref())?;
                let reply = mcp_dispatch::dispatch(
                    runtime,
                    mcp_request.as_ref().expect("MCP request"),
                    call,
                    session,
                    mcp_control.as_ref(),
                    ctx,
                )
                .await?;
                let successful = reply.successful;
                let continue_run = reply.continue_run;
                let body = reply.message.content.clone();
                // This is the Writer's exact canonical message for remote
                // operations. It must enter both request view and full suffix.
                session.push_message(reply.message, false);
                session.check_persistence()?;
                audit.lock().await.record(&Record {
                    tool: &call.name,
                    detail: &call.arguments.to_string(),
                    sandbox: None,
                    target: None,
                    result: &body,
                    caller,
                })?;
                mcp_dispatch::event(events.as_ref(), call, Some((&body, successful)));
                siblings_ok &= successful;
                if !continue_run {
                    return Err(AgentError::McpUnconfirmed);
                }
                if successful {
                    stop.observe_success();
                } else if let Some(reason) = stop.observe_error(&body) {
                    return Err(AgentError::Stopped(reason));
                }
                continue;
            }
            if state_tool && call.name == "state" {
                workflow_control::event(events.as_ref(), call, None);
                let outcome = match workflow_control::parse(call) {
                    Ok(
                        workflow_control::StateOperation::Get
                        | workflow_control::StateOperation::Transition { .. },
                    ) if !is_adaptive => Err(workflow_control::error(
                        "unavailable",
                        "この実行では段階操作を使えません",
                    )),
                    Ok(workflow_control::StateOperation::Get) => Ok(workflow_control::get(session)),
                    Ok(workflow_control::StateOperation::Window { since }) => {
                        workflow_control::window(session, since)
                            .await
                            .map_err(|e| workflow_control::error("unavailable", &e))
                    }
                    Ok(workflow_control::StateOperation::Transition {
                        phase,
                        expected_revision,
                    }) if transitions == 1 => {
                        deferred_transition = Some((call, phase, expected_revision));
                        continue;
                    }
                    Ok(workflow_control::StateOperation::Transition { .. }) => {
                        Err(workflow_control::error(
                            "not_applied",
                            "同一応答に複数の段階変更があります",
                        ))
                    }
                    Err(e) => Err(workflow_control::error("invalid_arguments", &e)),
                };
                let ok = outcome.is_ok();
                let result = outcome.unwrap_or_else(|e| e);
                workflow_control::event(events.as_ref(), call, Some((&result, ok)));
                session.push_tool_result(&call.id, &result);
                session.check_persistence()?;
                if ok {
                    stop.observe_success();
                } else {
                    siblings_ok = false;
                    if let Some(reason) = stop.observe_error(&result) {
                        return Err(AgentError::Stopped(reason));
                    }
                }
                continue;
            }
            check_event_delivery(events.as_ref())?;
            // Read/bash (and spawn/skill) may consume the generated map.
            // Explicit map edits must also see the preceding regeneration.
            if !matches!(call.name.as_str(), "write" | "edit")
                || call.arguments["path"].as_str().is_some_and(|path| {
                    std::path::Path::new(path)
                        .file_name()
                        .is_some_and(|name| name == "files.md")
                })
            {
                flush_directory_changes(
                    &mut pending_changes,
                    agent_types,
                    provider_pool.clone(),
                    audit.clone(),
                    child_workflow.clone(),
                    ctx,
                )
                .await;
            }
            // The `files.md` regeneration hook. It fires *only* for the
            // root's own tool calls — see `pre_call_snapshot`'s docs for
            // why `caller == "root"` is a correctness condition and not a
            // mere optimization.
            let watched = caller == "root"
                && !session.disable_files_md_auto_regenerate
                && matches!(call.name.as_str(), "bash" | "write" | "edit");
            let pre_snapshot = watched.then(|| pre_call_snapshot(&call.name, call, ctx));

            let memory_path = (call.name == "read")
                .then(|| call.arguments["path"].as_str())
                .flatten()
                .filter(|path| path.starts_with("memory://"));
            let conversation_path = (call.name == "read")
                .then(|| call.arguments["path"].as_str())
                .flatten()
                .filter(|path| path.starts_with("conversation://"));
            let outcome = if let Some(path) = conversation_path {
                dispatch_conversation_read(
                    session.persistence.as_ref(),
                    session.desktop_memory.as_deref(),
                    session.strict_history.as_deref(),
                    call,
                    path,
                    events.as_ref(),
                )
                .await
            } else if let Some(path) = memory_path {
                dispatch_memory_read(session.tool_memory.as_ref(), call, path, events.as_ref())
                    .await
            } else {
                dispatch(
                    call,
                    lookup_skills,
                    agent_types,
                    provider_pool.clone(),
                    audit.clone(),
                    spawn_concurrency,
                    spawn_write_concurrency,
                    events.clone(),
                    child_workflow.clone(),
                    session.tier_decider.as_deref(),
                    session.decider.as_deref(),
                    ctx,
                )
                .await
            };

            siblings_ok &= outcome.is_ok();

            // `result` is exactly the body actually returned to the model
            // (on success) or the error text (on failure).
            // A command can partially mutate files even if it returns an error.
            // Existing verification cannot certify the resulting working tree.
            let may_change_artifact = matches!(call.name.as_str(), "write" | "edit" | "bash")
                || (call.name == "spawn"
                    && call.arguments["tasks"].as_array().is_some_and(|tasks| {
                        tasks.iter().any(|task| task["write_root"].is_string())
                    }));
            if may_change_artifact
                && let Some(workflow) = &mut session.workflow
                && let Some(verification) = &mut workflow.gates.verification
            {
                verification.stale = true;
            }
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
            // Held for exactly one `record()`. A subagent started by the
            // `spawn` call just above has already returned by now, so
            // there is no lock to contend with here — and holding it any
            // longer would be the deadlock described on `run`.
            audit.lock().await.record(&Record {
                tool: &call.name,
                detail: &call.arguments.to_string(),
                sandbox: is_mutation.then_some(ctx.sandbox),
                target: target.as_deref(),
                result,
                caller,
            })?;

            // Deliberately *after* the record above: the regeneration runs
            // its own subagent, whose tool calls write their own lines to
            // this same log, and a reader should find the `bash` / `write`
            // that caused them already recorded above rather than below.
            // Cause before effect. Reordering is safe because the `?` on
            // the record is the only fallible expression here and does not
            // depend on any of this.
            //
            // Nothing here can change `outcome` — a failure to regenerate
            // `files.md` is never allowed to turn a tool call the model
            // made into a failure (`regenerate_for_changes` returns `()`
            // and folds its own errors into the audit log for exactly this
            // reason).
            if let Some((scope, before)) = pre_snapshot {
                let after = crate::dir_watch::snapshot_for_scope(&scope);
                let full = crate::dir_watch::diff(&before, &after);
                let (changes, skipped) = cap_changes(full, MAX_REGENERATION_TARGETS_PER_CALL);
                if skipped > 0 {
                    // Truncating silently would be worse than the fan-out
                    // it prevents: `files.md` would simply be missing for
                    // some directories with nothing anywhere saying why.
                    let _ = audit.lock().await.record(&Record {
                        tool: "files-md-writer",
                        detail: &call.arguments.to_string(),
                        sandbox: None,
                        target: None,
                        result: &format!(
                            "files.md regeneration capped at {MAX_REGENERATION_TARGETS_PER_CALL} \
                             directories for this call; {skipped} skipped"
                        ),
                        caller: "harness",
                    });
                }
                pending_changes.new_dirs.extend(changes.new_dirs);
                pending_changes
                    .new_files_in_existing_dirs
                    .extend(changes.new_files_in_existing_dirs);
            }

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
                    session.push_tool_result(&call.id, &msg);
                    session.check_persistence()?;
                    if let Some(r) = stop.observe_error(&msg) {
                        flush_directory_changes(
                            &mut pending_changes,
                            agent_types,
                            provider_pool.clone(),
                            audit.clone(),
                            child_workflow.clone(),
                            ctx,
                        )
                        .await;
                        return Err(AgentError::Stopped(r));
                    }
                }
            }
            session.check_persistence()?;
        }
        flush_directory_changes(
            &mut pending_changes,
            agent_types,
            provider_pool.clone(),
            audit.clone(),
            child_workflow.clone(),
            ctx,
        )
        .await;
        if let Some((call, phase, revision)) = deferred_transition {
            check_event_delivery(events.as_ref())?;
            let outcome = if siblings_ok {
                workflow_control::apply(
                    session,
                    workflow_turn.as_ref().expect("adaptive turn"),
                    phase,
                    revision,
                    call,
                )
                .await?
            } else {
                Err(workflow_control::error(
                    "not_applied",
                    "同じ応答のtoolが失敗したため段階を変更しません",
                ))
            };
            let ok = outcome.is_ok();
            let result = outcome.unwrap_or_else(|e| e);
            if !ok {
                session.push_tool_result(&call.id, &result);
            }
            session.check_persistence()?;
            workflow_control::event(events.as_ref(), call, Some((&result, ok)));
            audit.lock().await.record(&Record {
                tool: "state",
                detail: &call.arguments.to_string(),
                sandbox: None,
                target: None,
                result: &result,
                caller,
            })?;
            if ok {
                stop.observe_success();
            } else if let Some(reason) = stop.observe_error(&result) {
                return Err(AgentError::Stopped(reason));
            }
        }
    }
}

fn include_summary_usage(
    mut main: polaris_provider::UsageReport,
    before: polaris_provider::UsageReport,
    after: polaris_provider::UsageReport,
) -> polaris_provider::UsageReport {
    main.usage.input_tokens = main.usage.input_tokens.saturating_add(
        after
            .usage
            .input_tokens
            .saturating_sub(before.usage.input_tokens),
    );
    main.usage.output_tokens = main.usage.output_tokens.saturating_add(
        after
            .usage
            .output_tokens
            .saturating_sub(before.usage.output_tokens),
    );
    main.usage.total_tokens = main.usage.total_tokens.saturating_add(
        after
            .usage
            .total_tokens
            .saturating_sub(before.usage.total_tokens),
    );
    main.usage.cached_tokens = main.usage.cached_tokens.saturating_add(
        after
            .usage
            .cached_tokens
            .saturating_sub(before.usage.cached_tokens),
    );
    main.reported_responses += after
        .reported_responses
        .saturating_sub(before.reported_responses);
    main.missing_responses += after
        .missing_responses
        .saturating_sub(before.missing_responses);
    main.failed_requests += after.failed_requests.saturating_sub(before.failed_requests);
    main
}

pub(crate) async fn dispatch_conversation_read(
    saved: Option<&crate::session_store::PersistedSession>,
    desktop: Option<&crate::desktop_memory::DesktopMemory>,
    history: Option<&crate::conversation_memory::StrictHistory>,
    call: &polaris_provider::ToolCall,
    path: &str,
    events: Option<&crate::desktop_events::EventSink>,
) -> Result<String, String> {
    if let Some(tx) = events {
        let _ = tx.send(crate::events::AgentEvent::ToolStarted {
            name: "read".into(),
            detail: call.arguments.to_string(),
        });
    }
    check_event_delivery(events).map_err(|error| error.to_string())?;
    let search = crate::conversation_memory::related::is_search_path(path);
    let result = if search
        && (path != crate::conversation_memory::related::SEARCH_URI
            || call
                .arguments
                .get("path")
                .and_then(serde_json::Value::as_str)
                != Some(path)
            || call
                .arguments
                .as_object()
                .is_none_or(|args| args.keys().any(|key| key != "path")))
    {
        Err("関連検索には予約URIだけを指定してください".into())
    } else if let Some(desktop) = desktop {
        desktop.read(path).await.map_err(|error| error.to_string())
    } else if search {
        async {
            let (Some(saved), Some(history)) = (saved, history) else {
                return Err("この実行経路では関連検索が利用できません".into());
            };
            saved.snapshot().map_err(|error| error.to_string())?;
            let result = history
                .search_related(&saved.store, &saved.database)
                .await
                .map_err(|error| error.to_string())?;
            saved.snapshot().map_err(|error| error.to_string())?;
            Ok(result)
        }
        .await
    } else {
        saved
            .ok_or_else(|| "会話原文の永続保存が有効ではありません".to_string())
            .and_then(|saved| {
                saved.snapshot().map_err(|error| error.to_string())?;
                crate::conversation_memory::read_source(&saved.store, &saved.database, path)
                    .map_err(|error| error.to_string())
            })
    };
    if let Some(tx) = events {
        let _ = tx.send(crate::events::AgentEvent::ToolFinished {
            name: "read".into(),
            detail: call.arguments.to_string(),
            ok: result.is_ok(),
            result: result.as_ref().map_or_else(Clone::clone, Clone::clone),
            diff: None,
        });
    }
    result
}

async fn dispatch_memory_read(
    memory: Option<&crate::tool_memory::ToolMemory>,
    call: &polaris_provider::ToolCall,
    path: &str,
    events: Option<&crate::desktop_events::EventSink>,
) -> Result<String, String> {
    if let Some(tx) = events {
        let _ = tx.send(crate::events::AgentEvent::ToolStarted {
            name: "read".into(),
            detail: call.arguments.to_string(),
        });
    }
    check_event_delivery(events).map_err(|error| error.to_string())?;
    let result = async {
        let memory = memory
            .ok_or("tool memory is disabled; enable --tool-memory to retrieve saved results")?;
        let parse = |key: &str, default: usize| -> Result<usize, String> {
            match call.arguments.get(key) {
                None => Ok(default),
                Some(value) => value
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .ok_or_else(|| format!("{key} must be a nonnegative integer")),
            }
        };
        memory
            .backend
            .read(
                path,
                parse("offset", 0)?,
                parse("limit", polaris_tools::read::DEFAULT_LIMIT)?,
            )
            .await
            .map_err(|error| error.to_string())
    }
    .await;
    if let Some(tx) = events {
        let _ = tx.send(crate::events::AgentEvent::ToolFinished {
            name: "read".into(),
            detail: call.arguments.to_string(),
            ok: result.is_ok(),
            result: result.as_ref().map_or_else(Clone::clone, Clone::clone),
            diff: None,
        });
    }
    result
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
#[allow(clippy::too_many_arguments)]
async fn dispatch(
    call: &polaris_provider::ToolCall,
    skills: &[polaris_skills::Skill],
    agent_types: &[polaris_skills::AgentType],
    provider_pool: Arc<dyn Provider>,
    audit: Arc<Mutex<AuditLog>>,
    spawn_concurrency: usize,
    spawn_write_concurrency: usize,
    events: Option<crate::desktop_events::EventSink>,
    workflow: Option<crate::spawn::ChildWorkflow>,
    tier_decider: Option<&dyn crate::tier::TierDecider>,
    // The run's judge, for the one thing the weight decider cannot answer:
    // which subagent type the model meant when it named one that does not
    // exist.
    decider: Option<&dyn polaris_provider::decision::Decider>,
    ctx: &mut ToolContext<'_>,
) -> Result<String, String> {
    if let Some(tx) = &events {
        let _ = tx.send(crate::events::AgentEvent::ToolStarted {
            name: call.name.clone(),
            detail: call.arguments.to_string(),
        });
    }

    check_event_delivery(events.as_ref()).map_err(|error| error.to_string())?;
    let mut pending_diff: Option<crate::events::Diff> = None;
    let owned_port = match events.as_ref() {
        Some(crate::desktop_events::EventSink::Bounded(sink)) => sink.execution(),
        _ => None,
    };

    let outcome: Result<String, String> = match call.name.as_str() {
        "read" => 'read: {
            let path = match call.arguments["path"].as_str() {
                Some(p) => p,
                None => break 'read Err("path is missing".to_string()),
            };
            let offset = call.arguments["offset"].as_u64().unwrap_or(0) as usize;
            let limit = call.arguments["limit"]
                .as_u64()
                .map(|n| n as usize)
                .unwrap_or(polaris_tools::read::DEFAULT_LIMIT);
            if ctx.sandbox.isolated_boundary().is_some() {
                let budget = match polaris_tools::isolated_read::output_budget() {
                    Ok(budget) => budget,
                    Err(error) => break 'read Err(error),
                };
                let request = polaris_tools::isolated_read::Request::Lines {
                    path: path.into(),
                    offset,
                    limit,
                    budget,
                };
                match isolated_read(owned_port, ctx.sandbox, ctx.helper, request).await {
                    Ok(polaris_tools::isolated_read::Reply::Text(text)) => Ok(text),
                    _ => Err("isolated read unavailable; no direct-read fallback".into()),
                }
            } else {
                polaris_tools::read::read(Path::new(path), offset, limit).map_err(|e| e.to_string())
            }
        }
        "write" => 'write: {
            let path = match call.arguments["path"].as_str() {
                Some(p) => p,
                None => break 'write Err("path is missing".to_string()),
            };
            let content = match call.arguments["content"].as_str() {
                Some(c) => c,
                None => break 'write Err("content is missing".to_string()),
            };
            let path = Path::new(path);
            let approval = if owned_port.is_some() {
                ctx.gate.check_owned_write(ctx.sandbox, path)
            } else {
                ctx.gate.check_async(ctx.sandbox, path, ctx.approver).await
            };
            if let Err(e) = approval {
                break 'write Err(e);
            }
            check_event_delivery(events.as_ref()).map_err(|error| error.to_string())?;
            if let Some(port) = owned_port {
                break 'write owned_mutation(
                    port,
                    ctx.sandbox,
                    ctx.helper,
                    path,
                    polaris_sandbox::Mutation::Write {
                        path: path.into(),
                        content: content.into(),
                    },
                )
                .await;
            }
            // diffを送るため、上書き前の内容を先に読んでおく。読めない
            // (=存在しない)なら新規ファイル扱い。読み取り自体の失敗は
            // write本体の成否に影響させない——diff計算はベストエフォート
            // の副作用であり、読めないことがwrite自体を失敗させる理由に
            // はならない。
            //
            // ただし`polaris_tools::read`が常に拒否するパス(`.env`、秘密鍵
            // 等)はここでも読まない。`Gate::check`は`SandboxMode::FullAccess`
            // では`is_denied`を見ずに`Allowed`を返すため、素の
            // `read_to_string`だとreadツールなら決して見せない内容が
            // diffとして端末に出てしまう。読めなかった場合と同じ扱い
            // (=diffなし)にして、write本体の可否には手を触れない。
            let (old_content, diff_available) = if ctx.sandbox.isolated_boundary().is_some() {
                // 本文取得は隔離helperだけ。owner不在・拒否は差分を省略する。
                match isolated_read(
                    owned_port,
                    ctx.sandbox,
                    ctx.helper,
                    polaris_tools::isolated_read::Request::DiffBefore { path: path.into() },
                )
                .await
                {
                    Ok(polaris_tools::isolated_read::Reply::Text(text)) => (Some(text), true),
                    Ok(polaris_tools::isolated_read::Reply::Missing) => (None, true),
                    _ => (None, false),
                }
            } else if polaris_tools::path_policy::is_denied(path) {
                (None, true)
            } else {
                (std::fs::read_to_string(path).ok(), true)
            };
            let result = polaris_tools::write::write(ctx.sandbox, ctx.helper, path, content)
                .map_err(|e| e.to_string());
            if result.is_ok() && diff_available {
                let mut diff =
                    crate::events::compute_diff(old_content.as_deref().unwrap_or(""), content);
                diff.is_new_file = old_content.is_none();
                pending_diff = Some(diff);
            }
            result
        }
        "edit" => 'edit: {
            let path = match call.arguments["path"].as_str() {
                Some(p) => p,
                None => break 'edit Err("path is missing".to_string()),
            };
            let old = match call.arguments["old"].as_str() {
                Some(o) => o,
                None => break 'edit Err("old is missing".to_string()),
            };
            let new = match call.arguments["new"].as_str() {
                Some(n) => n,
                None => break 'edit Err("new is missing".to_string()),
            };
            let path = Path::new(path);
            let approval = if owned_port.is_some() {
                ctx.gate.check_owned_write(ctx.sandbox, path)
            } else {
                ctx.gate.check_async(ctx.sandbox, path, ctx.approver).await
            };
            if let Err(e) = approval {
                break 'edit Err(e);
            }
            check_event_delivery(events.as_ref()).map_err(|error| error.to_string())?;
            if let Some(port) = owned_port {
                break 'edit owned_mutation(
                    port,
                    ctx.sandbox,
                    ctx.helper,
                    path,
                    polaris_sandbox::Mutation::Edit {
                        path: path.into(),
                        old: old.into(),
                        new: new.into(),
                    },
                )
                .await;
            }
            let result = polaris_tools::edit::edit(ctx.sandbox, ctx.helper, path, old, new)
                .map_err(|e| e.to_string());
            if result.is_ok() {
                pending_diff = Some(crate::events::compute_diff(old, new));
            }
            result
        }
        "bash" => 'bash: {
            let command = match call.arguments["command"].as_str() {
                Some(c) => c,
                None => break 'bash Err("command is missing".to_string()),
            };
            let approval = if owned_port.is_some() {
                ctx.gate.check_owned_command(ctx.sandbox, command)
            } else {
                ctx.gate
                    .check_command_async(ctx.sandbox, command, ctx.approver)
                    .await
            };
            if let Err(e) = approval {
                break 'bash Err(e);
            }
            check_event_delivery(events.as_ref()).map_err(|error| error.to_string())?;
            if let Some(crate::desktop_events::EventSink::Bounded(sink)) = events.as_ref()
                && let Some(port) = sink.execution()
            {
                let submitted = if port.has_execution_ceiling() {
                    port.submit_confined_execution(ctx.sandbox, ctx.helper, command)
                } else {
                    port.submit(crate::desktop_execution::ExecutionCommand {
                        policy: ctx.sandbox.clone(),
                        program: PathBuf::from("/bin/sh"),
                        args: vec!["-c".into(), command.into()],
                        stdin: None,
                    })
                };
                let waiting = match submitted {
                    Ok(waiting) => waiting,
                    Err(error) => break 'bash Err(error),
                };
                let result = match waiting.await {
                    Ok(result) => result,
                    Err(error) => break 'bash Err(error),
                };
                let text = result.text();
                if result.end == crate::desktop_execution::ControlledEnd::Exited
                    && result.status == Some(0)
                    && result.problem.is_none()
                {
                    Ok(text)
                } else {
                    Err(text)
                }
            } else {
                polaris_tools::bash::run(ctx.sandbox, command).map_err(|e| e.to_string())
            }
        }
        "skill" => 'skill: {
            let q = match call.arguments["q"].as_str() {
                Some(q) => q,
                None => break 'skill Err("q is missing".to_string()),
            };
            Ok(lookup_skill(skills, q))
        }
        "spawn" => 'spawn: {
            let tasks_json = match call.arguments["tasks"].as_array() {
                Some(t) => t,
                None => break 'spawn Err("tasks is missing".to_string()),
            };
            let mut tasks = Vec::with_capacity(tasks_json.len());
            for t in tasks_json {
                // **The type is optional.** Naming one costs the
                // orchestrator a decision it is the most expensive party in
                // the run to make: it has no catalogue in its context, so it
                // either guesses a name or spends a round trip looking one
                // up, and both the input and the output of that round trip
                // are billed at the orchestrator's rate. The judge holds the
                // catalogue for a rate that does not register — measured on
                // 2026-09-18 at 0.04% of a run. So the ordinary path is to
                // describe the work and let the judge match it to a type.
                let named = t["type"].as_str();
                let task = match t["task"].as_str() {
                    Some(task) => task.to_string(),
                    None => break 'spawn Err("tasks[].task is missing".to_string()),
                };
                let write_root = t["write_root"].as_str().map(str::to_string);
                // A name the catalogue does have is taken as given: the
                // orchestrator that bothered to name a real one has said
                // something the judge should not overrule.
                let recognised =
                    named.filter(|name| agent_types.iter().any(|held| &held.name == name));
                let agent_type = match (recognised, decider) {
                    (Some(name), _) => name.to_string(),
                    // Either nothing was named, or what was named is not
                    // real. Both are the same question: which of the types
                    // this run actually offers does the work described?
                    (None, Some(decider)) => {
                        match crate::spawn::resolved_type(decider, agent_types, named, &task).await
                        {
                            Some(resolved) => {
                                // Choosing silently would let the person
                                // believe a type they never read is the one
                                // that ran — and when a name *was* given,
                                // that the one they read is the one that ran.
                                let recorded = audit.lock().await.record(&Record {
                                    tool: "spawn-type",
                                    detail: named.unwrap_or("(型の指定なし)"),
                                    sandbox: None,
                                    target: None,
                                    result: &resolved,
                                    caller: "root",
                                });
                                if let Err(error) = recorded {
                                    break 'spawn Err(error.to_string());
                                }
                                resolved
                            }
                            None => match named {
                                Some(name) => name.to_string(),
                                // No name, and the judge could not choose.
                                // Inventing one here would run something
                                // nobody asked for.
                                None => {
                                    break 'spawn Err(
                                        "tasks[].type がなく、判定器も型を選べませんでした"
                                            .to_string(),
                                    );
                                }
                            },
                        }
                    }
                    // No judge. A name that is not in the catalogue is
                    // passed through unchanged so the failure is reported
                    // where it already is, by name; no name at all cannot be
                    // resolved by anyone here.
                    (None, None) => match named {
                        Some(name) => name.to_string(),
                        None => {
                            break 'spawn Err(
                                "tasks[].type がありません（判定器が無い場合は型名が要ります）"
                                    .to_string(),
                            );
                        }
                    },
                };
                tasks.push(crate::spawn::SpawnTask {
                    agent_type,
                    task,
                    write_root,
                });
            }
            // Boxed to break the type-level cycle
            // `run_loop -> dispatch -> run_wave -> run_one -> run_loop`.
            // Depth is fixed at 1 so this never recurses at runtime (a
            // subagent's tool list has no `spawn` in it), but the compiler
            // still has to give the future a finite size.
            Ok(Box::pin(crate::spawn::run_wave_with_gate(
                tasks,
                agent_types,
                provider_pool,
                audit,
                ctx.sandbox,
                ctx.helper,
                spawn_concurrency,
                spawn_write_concurrency,
                events.clone(),
                workflow,
                ctx.gate,
                tier_decider,
            ))
            .await)
        }
        other => Err(format!("unknown tool: {other}")),
    };

    if let Some(tx) = &events {
        let _ = tx.send(crate::events::AgentEvent::ToolFinished {
            name: call.name.clone(),
            detail: call.arguments.to_string(),
            ok: outcome.is_ok(),
            // 成功なら本文、失敗ならエラーメッセージ——どちらも
            // `Result<String, String>`の中身をそのまま渡し、短く切るのは
            // 表示側(`format_event_for_live_print`)に任せる。
            result: match &outcome {
                Ok(body) => body.clone(),
                Err(msg) => msg.clone(),
            },
            diff: pending_diff,
        });
    }

    outcome
}

fn lookup_skill(skills: &[polaris_skills::Skill], query: &str) -> String {
    let query = query.trim();
    if skills.iter().filter(|skill| skill.name == query).count() > 1 {
        return "同名のskillが複数あるため、本文を開示できません。".into();
    }
    polaris_tools::skill::lookup(skills, query)
}

/// CLIの隔離helper入口だけが呼ぶ。親のdispatchからは呼ばない。
/// CLIは既存のcore依存を使い、本文処理はtoolsへ委譲する。
pub fn serve_confined_read(input: impl std::io::Read) -> Vec<u8> {
    polaris_tools::isolated_read::serve(input)
}

/// 隔離readは既存service ownerへ要求する。待機futureのDropが取消を通知し、
/// childと未回収handleはserviceに残る。owner不在時に親で本文を読まない。
async fn isolated_read(
    port: Option<&crate::desktop_execution::ExecutionPort>,
    policy: &polaris_sandbox::SandboxPolicy,
    helper: &Path,
    request: polaris_tools::isolated_read::Request,
) -> Result<polaris_tools::isolated_read::Reply, String> {
    let port = port.ok_or("isolated read requires an execution owner")?;
    let payload = serde_json::to_string(&request).map_err(|_| "invalid isolated read request")?;
    if payload.len() > polaris_tools::isolated_read::REQUEST_LIMIT {
        return Err("isolated read request too large".into());
    }
    let readonly = policy
        .restrict(polaris_sandbox::SandboxMode::ReadOnly, &[])
        .map_err(|_| "isolated read policy could not be restricted")?;
    let waiting = if port.has_read_helper() {
        if !port.read_helper_matches(helper, &readonly) {
            return Err("isolated read helper grant mismatch".into());
        }
        port.submit_confined_read(&request)?
    } else {
        port.submit(crate::desktop_execution::ExecutionCommand {
            policy: readonly,
            program: helper.into(),
            args: vec!["--confined-read".into()],
            stdin: Some(payload),
        })?
    };
    let result = tokio::time::timeout(std::time::Duration::from_secs(5), waiting)
        .await
        .map_err(|_| "isolated read timed out; cleanup remains owned")??;
    if result.end != crate::desktop_execution::ControlledEnd::Exited
        || result.status != Some(0)
        || result.problem.is_some()
        || !result.stdout_eof
        || !result.stderr_eof
        || result.stdout_truncated
        || result.stderr_truncated
    {
        // 失敗時のstdout/stderrを本文として露出させない。
        return Err("isolated read unavailable".into());
    }
    polaris_tools::isolated_read::decode(&result.stdout, &request)
}

async fn owned_mutation(
    port: &crate::desktop_execution::ExecutionPort,
    policy: &polaris_sandbox::SandboxPolicy,
    helper: &Path,
    path: &Path,
    mutation: polaris_sandbox::Mutation,
) -> Result<String, String> {
    let waiting = if port.has_mutation_grant() {
        port.submit_confined_mutation(policy, helper, &mutation)?
    } else {
        let payload = serde_json::to_string(&mutation).map_err(|e| e.to_string())?;
        port.submit(crate::desktop_execution::ExecutionCommand {
            policy: policy.clone(),
            program: helper.into(),
            args: vec!["--confined-apply".into()],
            stdin: Some(payload),
        })?
    };
    let result = waiting.await?;
    if result.end != crate::desktop_execution::ControlledEnd::Exited
        || result.problem.is_some()
        || !result.stdout_eof
        || !result.stderr_eof
    {
        return Err(result.text());
    }
    let status = result.status.ok_or_else(|| result.text())?;
    polaris_tools::write::interpret_mutation_result(
        policy,
        path,
        status,
        &result.stdout,
        &result.stderr,
    )
    .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    #[tokio::test]
    async fn related_search_dispatch_rejects_options_and_unsupported_owner_without_fallback() {
        use crate::conversation_memory::related::SEARCH_URI;
        let call = |arguments| polaris_provider::ToolCall {
            id: "related-read".into(),
            name: "read".into(),
            arguments,
        };
        for arguments in [
            serde_json::json!({"path": SEARCH_URI, "offset": 0}),
            serde_json::json!({"path": SEARCH_URI, "limit": 1}),
            serde_json::json!({"path": SEARCH_URI, "query": "different text"}),
            serde_json::json!({"path": format!("{SEARCH_URI}?offset=1")}),
        ] {
            let path = arguments["path"].as_str().unwrap().to_owned();
            let result =
                super::dispatch_conversation_read(None, None, None, &call(arguments), &path, None)
                    .await;
            assert_eq!(
                result.unwrap_err(),
                "関連検索には予約URIだけを指定してください"
            );
        }
        assert_eq!(
            super::dispatch_conversation_read(
                None,
                None,
                None,
                &call(serde_json::json!({"path": SEARCH_URI})),
                SEARCH_URI,
                None
            )
            .await
            .unwrap_err(),
            "この実行経路では関連検索が利用できません"
        );
        assert!(crate::conversation_memory::parse_uri(SEARCH_URI).is_err());
    }

    use super::*;
    use polaris_provider::{CompletionResponse, ToolCall};
    use std::sync::Mutex;

    #[tokio::test]
    async fn owned_mutation_uses_typed_grant_and_keeps_ungranted_requests_ordinary() {
        use crate::desktop_execution::{ExecutionPort, ExecutionResult};
        use polaris_desktop_protocol::ids::RunId;
        use polaris_sandbox::{Mutation, SandboxMode, SandboxPolicy};
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        for name in ["workspace", "workspace/home", "workspace/tmp", "runtime"] {
            std::fs::create_dir(base.join(name)).unwrap();
        }
        let helper = base.join("runtime/helper");
        std::fs::write(&helper, "dummy, not executed").unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o500)).unwrap();
        let policy = SandboxPolicy::isolated(
            SandboxMode::WorkspaceWrite,
            &base.join("workspace"),
            &[base.join("runtime")],
        )
        .unwrap()
        .with_isolated_environment(&base.join("workspace/home"), &base.join("workspace/tmp"))
        .unwrap();
        let run = RunId::new("mutation-dispatch").unwrap();
        for granted in [false, true] {
            let (port, mut receiver) = ExecutionPort::channel_with_read_helper_and_mutations(
                run.clone(),
                helper.clone(),
                policy.restrict(SandboxMode::ReadOnly, &[]).unwrap(),
                granted.then(|| policy.clone()),
            )
            .unwrap();
            for mutation in [
                Mutation::Write {
                    path: "file".into(),
                    content: "new".into(),
                },
                Mutation::Edit {
                    path: "file".into(),
                    old: "new".into(),
                    new: "edited".into(),
                },
            ] {
                let execute = owned_mutation(&port, &policy, &helper, Path::new("file"), mutation);
                let inspect = async {
                    let request = receiver.recv().await.unwrap();
                    assert_eq!(request.is_authorized_mutation_for(&run), granted);
                    assert_eq!(request.command.program, helper);
                    assert_eq!(request.command.args, ["--confined-apply"]);
                    let decoded: Mutation =
                        serde_json::from_str(request.command.stdin.as_ref().unwrap()).unwrap();
                    let path = match decoded {
                        Mutation::Write { path, .. } | Mutation::Edit { path, .. } => path,
                    };
                    assert_eq!(
                        path,
                        if granted {
                            base.join("workspace/file")
                        } else {
                            PathBuf::from("file")
                        }
                    );
                    let mut result = ExecutionResult::not_started("fixture".into());
                    result.end = crate::desktop_execution::ControlledEnd::Exited;
                    result.status = Some(0);
                    result.problem = None;
                    result.stdout = "fixed helper result".into();
                    request.reply.send(Arc::new(result)).unwrap();
                };
                let (result, ()) = tokio::join!(execute, inspect);
                assert_eq!(result.unwrap(), "fixed helper result");
            }
        }
    }

    #[tokio::test]
    async fn isolated_read_dispatch_never_falls_back_to_parent() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("ordinary.txt");
        std::fs::write(&path, "DUMMY_PARENT_READ_PROOF").unwrap();
        let call = polaris_provider::ToolCall {
            id: "read-test".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": path}),
        };
        for isolated in [false, true] {
            let policy = if isolated {
                polaris_sandbox::SandboxPolicy::isolated(
                    polaris_sandbox::SandboxMode::ReadOnly,
                    dir.path(),
                    &[],
                )
                .unwrap()
            } else {
                polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[])
                    .unwrap()
            };
            let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
            let mut approver = AutoApprove;
            let mut ctx = ToolContext {
                sandbox: &policy,
                helper: Path::new("/must-not-run"),
                gate: &mut gate,
                approver: &mut approver,
            };
            let result = dispatch(
                &call,
                &[],
                &[],
                unused_provider_pool(),
                dummy_audit(&dir),
                1,
                1,
                None,
                None,
                None,
                None,
                &mut ctx,
            )
            .await;
            if isolated {
                assert!(result.is_err());
            } else {
                assert!(result.unwrap().contains("DUMMY_PARENT_READ_PROOF"));
            }
        }
    }
    #[tokio::test]
    async fn isolated_read_deadline_signals_owner() {
        let dir = tempfile::tempdir().unwrap();
        let policy = polaris_sandbox::SandboxPolicy::isolated(
            polaris_sandbox::SandboxMode::ReadOnly,
            dir.path(),
            &[],
        )
        .unwrap();
        let (port, mut rx) = crate::desktop_execution::ExecutionPort::channel(
            polaris_desktop_protocol::ids::RunId::new("isolated-deadline").unwrap(),
        );
        let waiting = tokio::spawn(async move {
            isolated_read(
                Some(&port),
                &policy,
                Path::new("/dummy-helper"),
                isolated_dummy_request(),
            )
            .await
        });
        let request = rx.recv().await.unwrap();
        let result = tokio::time::timeout(std::time::Duration::from_secs(7), waiting)
            .await
            .unwrap()
            .unwrap();
        assert!(result.unwrap_err().contains("timed out"));
        assert!(request.cancelled.load(std::sync::atomic::Ordering::Acquire));
    }

    fn isolated_dummy_request() -> polaris_tools::isolated_read::Request {
        polaris_tools::isolated_read::Request::Lines {
            path: PathBuf::from("/does-not-exist/dummy.txt"),
            offset: 3,
            limit: 2,
            budget: 1024,
        }
    }
    fn isolated_dummy_result(stdout: String) -> crate::desktop_execution::ExecutionResult {
        crate::desktop_execution::ExecutionResult {
            end: crate::desktop_execution::ControlledEnd::Exited,
            status: Some(0),
            stdout,
            stderr: String::new(),
            stdout_truncated: false,
            stderr_truncated: false,
            stdout_eof: true,
            stderr_eof: true,
            problem: None,
        }
    }
    #[tokio::test]
    async fn isolated_read_uses_owner_without_parent_file_read() {
        let dir = tempfile::tempdir().unwrap();
        let policy = polaris_sandbox::SandboxPolicy::isolated(
            polaris_sandbox::SandboxMode::FullAccess,
            dir.path(),
            &[],
        )
        .unwrap();
        let home = dir.path().join("home");
        let tmp = dir.path().join("tmp");
        std::fs::create_dir(&home).unwrap();
        std::fs::create_dir(&tmp).unwrap();
        let policy = policy.with_isolated_environment(&home, &tmp).unwrap();
        let boundary = policy.isolated_boundary().unwrap().clone();
        let (port, mut rx) = crate::desktop_execution::ExecutionPort::channel(
            polaris_desktop_protocol::ids::RunId::new("isolated-read").unwrap(),
        );
        let owner = tokio::spawn(async move {
            let request = rx.recv().await.unwrap();
            assert_eq!(request.command.args, ["--confined-read"]);
            assert_eq!(
                request.command.policy.mode(),
                polaris_sandbox::SandboxMode::ReadOnly
            );
            assert!(request.command.policy.writable_roots().is_empty());
            let narrowed = request.command.policy.isolated_boundary().unwrap();
            assert_eq!(narrowed.workspace, boundary.workspace);
            assert_eq!(narrowed.readable_roots, boundary.readable_roots);
            assert_eq!(
                narrowed.environment.as_ref().unwrap().home,
                boundary.environment.as_ref().unwrap().home
            );
            assert_eq!(
                narrowed.environment.as_ref().unwrap().tmpdir,
                boundary.environment.as_ref().unwrap().tmpdir
            );
            let wire: polaris_tools::isolated_read::Request =
                serde_json::from_str(request.command.stdin.as_ref().unwrap()).unwrap();
            assert!(matches!(
                wire,
                polaris_tools::isolated_read::Request::Lines {
                    offset: 3,
                    limit: 2,
                    budget: 1024,
                    ..
                }
            ));
            request
                .reply
                .send(Arc::new(isolated_dummy_result(
                    "{\"Text\":\"4\\tdummy\\n\"}".into(),
                )))
                .unwrap();
        });
        let result = isolated_read(
            Some(&port),
            &policy,
            Path::new("/dummy-helper"),
            isolated_dummy_request(),
        )
        .await
        .unwrap();
        assert_eq!(
            result,
            polaris_tools::isolated_read::Reply::Text("4\tdummy\n".into())
        );
        owner.await.unwrap();
        assert!(
            isolated_read(
                None,
                &policy,
                Path::new("/dummy-helper"),
                isolated_dummy_request()
            )
            .await
            .is_err()
        );
    }
    #[tokio::test]
    async fn isolated_read_uses_trusted_grant_and_rejects_mismatched_helper() {
        use crate::desktop_execution::{ExecutionPort, SandboxMode, SandboxPolicy};
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let base = dir.path().canonicalize().unwrap();
        for name in ["workspace", "workspace/home", "workspace/tmp", "runtime"] {
            std::fs::create_dir(base.join(name)).unwrap();
        }
        let helper = base.join("runtime/helper");
        std::fs::write(&helper, "dummy, never executed").unwrap();
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700)).unwrap();
        let policy = SandboxPolicy::isolated(
            SandboxMode::FullAccess,
            &base.join("workspace"),
            &[base.join("runtime")],
        )
        .unwrap()
        .with_isolated_environment(&base.join("workspace/home"), &base.join("workspace/tmp"))
        .unwrap();
        let run = polaris_desktop_protocol::ids::RunId::new("trusted-read").unwrap();
        let (port, mut rx) = ExecutionPort::channel_with_read_helper(
            run.clone(),
            helper.clone(),
            policy.restrict(SandboxMode::ReadOnly, &[]).unwrap(),
        )
        .unwrap();
        assert!(
            isolated_read(
                Some(&port),
                &policy,
                Path::new("/mismatch"),
                isolated_dummy_request()
            )
            .await
            .is_err()
        );
        assert!(rx.try_recv().is_err());
        let owner = tokio::spawn(async move {
            let request = rx.recv().await.unwrap();
            assert!(request.is_authorized_read_for(&run));
            request
                .reply
                .send(Arc::new(isolated_dummy_result(
                    "{\"Text\":\"4\\tdummy\\n\"}".into(),
                )))
                .unwrap();
        });
        assert!(
            isolated_read(Some(&port), &policy, &helper, isolated_dummy_request())
                .await
                .is_ok()
        );
        owner.await.unwrap();
    }
    #[tokio::test]
    async fn isolated_read_rejects_partial_or_unconfirmed_output() {
        let dir = tempfile::tempdir().unwrap();
        let policy = polaris_sandbox::SandboxPolicy::isolated(
            polaris_sandbox::SandboxMode::ReadOnly,
            dir.path(),
            &[],
        )
        .unwrap();
        for kind in 0..4 {
            let (port, mut rx) = crate::desktop_execution::ExecutionPort::channel(
                polaris_desktop_protocol::ids::RunId::new("isolated-invalid").unwrap(),
            );
            let owner = tokio::spawn(async move {
                let request = rx.recv().await.unwrap();
                let mut result = isolated_dummy_result("DUMMY_UNTRUSTED_OUTPUT".into());
                match kind {
                    0 => result.stdout_truncated = true,
                    1 => result.stdout_eof = false,
                    2 => result.end = crate::desktop_execution::ControlledEnd::StopUnconfirmed,
                    _ => {}
                }
                request.reply.send(Arc::new(result)).unwrap();
            });
            let error = isolated_read(
                Some(&port),
                &policy,
                Path::new("/dummy-helper"),
                isolated_dummy_request(),
            )
            .await
            .unwrap_err();
            assert!(!error.contains("DUMMY_UNTRUSTED_OUTPUT"));
            owner.await.unwrap();
        }
    }
    #[tokio::test]
    async fn isolated_read_cancel_keeps_owner_and_signals_waiter() {
        let dir = tempfile::tempdir().unwrap();
        let policy = polaris_sandbox::SandboxPolicy::isolated(
            polaris_sandbox::SandboxMode::ReadOnly,
            dir.path(),
            &[],
        )
        .unwrap();
        let (port, mut rx) = crate::desktop_execution::ExecutionPort::channel(
            polaris_desktop_protocol::ids::RunId::new("isolated-cancel").unwrap(),
        );
        let waiting = tokio::spawn(async move {
            isolated_read(
                Some(&port),
                &policy,
                Path::new("/dummy-helper"),
                isolated_dummy_request(),
            )
            .await
        });
        let request = rx.recv().await.unwrap();
        waiting.abort();
        let _ = waiting.await;
        assert!(request.cancelled.load(std::sync::atomic::Ordering::Acquire));
    }

    /// The tests share one audit log per temp directory, wrapped exactly
    /// the way production wraps it (see `run`'s docs on why it is shared
    /// rather than borrowed exclusively).
    fn shared_audit(path: &std::path::Path) -> Arc<tokio::sync::Mutex<AuditLog>> {
        Arc::new(tokio::sync::Mutex::new(
            AuditLog::open(path).expect("cannot open"),
        ))
    }

    fn dummy_audit(dir: &tempfile::TempDir) -> Arc<tokio::sync::Mutex<AuditLog>> {
        shared_audit(&dir.path().join("audit.jsonl"))
    }

    /// A provider that panics if it is ever asked to complete anything.
    /// `provider_pool` only ever reaches the `spawn` arm, and no test in
    /// this module exercises it — `spawn`'s own behavior is covered in
    /// `crate::spawn`'s tests. Panicking rather than returning a default
    /// makes any accidental use of this argument visible instead of
    /// silently producing an empty turn.
    struct NeverCalled;

    #[async_trait::async_trait]
    impl Provider for NeverCalled {
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
            panic!("the provider pool was used by a test that should never reach the spawn arm")
        }
    }

    fn unused_provider_pool() -> Arc<dyn Provider> {
        Arc::new(NeverCalled)
    }

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

    #[tokio::test]
    async fn adaptive_workflow_replaces_phase_context_and_preserves_durable_results() {
        use crate::workflow::{Phase, SessionWorkflow, WorkflowMode};
        struct Adaptive {
            requests: Mutex<Vec<CompletionRequest>>,
            storage: crate::session_store::PersistedSession,
        }
        #[async_trait::async_trait]
        impl Provider for Adaptive {
            async fn complete(
                &self,
                req: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                let mut requests = self.requests.lock().unwrap();
                let index = requests.len();
                let state = self.storage.snapshot().unwrap().state;
                assert_eq!(state.workflow.revision, index as u64);
                let phase = [
                    Phase::General,
                    Phase::Implement,
                    Phase::Verify,
                    Phase::Deliver,
                ][index];
                assert_eq!(state.workflow.phase, phase);
                assert!(req.system.contains(&format!("phase={phase}")));
                assert_eq!(
                    req.system.matches("Required workflow instructions").count(),
                    1
                );
                for other in Phase::ALL {
                    let heading = format!("### builtin:{other}\n");
                    assert_eq!(
                        req.system.contains(&heading),
                        other == phase && phase != Phase::General
                    );
                }
                assert_eq!(req.tools.iter().filter(|s| s.name == "state").count(), 1);
                if index > 0 {
                    let last = req.messages.last().unwrap();
                    assert_eq!(last.tool_call_id, Some(format!("transition-{index}")));
                    let value: serde_json::Value = serde_json::from_str(&last.content).unwrap();
                    assert_eq!(value["revision"], index);
                    assert_eq!(value["status"], "applied");
                }
                let baseline = crate::prompt::assemble_always_on("", "", &[]);
                let workflow_tokens = crate::budget::count_tokens(
                    req.system.strip_prefix(baseline.system()).unwrap().trim(),
                );
                assert!(workflow_tokens <= crate::workflow::WORKFLOW_TOKEN_LIMIT);
                for (transport, body, base_wire, control_wire) in [
                    (
                        "openai",
                        polaris_provider::openai::build_body("fixture", &req, Some("medium")),
                        polaris_provider::openai::tool_wire_shape(baseline.tools()),
                        polaris_provider::openai::tool_wire_shape(&[polaris_tools::state_spec()]),
                    ),
                    (
                        "codex",
                        polaris_provider::codex::build_body("fixture", &req, Some("medium")),
                        polaris_provider::codex::tool_wire_shape(baseline.tools()),
                        polaris_provider::codex::tool_wire_shape(&[polaris_tools::state_spec()]),
                    ),
                ] {
                    let baseline_tokens = crate::budget::count_tokens(baseline.system())
                        + crate::budget::count_tokens(&serde_json::to_string(&base_wire).unwrap());
                    let state_tokens =
                        crate::budget::count_tokens(&serde_json::to_string(&control_wire).unwrap());
                    assert!(baseline_tokens <= 990 && state_tokens <= 256);
                    assert!(baseline_tokens + workflow_tokens + state_tokens <= 1630);
                    let wire_tokens = crate::budget::count_tokens(&body.to_string());
                    println!(
                        "workflow_budget transport={transport} phase={phase} baseline={baseline_tokens} workflow={workflow_tokens} state={state_tokens} full_fixture_wire={wire_tokens}"
                    );
                    if let Some(previous) = requests.last() {
                        assert_ne!(
                            polaris_provider::cache_key("fixture", Some("medium"), previous),
                            body["prompt_cache_key"].as_str().unwrap()
                        );
                    }
                }
                requests.push(req);
                if index == 3 {
                    return Ok(CompletionResponse {
                        text: "done".into(),
                        ..Default::default()
                    });
                }
                Ok(CompletionResponse {
                    tool_calls: vec![ToolCall {
                        id: format!("transition-{}", index + 1),
                        name: "state".into(),
                        arguments: serde_json::json!({"op":"transition","phase":(["implement","verify","deliver"][index]),"expected_revision":index}),
                    }],
                    ..Default::default()
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let mut config = crate::config::Config::default().workflow;
        config.mode = WorkflowMode::Adaptive;
        let mut session = Session::new();
        session.workflow = Some(SessionWorkflow::new(config));
        let storage = crate::session_store::PersistedSession::create(
            dir.path(),
            &dir.path().join("memory.sqlite"),
            "project",
            crate::conversation_state::HistoryMode::Legacy,
            session.workflow.as_ref(),
        )
        .unwrap();
        session.persistence = Some(storage.clone());
        session.push_user("complete the task");
        let provider = Adaptive {
            requests: Mutex::new(Vec::new()),
            storage,
        };
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let result = run(
            &provider,
            &mut session,
            dummy_audit(&dir),
            &mut StopTracker::new(5),
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            1,
            1,
            None,
            &mut ctx,
        )
        .await
        .unwrap();
        assert_eq!(result.text, "done");
        assert_eq!(provider.requests.lock().unwrap().len(), 4);
        assert_eq!(
            session.workflow.as_ref().unwrap().state().phase,
            Phase::Deliver
        );
        assert_eq!(session.messages.len(), 8);
        crate::budget::validate_state_schema().unwrap();
    }

    #[tokio::test]
    async fn adaptive_workflow_rejects_stale_duplicate_and_failed_sibling_transitions() {
        use crate::workflow::{Phase, SessionWorkflow, WorkflowMode};
        let change = |id: &str, revision: u64| ToolCall {
            id: id.into(),
            name: "state".into(),
            arguments: serde_json::json!({"op":"transition","phase":"verify","expected_revision":revision}),
        };
        let cases = vec![
            vec![change("stale", 99)],
            vec![change("first", 0), change("second", 0)],
            vec![
                change("change", 0),
                ToolCall {
                    id: "bad".into(),
                    name: "unknown".into(),
                    arguments: serde_json::json!({}),
                },
            ],
            vec![ToolCall {
                id: "invalid-phase".into(),
                name: "state".into(),
                arguments: serde_json::json!({"op":"transition","phase":"unknown","expected_revision":0}),
            }],
            vec![ToolCall {
                id: "grant".into(),
                name: "state".into(),
                arguments: serde_json::json!({"op":"transition","phase":"deliver","expected_revision":0,"spec_approval":true}),
            }],
        ];
        for calls in cases {
            let dir = tempfile::tempdir().unwrap();
            let mut config = crate::config::Config::default().workflow;
            config.mode = WorkflowMode::Adaptive;
            let mut session = Session::new();
            session.workflow = Some(SessionWorkflow::new(config));
            session.push_user("test");
            let provider = Scripted {
                replies: Mutex::new(vec![
                    CompletionResponse {
                        tool_calls: calls,
                        ..Default::default()
                    },
                    CompletionResponse {
                        text: "done".into(),
                        ..Default::default()
                    },
                ]),
            };
            let always_on = crate::prompt::assemble_always_on("", "", &[]);
            let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
            let mut ctx = ToolContext {
                sandbox: &sandbox,
                helper: &helper,
                gate: &mut gate,
                approver: &mut approver,
            };
            run(
                &provider,
                &mut session,
                dummy_audit(&dir),
                &mut StopTracker::new(4),
                &always_on,
                &[],
                &[],
                unused_provider_pool(),
                1,
                1,
                None,
                &mut ctx,
            )
            .await
            .unwrap();
            assert_eq!(
                session.workflow.as_ref().unwrap().state().phase,
                Phase::General
            );
            assert_eq!(session.workflow.as_ref().unwrap().state().revision, 0);
            let results: Vec<_> = session
                .messages
                .iter()
                .filter(|m| m.role == polaris_provider::Role::Tool)
                .collect();
            assert!(!results.is_empty());
            assert!(
                results
                    .iter()
                    .all(|m| !m.content.contains("\"status\":\"applied\""))
            );
            crate::session::desktop_request_history(&session.messages).unwrap();
        }
    }

    /// M6-1: the window is offered to a desktop run even with the workflow
    /// off, and the phase operations stay unavailable there.
    #[tokio::test]
    async fn the_state_window_is_available_without_the_workflow_but_phase_operations_are_not() {
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new();
        // The owner is dropped: a run whose owner is gone proceeds without
        // instructions instead of waiting for an answer that cannot come.
        let (port, _) = crate::desktop_workflow::DesktopWorkflow::pair(
            polaris_desktop_protocol::request::RunTarget {
                run_id: polaris_desktop_protocol::ids::RunId::new("r").unwrap(),
                attempt_id: polaris_desktop_protocol::ids::AttemptId::new("a").unwrap(),
            },
        );
        session.desktop_workflow = Some(port);
        session.push_user("test");
        let provider = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    tool_calls: vec![ToolCall {
                        id: "focus".into(),
                        name: "state".into(),
                        arguments: serde_json::json!({"op":"get"}),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    text: "done".into(),
                    ..Default::default()
                },
            ]),
        };
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        run(
            &provider,
            &mut session,
            dummy_audit(&dir),
            &mut StopTracker::new(4),
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            1,
            1,
            None,
            &mut ctx,
        )
        .await
        .unwrap();
        let result = session
            .messages
            .iter()
            .find(|m| m.role == polaris_provider::Role::Tool)
            .expect("the state tool was offered and dispatched");
        assert!(result.content.contains("unavailable"), "{}", result.content);
    }

    /// The next event that is not a model request boundary.
    fn next_event_now(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::events::AgentEvent>,
    ) -> Option<crate::events::AgentEvent> {
        loop {
            match rx.try_recv().ok()? {
                crate::events::AgentEvent::ModelRequestStarted { .. }
                | crate::events::AgentEvent::ModelRequestFinished { .. } => continue,
                other => return Some(other),
            }
        }
    }

    /// Nothing but the model request events arrived.
    fn only_model_events(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::events::AgentEvent>,
    ) -> bool {
        loop {
            match rx.try_recv() {
                Ok(
                    crate::events::AgentEvent::ModelRequestStarted { .. }
                    | crate::events::AgentEvent::ModelRequestFinished { .. },
                ) => continue,
                Ok(_) => return false,
                Err(_) => return true,
            }
        }
    }

    /// Model request start/end events surround every turn; a test that is
    /// about tools skips them instead of asserting on their position.
    async fn next_event(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<crate::events::AgentEvent>,
    ) -> Option<crate::events::AgentEvent> {
        loop {
            match rx.recv().await? {
                crate::events::AgentEvent::ModelRequestStarted { .. }
                | crate::events::AgentEvent::ModelRequestFinished { .. } => continue,
                other => return Some(other),
            }
        }
    }

    /// A provider that calls a tool the 1st time and returns body text the 2nd time.
    struct Scripted {
        replies: Mutex<Vec<CompletionResponse>>,
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agents_refresh_same_turn_keeps_workflow_and_raw_history() {
        struct Changing {
            path: PathBuf,
            requests: Mutex<Vec<CompletionRequest>>,
            change: bool,
            invalid: bool,
        }
        #[async_trait::async_trait]
        impl Provider for Changing {
            async fn complete(
                &self,
                req: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                let mut requests = self.requests.lock().unwrap();
                requests.push(req);
                if requests.len() == 1 {
                    if self.invalid {
                        std::fs::write(&self.path, [0xff]).unwrap();
                    } else {
                        let rule = if self.change {
                            "New rule."
                        } else {
                            "Old rule."
                        };
                        std::fs::write(
                            &self.path,
                            format!("## Always on\n{rule}\n## Other\nchanged outside section"),
                        )
                        .unwrap();
                    }
                    return Ok(CompletionResponse {
                        tool_calls: vec![polaris_provider::ToolCall {
                            id: "catalog".into(),
                            name: "skill".into(),
                            arguments: serde_json::json!({"q":""}),
                        }],
                        ..Default::default()
                    });
                }
                assert_eq!(
                    requests.last().unwrap().messages.last().unwrap().content,
                    polaris_tools::skill::lookup(
                        &polaris_skills::workflow_profile::recommended_bindings(),
                        "",
                    )
                );
                Ok(CompletionResponse {
                    text: "done".into(),
                    ..Default::default()
                })
            }
        }
        for (change, invalid) in [(true, false), (false, false), (false, true)] {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path().canonicalize().unwrap();
            let path = root.join("AGENTS.md");
            std::fs::write(&path, "## Always on\nOld rule.").unwrap();
            let prompt = crate::prompt::assemble_always_on("", "fixed environment", &[])
                .with_agents_refresh(crate::constitution::AgentsRefresh::new(None, &root).unwrap())
                .unwrap();
            let provider = Changing {
                path,
                requests: Mutex::new(vec![]),
                change,
                invalid,
            };
            let mut session = Session::new();
            session.push_user("Do the task");
            session.workflow = Some(crate::workflow::SessionWorkflow::new(
                crate::workflow::WorkflowConfig {
                    enabled: true,
                    always: vec!["builtin:workflow-core".into()],
                    ..Default::default()
                },
            ));
            let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
            let mut ctx = ToolContext {
                sandbox: &sandbox,
                helper: &helper,
                gate: &mut gate,
                approver: &mut approver,
            };
            let result = run(
                &provider,
                &mut session,
                dummy_audit(&dir),
                &mut StopTracker::new(4),
                &prompt,
                &[],
                &[],
                unused_provider_pool(),
                1,
                1,
                None,
                &mut ctx,
            )
            .await;
            assert_eq!(result.is_ok(), !invalid);
            let requests = provider.requests.lock().unwrap();
            assert_eq!(requests.len(), if invalid { 1 } else { 2 });
            assert!(requests[0].system.contains("Old rule."));
            if !invalid {
                assert_eq!(requests[0].system == requests[1].system, !change);
                assert_eq!(requests[1].system.contains("New rule."), change);
                let workflow = |s: &str| {
                    s.split("## Required workflow instructions")
                        .nth(1)
                        .unwrap()
                        .to_owned()
                };
                assert_eq!(workflow(&requests[0].system), workflow(&requests[1].system));
                assert_eq!(
                    polaris_provider::cache_key("m", None, &requests[0])
                        == polaris_provider::cache_key("m", None, &requests[1]),
                    !change
                );
            }
            assert!(
                session
                    .messages
                    .iter()
                    .all(|m| !m.content.contains("Old rule.") && !m.content.contains("New rule."))
            );
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn agents_refresh_initial_read_failure_makes_zero_provider_calls() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let path = root.join("AGENTS.md");
        std::fs::write(&path, "## Always on\nOriginal.").unwrap();
        let prompt = crate::prompt::assemble_always_on("", "", &[])
            .with_agents_refresh(crate::constitution::AgentsRefresh::new(None, &root).unwrap())
            .unwrap();
        std::fs::write(&path, [0xff]).unwrap();
        let mut session = Session::new();
        session.push_user("hello");
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let result = run(
            &NeverCalled,
            &mut session,
            dummy_audit(&dir),
            &mut StopTracker::new(2),
            &prompt,
            &[],
            &[],
            unused_provider_pool(),
            1,
            1,
            None,
            &mut ctx,
        )
        .await;
        assert!(matches!(result, Err(AgentError::Io(_))));
        assert_eq!(session.messages.len(), 1);
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

    #[tokio::test]
    async fn automatic_skill_catalog_is_metadata_only_and_stable_across_tool_continuation() {
        struct Capture {
            requests: Mutex<Vec<CompletionRequest>>,
            replies: Mutex<Vec<CompletionResponse>>,
        }
        #[async_trait::async_trait]
        impl Provider for Capture {
            async fn complete(
                &self,
                request: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                self.requests.lock().unwrap().push(request);
                Ok(self.replies.lock().unwrap().remove(0))
            }
        }

        let provider = Capture {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new(vec![
                CompletionResponse {
                    tool_calls: vec![ToolCall {
                        id: "read-catalog-entry".into(),
                        name: "skill".into(),
                        arguments: serde_json::json!({"q":"deploy-guide"}),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    text: "done".into(),
                    ..Default::default()
                },
            ]),
        };
        let skill = polaris_skills::Skill {
            name: "deploy-guide".into(),
            description: "Deploys the service safely.".into(),
            body: "BODY MUST ONLY ARRIVE THROUGH THE SKILL TOOL".into(),
            path: "/fixture/deploy-guide/SKILL.md".into(),
        };
        let mut session = Session::new();
        session.push_user("deploy the service");
        session.workflow = Some(crate::workflow::SessionWorkflow::new(
            crate::workflow::WorkflowConfig {
                enabled: true,
                always: vec!["builtin:workflow-core".into()],
                ..Default::default()
            },
        ));
        let dir = tempfile::tempdir().unwrap();
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        run(
            &provider,
            &mut session,
            dummy_audit(&dir),
            &mut StopTracker::new(3),
            &always_on,
            &[skill],
            &[],
            unused_provider_pool(),
            1,
            1,
            None,
            &mut ctx,
        )
        .await
        .unwrap();

        let requests = provider.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        for request in requests.iter() {
            assert!(request.system.contains("## Optional skill catalog"));
            assert!(request.system.contains("deploy-guide"));
            assert!(request.system.contains("Deploys the service safely."));
            assert!(
                !request
                    .system
                    .contains("BODY MUST ONLY ARRIVE THROUGH THE SKILL TOOL")
            );
            assert_eq!(
                request.system.matches("## Optional skill catalog").count(),
                1
            );
        }
        assert!(requests[1].messages.iter().any(|message| {
            message
                .content
                .contains("BODY MUST ONLY ARRIVE THROUGH THE SKILL TOOL")
        }));
    }

    /// Builds a project whose map documents three files, and a judge that
    /// adopts only the one named in `wanted`.
    fn filemap_fixture(wanted: &'static str) -> (tempfile::TempDir, FilemapJudge) {
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::create_dir_all(dir.path().join("docs")).expect("docs");
        std::fs::write(
            dir.path().join("docs/filemap.md"),
            "# File map\n\n## `crates/polaris-core/src`\n\n\
             - `agent.rs` — The loop that runs one turn.\n\
             - `budget.rs` — Token ceilings for the always-on context.\n\
             - `session.rs` — Request history.\n",
        )
        .expect("map");
        (dir, FilemapJudge { wanted })
    }

    struct FilemapJudge {
        wanted: &'static str,
    }

    #[async_trait::async_trait]
    impl polaris_provider::decision::Decider for FilemapJudge {
        async fn decide(
            &self,
            _state: &serde_json::Value,
            questions: &std::collections::BTreeMap<String, polaris_provider::decision::Question>,
        ) -> Result<polaris_provider::decision::Decision, polaris_provider::decision::DecisionError>
        {
            let answers = questions
                .iter()
                .map(|(key, question)| {
                    let asked = match question {
                        polaris_provider::decision::Question::Noul { instructions } => {
                            instructions.clone()
                        }
                        _ => String::new(),
                    };
                    (
                        key.clone(),
                        polaris_provider::decision::Answer {
                            noul: Some(if asked.contains(self.wanted) {
                                1.0
                            } else {
                                0.0
                            }),
                            ..Default::default()
                        },
                    )
                })
                .collect();
            Ok(polaris_provider::decision::Decision {
                answers,
                input_tokens: None,
                output_tokens: None,
            })
        }
    }

    /// The system prompt of every request the run sent. `CompletionRequest`
    /// is not `Clone`, and the only part these tests judge is the prompt.
    async fn filemap_run_system(
        root: &std::path::Path,
        judge: Option<FilemapJudge>,
    ) -> Vec<String> {
        struct Capture {
            requests: Mutex<Vec<CompletionRequest>>,
        }
        #[async_trait::async_trait]
        impl Provider for Capture {
            async fn complete(
                &self,
                request: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                self.requests.lock().unwrap().push(request);
                Ok(CompletionResponse {
                    text: "done".into(),
                    ..Default::default()
                })
            }
        }
        let provider = Capture {
            requests: Mutex::new(Vec::new()),
        };
        let mut session = Session::new();
        session.push_user("常時コンテキストの上限はどこで決まるか");
        session.decider = judge.map(|judge| {
            std::sync::Arc::new(judge) as std::sync::Arc<dyn polaris_provider::decision::Decider>
        });
        let audit_dir = tempfile::tempdir().unwrap();
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        // The route takes the project root from the writable roots, and
        // full-access refuses to name one.
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            std::slice::from_ref(&root.to_path_buf()),
        )
        .expect("policy");
        let (_, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        run(
            &provider,
            &mut session,
            dummy_audit(&audit_dir),
            &mut StopTracker::new(3),
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            1,
            1,
            None,
            &mut ctx,
        )
        .await
        .unwrap();
        let requests = provider.requests.lock().unwrap();
        requests
            .iter()
            .map(|request| request.system.clone())
            .collect()
    }

    /// The point of the route: the model is handed the file, not the map.
    /// The section must name what the judge adopted and nothing it dropped
    /// — a section that listed every entry would cost more than the read it
    /// replaces.
    #[tokio::test]
    async fn the_routed_files_reach_the_request_and_the_dropped_ones_do_not() {
        let (dir, judge) = filemap_fixture("budget.rs");
        let systems = filemap_run_system(dir.path(), Some(judge)).await;
        let system = &systems[0];
        assert!(
            system.contains(crate::filemap::HEADING),
            "the section is missing: {system}"
        );
        assert!(system.contains("crates/polaris-core/src/budget.rs"));
        assert!(
            !system.contains("crates/polaris-core/src/session.rs"),
            "an entry the judge dropped was sent anyway"
        );
        // The whole map must not be what was sent.
        assert!(!system.contains("The loop that runs one turn."));
    }

    /// Without a judge there is no route, and the run must look exactly as
    /// it did before this existed. A section built from some fallback
    /// ordering would put unjudged paths in front of the model on every
    /// default run, where the judge is off.
    #[tokio::test]
    async fn no_judge_means_no_section_at_all() {
        let (dir, _) = filemap_fixture("budget.rs");
        let systems = filemap_run_system(dir.path(), None).await;
        assert!(
            !systems[0].contains(crate::filemap::HEADING),
            "a section was injected without a judge: {}",
            systems[0]
        );
    }

    /// A judge that adopts nothing is not a judge that failed. An empty
    /// heading spends tokens on every iteration to say nothing.
    #[tokio::test]
    async fn nothing_adopted_leaves_no_empty_heading() {
        let (dir, judge) = filemap_fixture("a-file-the-map-does-not-have");
        let systems = filemap_run_system(dir.path(), Some(judge)).await;
        assert!(!systems[0].contains(crate::filemap::HEADING));
    }

    /// The map is read the way `environment_block` reads it: a link is not
    /// followed. Otherwise a repository could point `docs/filemap.md` at a
    /// file outside itself and have its contents summarised into the
    /// prompt.
    #[tokio::test]
    async fn a_map_that_is_a_symlink_is_not_followed() {
        // The judge adopts what the *target* documents. Were it looking for
        // something only the real map holds, this would pass whether or not
        // the link was followed.
        let (dir, judge) = filemap_fixture("key.pem");
        let outside = tempfile::tempdir().expect("outside");
        let target = outside.path().join("filemap.md");
        std::fs::write(
            &target,
            "## `secrets`\n\n- `key.pem` — the private key nobody asked for\n",
        )
        .expect("target");
        let map = dir.path().join("docs/filemap.md");
        std::fs::remove_file(&map).expect("remove");
        std::os::unix::fs::symlink(&target, &map).expect("symlink");

        let systems = filemap_run_system(dir.path(), Some(judge)).await;
        assert!(!systems[0].contains(crate::filemap::HEADING));
        assert!(!systems[0].contains("key.pem"));
    }

    #[tokio::test]
    async fn root_mcp_setup_lists_candidates_without_values_outside_mcp_dispatch() {
        struct Capture {
            requests: Mutex<Vec<CompletionRequest>>,
            replies: Mutex<Vec<CompletionResponse>>,
        }
        #[async_trait::async_trait]
        impl Provider for Capture {
            async fn complete(
                &self,
                request: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                self.requests.lock().unwrap().push(request);
                Ok(self.replies.lock().unwrap().remove(0))
            }
        }
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().canonicalize().unwrap();
        std::fs::create_dir_all(home.join(".codex")).unwrap();
        std::fs::write(
            home.join(".codex/config.toml"),
            "[mcp_servers.docs]\nurl = \"https://docs.example.com/mcp\"\nhttp_headers = { authorization = \"Bearer synthetic-token\" }\n",
        )
        .unwrap();
        let target = polaris_desktop_protocol::request::RunTarget {
            run_id: polaris_desktop_protocol::ids::RunId::new("setup-run").unwrap(),
            attempt_id: polaris_desktop_protocol::ids::AttemptId::new("attempt").unwrap(),
        };
        let mut owner = crate::execution_owner::RunExecution::new(
            &target,
            Arc::new(std::sync::atomic::AtomicBool::new(false)),
        );
        let port = owner
            .configure_registration(&target, crate::mcp::operation::ExternalCeiling::Denied)
            .unwrap();
        let provider = Capture {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new(vec![
                CompletionResponse {
                    tool_calls: vec![ToolCall {
                        id: "setup".into(),
                        name: "mcp_setup".into(),
                        arguments: serde_json::json!({"op":"detect"}),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    text: "done".into(),
                    ..Default::default()
                },
            ]),
        };
        let mut session = Session::new();
        session.push_user("use my docs server");
        session.mcp_setup = Some(Arc::new(crate::mcp::setup::SetupTool::new(
            home.clone(),
            vec![],
            vec![],
            crate::isolated_workspace::Limits {
                max_entries: 8,
                max_files: 4,
                max_file_bytes: 1024,
                max_total_bytes: 4096,
                max_depth: 2,
            },
            "project".into(),
            crate::mcp::setup::Authorization::Owner(port),
        )));
        let dir = tempfile::tempdir().unwrap();
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        run(
            &provider,
            &mut session,
            dummy_audit(&dir),
            &mut StopTracker::new(3),
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            1,
            1,
            None,
            &mut ctx,
        )
        .await
        .unwrap();
        let requests = provider.requests.lock().unwrap();
        assert!(
            requests[0]
                .tools
                .iter()
                .any(|tool| tool.name == "mcp_setup")
        );
        let result = requests[1]
            .messages
            .iter()
            .find(|message| message.tool_call_id.as_deref() == Some("setup"))
            .unwrap();
        assert!(result.content.contains(r#""name":"docs""#));
        assert!(!result.content.contains("synthetic-token"));
        assert!(!result.content.contains("docs.example.com"));
    }

    #[tokio::test]
    async fn the_setting_tool_is_offered_to_the_root_and_answers_without_a_shell() {
        struct Capture {
            requests: Mutex<Vec<CompletionRequest>>,
            replies: Mutex<Vec<CompletionResponse>>,
        }
        #[async_trait::async_trait]
        impl Provider for Capture {
            async fn complete(
                &self,
                request: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                self.requests.lock().unwrap().push(request);
                Ok(self.replies.lock().unwrap().remove(0))
            }
        }
        let project = tempfile::tempdir().unwrap();
        let root = project.path().canonicalize().unwrap();
        std::fs::write(root.join("package.json"), r#"{"name":"demo"}"#).unwrap();
        let provider = Capture {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new(vec![
                CompletionResponse {
                    tool_calls: vec![ToolCall {
                        id: "setting".into(),
                        name: "setting".into(),
                        arguments: serde_json::json!({"op":"plan"}),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    text: "done".into(),
                    ..Default::default()
                },
            ]),
        };
        let mut session = Session::new();
        session.push_user("set this project up");
        session.setting = Some(Arc::new(crate::recipe::tool::SettingTool::new(
            root.clone(),
        )));
        let dir = tempfile::tempdir().unwrap();
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        run(
            &provider,
            &mut session,
            dummy_audit(&dir),
            &mut StopTracker::new(3),
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            1,
            1,
            None,
            &mut ctx,
        )
        .await
        .unwrap();
        let requests = provider.requests.lock().unwrap();
        let offered = requests[0]
            .tools
            .iter()
            .find(|tool| tool.name == "setting")
            .expect("the root is offered the setting tool");
        // The model names an operation. There is nowhere to put a command,
        // a path or a package name.
        let properties = &offered.parameters["properties"];
        assert_eq!(
            properties
                .as_object()
                .expect("properties")
                .keys()
                .collect::<Vec<_>>(),
            vec!["op"]
        );
        assert_eq!(offered.parameters["additionalProperties"], false);

        let result = requests[1]
            .messages
            .iter()
            .find(|message| message.tool_call_id.as_deref() == Some("setting"))
            .expect("the answer");
        let answer: serde_json::Value =
            serde_json::from_str(&result.content).expect("the tool answers with JSON");
        assert_eq!(answer["nextjs"]["applicable"], true);
        assert_eq!(answer["nextjs"]["install_requires_person"], true);
        // Planning changed nothing in the project.
        assert!(!root.join("node_modules").exists());
        assert!(!root.join("next.config.mjs").exists());
    }

    #[tokio::test]
    async fn a_child_run_is_never_offered_the_setting_tool() {
        struct Once(Mutex<Vec<CompletionRequest>>);
        #[async_trait::async_trait]
        impl Provider for Once {
            async fn complete(
                &self,
                request: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                self.0.lock().unwrap().push(request);
                Ok(CompletionResponse {
                    text: "done".into(),
                    ..Default::default()
                })
            }
        }
        let project = tempfile::tempdir().unwrap();
        let root = project.path().canonicalize().unwrap();
        let provider = Once(Mutex::new(Vec::new()));
        let mut session = Session::new();
        session.push_user("do something");
        session.setting = Some(Arc::new(crate::recipe::tool::SettingTool::new(root)));
        let dir = tempfile::tempdir().unwrap();
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        run_loop(
            &provider,
            &mut session,
            dummy_audit(&dir),
            &mut StopTracker::new(3),
            "BASE_SYSTEM",
            &polaris_tools::all_specs(),
            &[],
            &[],
            unused_provider_pool(),
            1,
            1,
            "child",
            "",
            false,
            None,
            &mut ctx,
            None,
        )
        .await
        .unwrap();
        assert!(
            !provider.0.lock().unwrap()[0]
                .tools
                .iter()
                .any(|tool| tool.name == "setting"),
            "a child run must not be able to change the project's configuration"
        );
    }

    #[tokio::test]
    async fn local_mcp_calls_are_approved_in_the_loop_and_answered_with_the_canonical_result() {
        use polaris_mcp::catalog::{ItemId, Kind};
        struct Capture {
            requests: Mutex<Vec<CompletionRequest>>,
            replies: Mutex<Vec<CompletionResponse>>,
        }
        #[async_trait::async_trait]
        impl Provider for Capture {
            async fn complete(
                &self,
                request: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                self.requests.lock().unwrap().push(request);
                Ok(self.replies.lock().unwrap().remove(0))
            }
        }
        let (server, methods, fixture) =
            crate::mcp::runtime::test_fixture::server_fixture("").await;
        let config = crate::mcp::runtime::RuntimeConfig::new(
            polaris_mcp::registration::Registry {
                schema_version: 1,
                revision: 1,
                servers: vec![server],
            },
            "project".into(),
            Arc::new(polaris_mcp::store::Secrets::new(1, Default::default()).unwrap()),
            Default::default(),
        )
        .unwrap();
        let mut runtime = crate::mcp::runtime::RunRuntime::new(
            config,
            crate::execution_owner::McpPort::local(
                "local-session",
                "run-1",
                "1",
                crate::mcp::operation::ExternalCeiling::Allowed,
            ),
        )
        .unwrap();
        runtime
            .ensure_catalog("fixture", std::future::pending())
            .await
            .unwrap();
        let item = ItemId {
            server_id: "fixture".into(),
            kind: Kind::Tool,
            key: "fixture".into(),
        };
        let found = runtime
            .candidates("fixture", Some("fixture"), "implement")
            .unwrap();
        runtime.select(&item, &found[0].generation).unwrap();
        let alias = polaris_mcp::catalog::tool_alias(&item).unwrap();
        let provider = Capture {
            requests: Mutex::new(Vec::new()),
            replies: Mutex::new(vec![
                CompletionResponse {
                    tool_calls: vec![ToolCall {
                        id: "mcp-call".into(),
                        name: alias.clone(),
                        arguments: serde_json::json!({}),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    text: "done".into(),
                    ..Default::default()
                },
            ]),
        };
        let mut session = Session::new();
        session.push_user("use the fixture tool");
        let dir = tempfile::tempdir().unwrap();
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, _, mut approver) = dummy_tool_parts();
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Always);
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        run_with_mcp(
            &provider,
            &mut session,
            dummy_audit(&dir),
            &mut StopTracker::new(3),
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            1,
            1,
            None,
            &mut ctx,
            Some(&mut runtime),
        )
        .await
        .unwrap();
        {
            let requests = provider.requests.lock().unwrap();
            assert!(requests[0].tools.iter().any(|tool| tool.name == alias));
            let result = requests[1]
                .messages
                .iter()
                .find(|message| message.tool_call_id.as_deref() == Some("mcp-call"))
                .unwrap();
            let envelope: serde_json::Value = serde_json::from_str(&result.content).unwrap();
            assert_eq!(envelope["disposition"], "completed");
            assert_eq!(
                methods
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|m| *m == "tools/call")
                    .count(),
                1
            );
        }
        let repeated = ToolCall {
            id: "repeated".into(),
            name: alias.clone(),
            arguments: serde_json::json!({}),
        };
        *provider.replies.lock().unwrap() = vec![CompletionResponse {
            tool_calls: vec![repeated.clone(), repeated],
            ..Default::default()
        }];
        let mut session = Session::new();
        session.push_user("call it twice");
        let result = run_with_mcp(
            &provider,
            &mut session,
            dummy_audit(&dir),
            &mut StopTracker::new(3),
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            1,
            1,
            None,
            &mut ctx,
            Some(&mut runtime),
        )
        .await;
        assert!(matches!(
            result,
            Err(AgentError::Mcp(crate::mcp::runtime::Error::Owner(
                crate::execution_owner::McpCallError::Invalid
            )))
        ));
        assert_eq!(approver.asked, 1, "a repeated call id stops before asking");
        runtime.shutdown().await.unwrap();
        fixture.abort();
    }

    #[tokio::test]
    async fn automatic_skill_catalog_does_not_require_workflow() {
        struct Capture(Mutex<Option<CompletionRequest>>);
        #[async_trait::async_trait]
        impl Provider for Capture {
            async fn complete(
                &self,
                request: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                *self.0.lock().unwrap() = Some(request);
                Ok(CompletionResponse {
                    text: "done".into(),
                    ..Default::default()
                })
            }
        }
        let provider = Capture(Mutex::new(None));
        let skill = polaris_skills::Skill {
            name: "deploy-guide".into(),
            description: "Deploys the service safely.".into(),
            body: "not automatic".into(),
            path: "/fixture/deploy-guide/SKILL.md".into(),
        };
        let mut session = Session::new();
        session.push_user("deploy the service");
        let dir = tempfile::tempdir().unwrap();
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        run(
            &provider,
            &mut session,
            dummy_audit(&dir),
            &mut StopTracker::new(2),
            &always_on,
            &[skill],
            &[],
            unused_provider_pool(),
            1,
            1,
            None,
            &mut ctx,
        )
        .await
        .unwrap();
        let request = provider.0.lock().unwrap().take().unwrap();
        assert!(request.system.contains("## Optional skill catalog"));
        assert!(request.system.contains("deploy-guide"));
        assert!(!request.system.contains("not automatic"));
    }

    /// `spawn` is paid for on every request. With no type to resolve
    /// against, every call it invites ends before a provider call and two
    /// of the three branches stop the run outright, so advertising it into
    /// an empty catalogue buys nothing and can only cost.
    #[tokio::test]
    async fn spawn_is_not_offered_when_there_is_no_agent_type_to_run() {
        struct Capture(Mutex<Option<CompletionRequest>>);
        #[async_trait::async_trait]
        impl Provider for Capture {
            async fn complete(
                &self,
                request: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                *self.0.lock().unwrap() = Some(request);
                Ok(CompletionResponse {
                    text: "done".into(),
                    ..Default::default()
                })
            }
        }

        async fn offered_tools(agent_types: &[polaris_skills::AgentType]) -> Vec<String> {
            let provider = Capture(Mutex::new(None));
            let mut session = Session::new();
            session.push_user("do the thing");
            let dir = tempfile::tempdir().unwrap();
            let always_on = crate::prompt::assemble_always_on("", "", &[]);
            let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
            let mut ctx = ToolContext {
                sandbox: &sandbox,
                helper: &helper,
                gate: &mut gate,
                approver: &mut approver,
            };
            run(
                &provider,
                &mut session,
                dummy_audit(&dir),
                &mut StopTracker::new(2),
                &always_on,
                &[],
                agent_types,
                unused_provider_pool(),
                1,
                1,
                None,
                &mut ctx,
            )
            .await
            .unwrap();
            let request = provider.0.lock().unwrap().take().unwrap();
            request.tools.iter().map(|t| t.name.clone()).collect()
        }

        let without = offered_tools(&[]).await;
        assert!(
            !without.iter().any(|n| n == "spawn"),
            "spawn was advertised into an empty catalogue: {without:?}"
        );
        // The rest of the baseline is untouched — this withdraws one tool,
        // it does not shrink the tool list generally.
        assert!(without.iter().any(|n| n == "read"), "{without:?}");

        let shipped = polaris_skills::builtin::agent_types();
        assert!(!shipped.is_empty(), "nothing ships with the binary");
        let with = offered_tools(&shipped).await;
        assert!(
            with.iter().any(|n| n == "spawn"),
            "spawn vanished even with types available: {with:?}"
        );
        assert_eq!(with.len(), without.len() + 1, "{with:?} vs {without:?}");
    }

    #[tokio::test]
    async fn empty_catalog_keeps_the_base_system_and_is_audited() {
        struct Capture(Mutex<Option<CompletionRequest>>);
        #[async_trait::async_trait]
        impl Provider for Capture {
            async fn complete(
                &self,
                request: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                *self.0.lock().unwrap() = Some(request);
                Ok(CompletionResponse {
                    text: "done".into(),
                    ..Default::default()
                })
            }
        }
        let provider = Capture(Mutex::new(None));
        let skill = polaris_skills::Skill {
            name: "deploy-guide".into(),
            description: "Deploys the service safely.".into(),
            body: "not automatic".into(),
            path: "/fixture/deploy-guide/SKILL.md".into(),
        };
        let dir = tempfile::tempdir().unwrap();
        let audit_path = dir.path().join("audit.jsonl");
        let audit = shared_audit(&audit_path);
        let mut session = Session::new();
        session.push_user("unrelated request");
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        run_loop(
            &provider,
            &mut session,
            audit,
            &mut StopTracker::new(2),
            "BASE_SYSTEM",
            &polaris_tools::all_specs(),
            &[skill],
            &[],
            unused_provider_pool(),
            1,
            1,
            "root",
            "unrelated request",
            false,
            None,
            &mut ctx,
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            provider.0.lock().unwrap().as_ref().unwrap().system,
            "BASE_SYSTEM"
        );
        let audit = std::fs::read_to_string(audit_path).unwrap();
        assert!(audit.contains("\"tool\":\"skill-catalog\""));
        assert!(audit.contains("count=0"));
        assert!(audit.contains("revision="));
        assert!(audit.contains("tokens=0"));
    }

    #[tokio::test]
    async fn all_injected_catalog_is_audited_without_prompt_metadata() {
        struct Capture;
        #[async_trait::async_trait]
        impl Provider for Capture {
            async fn complete(
                &self,
                request: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                assert!(!request.system.contains("## Optional skill catalog"));
                Ok(CompletionResponse {
                    text: "done".into(),
                    ..Default::default()
                })
            }
        }
        let skill = polaris_skills::Skill {
            name: "deploy-guide".into(),
            description: "Deploys the service safely.".into(),
            body: "required body".into(),
            path: "/fixture/deploy-guide/SKILL.md".into(),
        };
        let dir = tempfile::tempdir().unwrap();
        let audit_path = dir.path().join("audit.jsonl");
        let mut session = Session::new();
        session.push_user("deploy the service");
        session.workflow = Some(crate::workflow::SessionWorkflow::new(
            crate::workflow::WorkflowConfig {
                enabled: true,
                always: vec!["user:deploy-guide".into()],
                ..Default::default()
            },
        ));
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        run_loop(
            &Capture,
            &mut session,
            shared_audit(&audit_path),
            &mut StopTracker::new(2),
            "BASE_SYSTEM",
            &polaris_tools::all_specs(),
            &[skill],
            &[],
            unused_provider_pool(),
            1,
            1,
            "root",
            "deploy the service",
            true,
            None,
            &mut ctx,
            None,
        )
        .await
        .unwrap();
        let audit = std::fs::read_to_string(audit_path).unwrap();
        assert!(audit.contains("\"tool\":\"skill-catalog\""));
        assert!(audit.contains("count=0"));
        assert!(audit.contains("excluded=1"));
    }

    #[test]
    fn explicit_lookup_rejects_a_visible_name_with_multiple_frozen_bodies() {
        let skills = vec![
            polaris_skills::Skill {
                name: "workflow-core".into(),
                description: "Builtin.".into(),
                body: "builtin body".into(),
                path: "builtin:workflow-core".into(),
            },
            polaris_skills::Skill {
                name: "workflow-core".into(),
                description: "User.".into(),
                body: "user body".into(),
                path: "/fixture/workflow-core/SKILL.md".into(),
            },
        ];
        let result = lookup_skill(&skills, "workflow-core");
        assert!(result.contains("複数"));
        assert!(!result.contains("builtin body"));
        assert!(!result.contains("user body"));
    }

    struct TurnOnlyProvider {
        turn_calls: std::sync::atomic::AtomicUsize,
    }

    #[tokio::test]
    async fn desktop_stream_is_provisional_and_final_response_is_recorded_once() {
        struct Streaming;
        #[async_trait::async_trait]
        impl polaris_provider::Provider for Streaming {
            async fn complete(
                &self,
                _: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                panic!("desktop must pass the response observer");
            }
            async fn complete_in_turn_with_observer(
                &self,
                _: CompletionRequest,
                _: &polaris_provider::turn_affinity::TurnContext,
                observer: Option<&dyn polaris_provider::ResponseObserver>,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                let observer = observer.expect("desktop observer");
                observer
                    .try_emit(polaris_provider::ResponseEvent::TextDelta {
                        output_index: 0,
                        content_index: 0,
                        text: "provisional",
                    })
                    .unwrap();
                observer
                    .try_emit(polaris_provider::ResponseEvent::FinalText {
                        text: "authoritative",
                        delta_matches: Some(false),
                    })
                    .unwrap();
                Ok(CompletionResponse {
                    text: "authoritative".into(),
                    usage: Some(polaris_provider::Usage {
                        input_tokens: 12,
                        output_tokens: 3,
                        total_tokens: 15,
                        cached_tokens: 0,
                    }),
                    ..Default::default()
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let (sink, mut receiver, _) = crate::desktop_events::DesktopEventSink::channel(
            polaris_desktop_protocol::ids::RunId::new("stream-final").unwrap(),
        );
        let mut session = Session::new();
        session.push_user("hello");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let desktop_query = real_user_query(&session);
        let result = run_desktop(
            &Streaming,
            &mut session,
            &desktop_query,
            dummy_audit(&dir),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            1,
            1,
            sink,
            &mut ctx,
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.text, "authoritative");
        assert_eq!(result.usage.total_tokens, 15);
        assert_eq!(result.usage_report.reported_responses, 1);
        assert_eq!(session.messages.len(), 2);
        assert_eq!(session.messages[1].content, "authoritative");
        let mut envelopes = std::iter::from_fn(|| receiver.try_recv().ok())
            .map(|envelope| envelope.event)
            .filter(|event| {
                !matches!(
                    event,
                    crate::events::AgentEvent::ModelRequestStarted { .. }
                        | crate::events::AgentEvent::ModelRequestFinished { .. }
                )
            });
        assert!(matches!(envelopes.next(),
            Some(crate::events::AgentEvent::TextDelta { request_id: 1, text, .. }) if text == "provisional"));
        assert!(envelopes.next().is_none());
    }

    #[tokio::test]
    async fn desktop_observed_completion_keeps_usage_when_display_disconnects_in_same_poll() {
        struct DisconnectOnReply(Mutex<Option<crate::desktop_events::DesktopEventReceiver>>);
        #[async_trait::async_trait]
        impl polaris_provider::Provider for DisconnectOnReply {
            async fn complete(
                &self,
                _: polaris_provider::CompletionRequest,
            ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError>
            {
                self.0.lock().unwrap().take();
                Ok(completed_response())
            }
        }
        fn completed_response() -> polaris_provider::CompletionResponse {
            polaris_provider::CompletionResponse {
                text: "completed".into(),
                usage: Some(polaris_provider::Usage {
                    input_tokens: 12,
                    output_tokens: 3,
                    total_tokens: 15,
                    cached_tokens: 0,
                }),
                ..Default::default()
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let (sink, receiver, _) = crate::desktop_events::DesktopEventSink::channel(
            polaris_desktop_protocol::ids::RunId::new("complete-disconnect").unwrap(),
        );
        let provider = DisconnectOnReply(Mutex::new(Some(receiver)));
        let mut session = Session::new();
        session.push_user("hello");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let desktop_query = real_user_query(&session);
        let outcome = run_desktop(
            &provider,
            &mut session,
            &desktop_query,
            dummy_audit(&dir),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            1,
            1,
            sink,
            &mut ctx,
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome.text, "completed");
        assert_eq!(outcome.usage.input_tokens, 12);
        assert_eq!(outcome.usage.output_tokens, 3);
        assert_eq!(session.messages.last().unwrap().content, "completed");
    }

    #[tokio::test]
    async fn desktop_owned_command_reaches_run_port_after_approval() {
        let dir = tempfile::tempdir().unwrap();
        let run_id = polaris_desktop_protocol::ids::RunId::new("owned-command").unwrap();
        let (sink, _events, _) = crate::desktop_events::DesktopEventSink::channel(run_id.clone());
        let (port, mut requests) = crate::desktop_execution::ExecutionPort::channel(run_id);
        let sink = sink.with_execution(port).unwrap();
        let sandbox =
            polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[])
                .unwrap();
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AutoApprove;
        let call = polaris_provider::ToolCall {
            id: "owned".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command":"not-a-real-shell-command"}),
        };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: Path::new("/missing"),
            gate: &mut gate,
            approver: &mut approver,
        };
        let execution = dispatch(
            &call,
            &[],
            &[],
            unused_provider_pool(),
            dummy_audit(&dir),
            1,
            1,
            Some(sink.into()),
            None,
            None,
            None,
            &mut ctx,
        );
        let owner = async {
            let request = requests.recv().await.unwrap();
            assert_eq!(request.command.program, Path::new("/bin/sh"));
            assert_eq!(request.command.args, ["-c", "not-a-real-shell-command"]);
            assert!(!request.cancelled.load(std::sync::atomic::Ordering::Acquire));
            request
                .reply
                .send(Arc::new(crate::desktop_execution::ExecutionResult {
                    end: crate::desktop_execution::ControlledEnd::Exited,
                    status: Some(0),
                    stdout: "owned-result".into(),
                    stderr: String::new(),
                    stdout_truncated: false,
                    stderr_truncated: false,
                    stdout_eof: true,
                    stderr_eof: true,
                    problem: None,
                }))
                .unwrap();
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(execution, owner)
        })
        .await
        .unwrap();
        assert!(result.unwrap().contains("owned-result"));
    }

    #[tokio::test]
    async fn desktop_owned_bash_port_failures_emit_finished() {
        use crate::desktop_execution::{CAPACITY, ExecutionCommand, ExecutionPort};
        use crate::events::AgentEvent;

        struct OwnerApproves;
        impl crate::approval::Approver for OwnerApproves {
            fn ask(&mut self, _: &str) -> crate::approval::Decision {
                panic!("only the execution owner may ask");
            }
        }

        // submit拒否の2経路と、受付後の返信切断。実行workerは起動しない。
        for failure in ["closed", "full", "reply-dropped"] {
            let dir = tempfile::tempdir().unwrap();
            let run_id = polaris_desktop_protocol::ids::RunId::new(failure).unwrap();
            let (sink, mut events, control) =
                crate::desktop_events::DesktopEventSink::channel(run_id.clone());
            let (port, mut requests) = ExecutionPort::channel(run_id.clone());
            let sandbox =
                polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[])
                    .unwrap();
            let mut prefilled = Vec::new();
            if failure == "full" {
                for _ in 0..CAPACITY {
                    prefilled.push(
                        port.submit(ExecutionCommand {
                            policy: sandbox.clone(),
                            program: PathBuf::from("/never-executed-fixture"),
                            args: vec![],
                            stdin: None,
                        })
                        .unwrap(),
                    );
                }
            } else if failure == "closed" {
                requests.close();
            }
            let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Always);
            let mut approver = OwnerApproves;
            let mut ctx = ToolContext {
                sandbox: &sandbox,
                helper: Path::new("/never-executed-helper"),
                gate: &mut gate,
                approver: &mut approver,
            };
            let call = polaris_provider::ToolCall {
                id: "port-failure".into(),
                name: "bash".into(),
                arguments: serde_json::json!({"command": "printf unexpected-direct-fallback"}),
            };
            let execution = dispatch(
                &call,
                &[],
                &[],
                unused_provider_pool(),
                dummy_audit(&dir),
                1,
                1,
                Some(sink.with_execution(port).unwrap().into()),
                None,
                None,
                None,
                &mut ctx,
            );
            let owner = async {
                if failure == "reply-dropped" {
                    let request = requests.recv().await.unwrap();
                    assert_eq!(request.command.program, Path::new("/bin/sh"));
                    assert_eq!(
                        request.command.args,
                        ["-c", "printf unexpected-direct-fallback"]
                    );
                    drop(request.reply);
                }
            };
            let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                tokio::join!(execution, owner)
            })
            .await
            .unwrap();
            let expected = if failure == "reply-dropped" {
                "desktop execution owner disconnected"
            } else {
                "desktop execution owner unavailable or full"
            };
            assert_eq!(result, Err(expected.into()), "{failure}");
            assert_eq!(control.failure(), None, "event delivery remains available");
            let started = events.try_recv().expect("Started");
            assert_eq!(started.run_id, run_id);
            assert_eq!(started.sequence, 1);
            assert!(
                matches!(started.event, AgentEvent::ToolStarted { name, .. } if name == "bash")
            );
            let finished = events.try_recv().expect("port failure must emit Finished");
            assert_eq!(finished.run_id, run_id);
            assert_eq!(finished.sequence, 2);
            match finished.event {
                AgentEvent::ToolFinished {
                    name,
                    detail,
                    ok,
                    result,
                    ..
                } => {
                    assert_eq!(name, "bash");
                    assert_eq!(detail, call.arguments.to_string());
                    assert!(!ok);
                    assert_eq!(result, expected);
                }
                other => panic!("unexpected terminal event: {other:?}"),
            }
            assert!(events.try_recv().is_err(), "exactly one terminal event");
            if failure == "full" {
                for _ in 0..CAPACITY {
                    let request = requests.try_recv().unwrap();
                    assert_eq!(
                        request.command.program,
                        Path::new("/never-executed-fixture")
                    );
                }
            }
            assert!(requests.try_recv().is_err(), "no extra command submitted");
            drop(prefilled);
        }
    }

    #[tokio::test]
    async fn desktop_mutations_are_owned_and_do_not_ask_a_second_approver() {
        struct OwnerApproves;
        impl crate::approval::Approver for OwnerApproves {
            fn ask(&mut self, _: &str) -> crate::approval::Decision {
                panic!("the durable execution owner must be the only approver");
            }
        }
        for operation in ["write", "edit"] {
            let dir = tempfile::tempdir().unwrap();
            let target = dir.path().join("target.txt");
            std::fs::write(&target, "old").unwrap();
            let run_id = polaris_desktop_protocol::ids::RunId::new("owned-mutation").unwrap();
            let (sink, _events, _) =
                crate::desktop_events::DesktopEventSink::channel(run_id.clone());
            let (port, mut requests) = crate::desktop_execution::ExecutionPort::channel(run_id);
            let sandbox = polaris_sandbox::SandboxPolicy::new(
                polaris_sandbox::SandboxMode::WorkspaceWrite,
                &[dir.path().to_path_buf()],
            )
            .unwrap();
            let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Always);
            let mut approver = OwnerApproves;
            let mut ctx = ToolContext {
                sandbox: &sandbox,
                helper: Path::new("/must-not-launch"),
                gate: &mut gate,
                approver: &mut approver,
            };
            let call = polaris_provider::ToolCall {
                id: "mutation".into(),
                name: operation.into(),
                arguments: serde_json::json!({"path":target,"content":"new","old":"old","new":"new"}),
            };
            let execution = dispatch(
                &call,
                &[],
                &[],
                unused_provider_pool(),
                dummy_audit(&dir),
                1,
                1,
                Some(sink.with_execution(port).unwrap().into()),
                None,
                None,
                None,
                &mut ctx,
            );
            let owner = async {
                let request = requests.recv().await.unwrap();
                assert_eq!(request.command.program, Path::new("/must-not-launch"));
                assert_eq!(request.command.args, ["--confined-apply"]);
                let payload: serde_json::Value =
                    serde_json::from_str(request.command.stdin.as_ref().unwrap()).unwrap();
                assert!(payload.to_string().contains("new"));
                request
                    .reply
                    .send(Arc::new(
                        crate::desktop_execution::ExecutionResult::not_started(
                            "owner denied".into(),
                        ),
                    ))
                    .unwrap();
            };
            let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                tokio::join!(execution, owner)
            })
            .await
            .unwrap();
            assert!(result.unwrap_err().contains("owner denied"));
            assert_eq!(std::fs::read_to_string(&target).unwrap(), "old");
        }
    }

    #[tokio::test]
    async fn desktop_command_uses_the_async_approval_mailbox() {
        let dir = tempfile::tempdir().unwrap();
        let run_id = polaris_desktop_protocol::ids::RunId::new("async-command").unwrap();
        let (sink, _events, control) = crate::desktop_events::EventSink::desktop(run_id.clone());
        let (mut approver, mut requests) = crate::desktop_approval::DesktopApprover::channel(
            run_id,
            4,
            std::time::Duration::from_secs(2),
            control,
        );
        let sandbox =
            polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[])
                .unwrap();
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Always);
        let call = polaris_provider::ToolCall {
            id: "command".into(),
            name: "bash".into(),
            arguments: serde_json::json!({"command":"printf p4-approval"}),
        };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: Path::new("/bin/false"),
            gate: &mut gate,
            approver: &mut approver,
        };
        let execution = dispatch(
            &call,
            &[],
            &[],
            unused_provider_pool(),
            dummy_audit(&dir),
            1,
            1,
            Some(sink),
            None,
            None,
            None,
            &mut ctx,
        );
        let controller = async {
            let pending = requests.recv().await.unwrap();
            assert_eq!(pending.run_id().as_str(), "async-command");
            assert!(pending.reason().contains("p4-approval"));
            assert!(pending.resolve(4, crate::approval::Decision::Allow));
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(5), async {
            tokio::join!(execution, controller)
        })
        .await
        .unwrap();
        assert_eq!(result.unwrap(), "p4-approval");
    }

    #[tokio::test]
    async fn desktop_start_event_overflow_stops_before_approval_or_tool_execution() {
        struct MustNotAsk;
        impl crate::approval::Approver for MustNotAsk {
            fn ask(&mut self, _: &str) -> crate::approval::Decision {
                panic!("approval/tool path was reached after notification failure")
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let (sink, _receiver, control) = crate::desktop_events::EventSink::desktop(
            polaris_desktop_protocol::ids::RunId::new("full-start").unwrap(),
        );
        for _ in 0..crate::desktop_events::EVENT_CAPACITY {
            sink.send(crate::events::AgentEvent::SpawnFinished {
                agent_type: "fixture".into(),
                ok: true,
            })
            .unwrap();
        }
        assert!(control.failure().is_none());
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[dir.path().to_path_buf()],
        )
        .unwrap();
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Always);
        let target = dir.path().join("must-not-exist");
        let call = polaris_provider::ToolCall {
            id: "write".into(),
            name: "write".into(),
            arguments: serde_json::json!({"path":target,"content":"must not write"}),
        };
        let result = dispatch(
            &call,
            &[],
            &[],
            unused_provider_pool(),
            dummy_audit(&dir),
            1,
            1,
            Some(sink),
            None,
            None,
            None,
            &mut ToolContext {
                sandbox: &sandbox,
                helper: Path::new("/bin/false"),
                gate: &mut gate,
                approver: &mut MustNotAsk,
            },
        )
        .await;
        assert!(result.unwrap_err().contains("Full"));
        assert!(!target.exists());
    }

    #[tokio::test]
    async fn desktop_delivery_cancels_a_pending_provider_without_a_completed_reply() {
        struct PendingProvider(tokio::sync::Notify);
        #[async_trait::async_trait]
        impl polaris_provider::Provider for PendingProvider {
            async fn complete(
                &self,
                _: polaris_provider::CompletionRequest,
            ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError>
            {
                self.0.notify_one();
                std::future::pending().await
            }
        }
        let provider = PendingProvider(tokio::sync::Notify::new());
        let dir = tempfile::tempdir().unwrap();
        let mut session = Session::new();
        session.push_user("hello");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let (sink, receiver, _) = crate::desktop_events::DesktopEventSink::channel(
            polaris_desktop_protocol::ids::RunId::new("cancel-provider").unwrap(),
        );
        let desktop_query = real_user_query(&session);
        let run = run_desktop(
            &provider,
            &mut session,
            &desktop_query,
            dummy_audit(&dir),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            1,
            1,
            sink,
            &mut ctx,
            None,
        );
        let disconnect = async {
            provider.0.notified().await;
            drop(receiver);
        };
        let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            tokio::join!(run, disconnect)
        })
        .await
        .expect("pending provider was not cancelled");
        assert!(matches!(
            result,
            Err(AgentError::Desktop(
                crate::desktop_events::EventFailure::ReceiverDropped
            ))
        ));
        assert_eq!(
            session.messages.len(),
            1,
            "cancelled provider must not publish a completed reply"
        );
    }

    #[tokio::test]
    async fn desktop_pre_cancel_does_not_call_provider_and_normal_completion_still_works() {
        let dir = tempfile::tempdir().unwrap();
        let provider = Scripted {
            replies: Mutex::new(vec![CompletionResponse {
                text: "done".into(),
                ..Default::default()
            }]),
        };
        let mut session = Session::new();
        session.push_user("hello");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let (sink, _receiver, control) = crate::desktop_events::DesktopEventSink::channel(
            polaris_desktop_protocol::ids::RunId::new("pre-cancel").unwrap(),
        );
        control.cancel();
        let desktop_query = real_user_query(&session);
        let result = run_desktop(
            &provider,
            &mut session,
            &desktop_query,
            dummy_audit(&dir),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            1,
            1,
            sink,
            &mut ctx,
            None,
        )
        .await;
        assert!(matches!(
            result,
            Err(AgentError::Desktop(
                crate::desktop_events::EventFailure::Cancelled
            ))
        ));
        assert_eq!(provider.replies.lock().unwrap().len(), 1);
        let (sink, _receiver, _) = crate::desktop_events::DesktopEventSink::channel(
            polaris_desktop_protocol::ids::RunId::new("normal").unwrap(),
        );
        let desktop_query = real_user_query(&session);
        let result = run_desktop(
            &provider,
            &mut session,
            &desktop_query,
            dummy_audit(&dir),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            1,
            1,
            sink,
            &mut ctx,
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.text, "done");
        assert!(provider.replies.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn strict_history_keeps_ten_real_turns_and_never_archives_retrieval() {
        use crate::conversation_memory::{
            EmbeddingModel, StrictEmbedder, StrictHistory, StrictSummaryProvider, SummaryRequest,
        };
        use std::{future::Future, pin::Pin};
        type Reply<'a, T> = Pin<Box<dyn Future<Output = std::io::Result<T>> + Send + 'a>>;
        struct Summary;
        impl StrictSummaryProvider for Summary {
            fn summarize<'a>(&'a self, request: SummaryRequest) -> Reply<'a, String> {
                Box::pin(async move {
                    Ok(serde_json::json!({"facts":[format!("marker-{}", request.source_turn_id)],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[request.source_turn_id]}).to_string())
                })
            }
        }
        struct Embedding(EmbeddingModel);
        impl StrictEmbedder for Embedding {
            fn metadata(&self) -> &EmbeddingModel {
                &self.0
            }
            fn embed_passage<'a>(&'a self, _: &'a str) -> Reply<'a, Vec<f32>> {
                Box::pin(async { Ok(vec![1., 0., 0.]) })
            }
            fn embed_query<'a>(&'a self, _: &'a str) -> Reply<'a, Vec<Vec<f32>>> {
                Box::pin(async { Ok(vec![vec![1., 0., 0.]]) })
            }
        }
        struct Capture(Mutex<Vec<CompletionRequest>>);
        #[async_trait::async_trait]
        impl Provider for Capture {
            async fn complete(
                &self,
                request: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                self.0.lock().unwrap().push(request);
                Ok(CompletionResponse {
                    text: "recorded".into(),
                    ..Default::default()
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let saved = crate::session_store::PersistedSession::create(
            dir.path(),
            &dir.path().join("memory.sqlite3"),
            "fixture",
            crate::conversation_state::HistoryMode::Strict10,
            None,
        )
        .unwrap();
        let mut session = Session::new();
        session.attach(saved.clone()).unwrap();
        session.strict_history = Some(Arc::new(StrictHistory::new(
            Arc::new(Summary),
            Arc::new(Embedding(EmbeddingModel {
                model: "fixture".into(),
                revision: "1".into(),
                dimension: 3,
            })),
        )));
        // A low legacy threshold must not route strict10 through best-effort compaction.
        session.compaction_threshold = Some(1);
        let provider = Capture(Mutex::new(Vec::new()));
        let always = crate::prompt::assemble_always_on("", "", &[]);
        for turn in 1..=12 {
            session.push_user(&format!("marker-{turn}"));
            let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
            let mut ctx = ToolContext {
                sandbox: &sandbox,
                helper: &helper,
                gate: &mut gate,
                approver: &mut approver,
            };
            run(
                &provider,
                &mut session,
                dummy_audit(&dir),
                &mut StopTracker::new(2),
                &always,
                &[],
                &[],
                unused_provider_pool(),
                1,
                1,
                None,
                &mut ctx,
            )
            .await
            .unwrap();
        }
        let requests = provider.0.lock().unwrap();
        assert_eq!(requests.len(), 12);
        assert_eq!(
            requests[9]
                .messages
                .iter()
                .filter(|message| message.content.starts_with("marker-"))
                .count(),
            10
        );
        assert_eq!(
            requests[11]
                .messages
                .iter()
                .filter(|message| message.content.starts_with("marker-"))
                .count(),
            10
        );
        assert!(
            requests[11]
                .messages
                .iter()
                .any(|message| message.content.contains("conversation://"))
        );
        let snapshot = saved.snapshot().unwrap();
        assert_eq!(snapshot.events.len(), 24);
        assert_eq!(
            snapshot
                .events
                .iter()
                .filter(|event| event.starts_turn)
                .count(),
            12
        );
        assert_eq!(snapshot.state.visible_summary_ids.len(), 2);
        assert!(
            !snapshot
                .events
                .iter()
                .any(|event| event.message.content.contains("conversation://"))
        );
    }

    #[async_trait::async_trait]
    impl Provider for TurnOnlyProvider {
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
            panic!("run_loop must use complete_in_turn for model calls")
        }

        async fn complete_in_turn(
            &self,
            _req: CompletionRequest,
            _turn: &polaris_provider::turn_affinity::TurnContext,
        ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
            self.turn_calls
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(CompletionResponse {
                hosted_web_search: Vec::new(),
                url_citations: Vec::new(),
                text: "done".into(),
                ..Default::default()
            })
        }
    }

    #[tokio::test]
    async fn workflow_is_injected_before_send_and_invalid_bindings_make_zero_calls() {
        use crate::workflow::{SessionWorkflow, WorkflowConfig, WorkflowGatesV1, WorkflowStateV1};
        struct Capture(Mutex<Vec<String>>);
        #[async_trait::async_trait]
        impl Provider for Capture {
            async fn complete(
                &self,
                request: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                assert!(request.tools.iter().all(|tool| tool.name != "state"));
                self.0.lock().unwrap().push(request.system);
                Ok(CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "done".into(),
                    ..Default::default()
                })
            }
        }
        let dir = tempfile::tempdir().unwrap();
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let provider = Capture(Mutex::new(Vec::new()));
        for (enabled, mode, reference, expected_ok) in [
            (
                false,
                crate::workflow::WorkflowMode::Adaptive,
                "user:missing",
                true,
            ),
            (
                true,
                crate::workflow::WorkflowMode::Manual,
                "builtin:workflow-core",
                true,
            ),
            (
                true,
                crate::workflow::WorkflowMode::Manual,
                "user:missing",
                false,
            ),
        ] {
            let mut session = Session::new();
            session.workflow = Some(
                SessionWorkflow::restore(
                    WorkflowConfig {
                        enabled,
                        mode,
                        always: vec![reference.into()],
                        ..Default::default()
                    },
                    WorkflowGatesV1::default(),
                    WorkflowStateV1::default(),
                )
                .unwrap(),
            );
            session.push_user("question");
            let mut stop = StopTracker::new(2);
            let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
            let mut ctx = ToolContext {
                sandbox: &sandbox,
                helper: &helper,
                gate: &mut gate,
                approver: &mut approver,
            };
            let before = provider.0.lock().unwrap().len();
            let result = run(
                &provider,
                &mut session,
                dummy_audit(&dir),
                &mut stop,
                &always_on,
                &[],
                &[],
                unused_provider_pool(),
                crate::spawn::DEFAULT_CONCURRENCY,
                crate::spawn::DEFAULT_WRITE_CONCURRENCY,
                None,
                &mut ctx,
            )
            .await;
            assert_eq!(result.is_ok(), expected_ok);
            let captured = provider.0.lock().unwrap();
            assert_eq!(captured.len() - before, usize::from(expected_ok));
            if expected_ok {
                if enabled {
                    assert!(
                        captured
                            .last()
                            .unwrap()
                            .contains("Required workflow instructions")
                    );
                } else {
                    assert_eq!(captured.last().unwrap(), always_on.system());
                }
            }
        }
    }

    /// A provider whose first `complete` call — the summarization call
    /// `compaction::compact` makes internally — fails, and every call
    /// after that succeeds with a scripted reply. Used to prove a
    /// compaction failure doesn't propagate via `?` and block the turn
    /// (see `a_failed_compaction_call_does_not_block_the_turn`).
    struct FailsOnFirstCallThenSucceeds {
        calls: Mutex<usize>,
    }

    #[tokio::test]
    async fn thirty_six_logical_turns_each_use_turn_aware_completion() {
        let dir = tempfile::tempdir().expect("temp directory");
        let provider = TurnOnlyProvider {
            turn_calls: std::sync::atomic::AtomicUsize::new(0),
        };
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        for turn in 0..36 {
            let mut session = Session::new();
            session.push_user(&format!("turn {turn}"));
            let mut stop = StopTracker::new(2);
            let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
            let mut ctx = ToolContext {
                sandbox: &sandbox,
                helper: &helper,
                gate: &mut gate,
                approver: &mut approver,
            };
            let outcome = run(
                &provider,
                &mut session,
                dummy_audit(&dir),
                &mut stop,
                &always_on,
                &[],
                &[],
                unused_provider_pool(),
                crate::spawn::DEFAULT_CONCURRENCY,
                crate::spawn::DEFAULT_WRITE_CONCURRENCY,
                None,
                &mut ctx,
            )
            .await
            .expect("turn succeeds");
            assert_eq!(outcome.text, "done");
        }
        assert_eq!(
            provider
                .turn_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            36
        );
    }

    #[async_trait::async_trait]
    impl Provider for FailsOnFirstCallThenSucceeds {
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
            let mut calls = self.calls.lock().expect("lock");
            *calls += 1;
            if *calls == 1 {
                Err(polaris_provider::ProviderError::Http("boom".into()))
            } else {
                Ok(CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "final reply".into(),
                    ..Default::default()
                })
            }
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

    #[tokio::test]
    async fn the_skill_tool_reads_the_argument_name_its_schema_declares() {
        // Take the argument name from the published schema, not a literal.
        // Change the schema's `q` to `query` (while dispatch still reads
        // `q`) and this test alone exposes the mismatch.
        let dir = tempfile::tempdir().expect("temp directory");
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
        let out = dispatch(
            &call_with("skill", &param, "demo"),
            &skills,
            &[],
            unused_provider_pool(),
            dummy_audit(&dir),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
            None,
            None,
            None,
            &mut ctx,
        )
        .await
        .unwrap_or_else(|e| {
            panic!(
                "dispatch does not read the argument name {param} the public schema declares: {e}"
            )
        });
        assert!(
            out.contains("demo body"),
            "the body was not returned: {out}"
        );
    }

    #[tokio::test]
    async fn the_read_tool_reads_the_argument_name_its_schema_declares() {
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
            &[],
            unused_provider_pool(),
            dummy_audit(&dir),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
            None,
            None,
            None,
            &mut ctx,
        )
        .await
        .unwrap_or_else(|e| {
            panic!(
                "dispatch does not read the argument name {param} the public schema declares: {e}"
            )
        });
        assert!(out.contains("hello"), "the body was not returned: {out}");
    }

    #[tokio::test]
    async fn a_read_without_an_explicit_limit_says_it_stopped_early() {
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
            &[],
            unused_provider_pool(),
            dummy_audit(&dir),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
            None,
            None,
            None,
            &mut ctx,
        )
        .await
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
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({ "path": target.to_str().unwrap() }),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "it was 1 line".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let mut session = Session::new();
        session.push_user("how many lines is a.txt");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
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
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
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
    async fn a_run_call_with_no_events_channel_behaves_identically_to_before() {
        // `events: None` is the polaris-cli one-shot code path. Every other
        // test in this module already exercises it (none of them pass
        // `Some`), so their continuing to pass unmodified after the
        // `events` channel was threaded through `run`/`run_loop`/
        // `dispatch`/`spawn::run_one`/`run_wave` is the primary evidence
        // for this acceptance criterion. This test pins the narrower claim
        // directly: passing `None` all the way through does not panic and
        // still returns the same successful outcome it always did.
        let dir = tempfile::tempdir().expect("temp directory");
        let p = Scripted {
            replies: Mutex::new(vec![CompletionResponse {
                hosted_web_search: Vec::new(),
                url_citations: Vec::new(),
                text: "done".into(),
                tool_calls: vec![],
                ..Default::default()
            }]),
        };

        let mut session = Session::new();
        session.push_user("hello");
        let audit = dummy_audit(&dir);
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
            audit,
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
            &mut ctx,
        )
        .await
        .expect("events: None must not cause run to fail")
        .text;

        assert_eq!(out, "done");
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
            hosted_web_search: Vec::new(),
            url_citations: Vec::new(),
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c".into(),
                name: "read".into(),
                arguments: serde_json::json!({ "path": "/home/u/.ssh/id_rsa" }),
            }],
            ..Default::default()
        };
        let ok_call = || CompletionResponse {
            hosted_web_search: Vec::new(),
            url_citations: Vec::new(),
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
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "done".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let mut session = Session::new();
        session.push_user("read it");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
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
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
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
            hosted_web_search: Vec::new(),
            url_citations: Vec::new(),
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
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
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
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
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
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "skill".into(),
                        arguments: serde_json::json!({ "q": "demo" }),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "read it".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let mut session = Session::new();
        session.push_user("read demo's body");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
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
            audit.clone(),
            &mut stop,
            &always_on,
            &skills,
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
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
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "skill".into(),
                        arguments: serde_json::json!({ "q": "something" }),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "got it".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };
        let mut session = Session::new();
        session.push_user("look up a skill");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
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
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
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
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "write".into(),
                        arguments: serde_json::Value::Object(args),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "wrote it".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("temp directory");
        let mut session = Session::new();
        session.push_user("create a.txt");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
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
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
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
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
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
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "I'll write elsewhere instead".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("temp directory");
        let mut session = Session::new();
        session.push_user("write outside");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
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
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
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
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
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
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "elsewhere".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("temp directory");
        let mut session = Session::new();
        session.push_user("rewrite hardlink.txt");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
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
                audit.clone(),
                &mut stop,
                &always_on,
                &[],
                &[],
                unused_provider_pool(),
                crate::spawn::DEFAULT_CONCURRENCY,
                crate::spawn::DEFAULT_WRITE_CONCURRENCY,
                None,
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
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
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
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "elsewhere".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("temp directory");
        let mut session = Session::new();
        session.push_user("edit hardlink.txt");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
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
                audit.clone(),
                &mut stop,
                &always_on,
                &[],
                &[],
                unused_provider_pool(),
                crate::spawn::DEFAULT_CONCURRENCY,
                crate::spawn::DEFAULT_WRITE_CONCURRENCY,
                None,
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
    async fn d04_always_bash_requires_approval_before_execution() {
        use crate::approval::{ApprovalPolicy, Decision, Gate};
        use polaris_sandbox::{SandboxMode, SandboxPolicy};
        for mode in [
            SandboxMode::ReadOnly,
            SandboxMode::WorkspaceWrite,
            SandboxMode::FullAccess,
        ] {
            for decision in [Decision::Deny, Decision::Allow] {
                let dir = tempfile::tempdir().unwrap();
                let proof = dir.path().canonicalize().unwrap().join("started");
                let roots = if mode == SandboxMode::WorkspaceWrite {
                    vec![dir.path().to_path_buf()]
                } else {
                    vec![]
                };
                let sandbox = SandboxPolicy::new(mode, &roots).unwrap();
                let mut gate = Gate::new(ApprovalPolicy::Always);
                let mut approver = RecordingApprover {
                    decision,
                    asked: 0,
                    last_reason: None,
                };
                let command = if mode == SandboxMode::ReadOnly {
                    "printf d04-ran".to_string()
                } else {
                    format!(
                        "printf d04-ran > '{}' && cat '{}'",
                        proof.display(),
                        proof.display()
                    )
                };
                let call = ToolCall {
                    id: "d04".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command": command}),
                };
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let result = dispatch(
                    &call,
                    &[],
                    &[],
                    unused_provider_pool(),
                    dummy_audit(&dir),
                    1,
                    1,
                    Some(tx.into()),
                    None,
                    None,
                    None,
                    &mut ToolContext {
                        sandbox: &sandbox,
                        helper: Path::new("/bin/true"),
                        gate: &mut gate,
                        approver: &mut approver,
                    },
                )
                .await;
                assert_eq!(
                    approver.asked, 1,
                    "{mode:?}: command executed without asking: {result:?}"
                );
                assert!(approver.last_reason.unwrap().contains(&command));
                if decision == Decision::Deny {
                    assert!(result.unwrap_err().contains("did not approve"));
                    assert!(!proof.exists(), "denied command started");
                } else {
                    assert_eq!(result.unwrap(), "d04-ran");
                    if mode != SandboxMode::ReadOnly {
                        assert_eq!(std::fs::read_to_string(&proof).unwrap(), "d04-ran");
                    }
                }
                assert!(matches!(
                    rx.try_recv().unwrap(),
                    crate::events::AgentEvent::ToolStarted { .. }
                ));
                assert!(
                    matches!(rx.try_recv().unwrap(), crate::events::AgentEvent::ToolFinished { ok, .. } if ok == (decision == Decision::Allow))
                );
            }
        }
    }

    #[tokio::test]
    async fn d04_automatic_regeneration_cannot_drop_always() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let proof = root.join("automatic-child-started");
        let mut agent = files_md_writer_agent_type();
        agent.allowed_tools.push("bash".into());
        let provider = Arc::new(Scripted {
            replies: Mutex::new(vec![
                bash_call(format!("printf started > '{}'", proof.display())),
                CompletionResponse {
                    text: serde_json::json!({"path": root.join("files.md"), "status": "ok"})
                        .to_string(),
                    ..Default::default()
                },
            ]),
        });
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            std::slice::from_ref(&root),
        )
        .unwrap();
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Always);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut pending = crate::dir_watch::DirChanges {
            new_dirs: vec![root],
            ..Default::default()
        };
        flush_directory_changes(
            &mut pending,
            &[agent],
            provider,
            dummy_audit(&dir),
            None,
            &mut ToolContext {
                sandbox: &sandbox,
                helper: Path::new("/bin/true"),
                gate: &mut gate,
                approver: &mut approver,
            },
        )
        .await;
        assert!(!proof.exists(), "automatic child dropped Always");
        let audit = std::fs::read_to_string(dir.path().join("audit.jsonl")).unwrap();
        assert!(audit.contains("approval relay is unavailable"), "{audit}");
        assert!(pending.new_dirs.is_empty());
        assert_eq!(approver.asked, 0);
    }

    #[tokio::test]
    async fn d04_spawn_inherits_always_including_schema_retry() {
        use crate::approval::{ApprovalPolicy, Decision, Gate};
        use polaris_sandbox::{SandboxMode, SandboxPolicy};
        struct Child {
            replies: Mutex<Vec<CompletionResponse>>,
            results: Arc<Mutex<Vec<String>>>,
        }
        #[async_trait::async_trait]
        impl Provider for Child {
            async fn complete(
                &self,
                req: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                self.results.lock().unwrap().extend(
                    req.messages
                        .iter()
                        .filter(|m| m.tool_call_id.is_some())
                        .map(|m| m.content.clone()),
                );
                Ok(self.replies.lock().unwrap().remove(0))
            }
        }
        for retry in [false, true] {
            for policy in [
                ApprovalPolicy::Always,
                ApprovalPolicy::Never,
                ApprovalPolicy::OnRequest,
            ] {
                let dir = tempfile::tempdir().unwrap();
                let root = dir.path().canonicalize().unwrap();
                let proof = root.join("child-started");
                let schema = root.join("schema.json");
                std::fs::write(&schema, r#"{"type":"object"}"#).unwrap();
                let agent = polaris_skills::AgentType {
                    name: "d04-child".into(),
                    description: "fixture".into(),
                    body: "fixture".into(),
                    path: root.clone(),
                    allowed_tools: vec!["bash".into()],
                    access: polaris_skills::AgentAccess::ReadWrite,
                    tier: polaris_skills::Tier::Dwarf,
                    wall_seconds: 30,
                    max_turns: 10,
                    workflow_phase: None,
                    continuation: false,
                    output_schema: schema,
                };
                let mut replies = Vec::new();
                if retry {
                    replies.push(CompletionResponse {
                        text: "invalid json".into(),
                        ..Default::default()
                    });
                }
                replies.push(bash_call(format!(
                    "printf d04-child-ran > '{}' && cat '{}'",
                    proof.display(),
                    proof.display()
                )));
                replies.push(CompletionResponse {
                    text: "{}".into(),
                    ..Default::default()
                });
                let results = Arc::new(Mutex::new(Vec::new()));
                let provider = Arc::new(Child {
                    replies: Mutex::new(replies),
                    results: results.clone(),
                });
                let sandbox =
                    SandboxPolicy::new(SandboxMode::WorkspaceWrite, std::slice::from_ref(&root))
                        .unwrap();
                let mut gate = Gate::new(policy);
                let mut approver = RecordingApprover {
                    decision: Decision::Allow,
                    asked: 0,
                    last_reason: None,
                };
                let call = ToolCall {
                    id: "spawn-d04".into(),
                    name: "spawn".into(),
                    arguments: serde_json::json!({"tasks":[{"type":"d04-child","task":"fixture","write_root":root}]}),
                };
                let out = dispatch(
                    &call,
                    &[],
                    &[agent],
                    provider,
                    dummy_audit(&dir),
                    1,
                    1,
                    None,
                    None,
                    None,
                    None,
                    &mut ToolContext {
                        sandbox: &sandbox,
                        helper: Path::new("/bin/true"),
                        gate: &mut gate,
                        approver: &mut approver,
                    },
                )
                .await
                .unwrap();
                assert_eq!(
                    serde_json::from_str::<serde_json::Value>(&out).unwrap()[0]["ok"],
                    true,
                    "{out}"
                );
                let messages = results.lock().unwrap();
                assert!(!messages.is_empty(), "child did not attempt a command");
                if policy == ApprovalPolicy::Always {
                    assert!(!proof.exists(), "child downgraded Always (retry={retry})");
                    assert!(
                        messages
                            .iter()
                            .any(|m| m.contains("approval relay is unavailable")),
                        "{messages:?}"
                    );
                } else {
                    assert_eq!(std::fs::read_to_string(&proof).unwrap(), "d04-child-ran");
                    assert!(messages.iter().any(|m| m == "d04-child-ran"));
                }
                assert_eq!(approver.asked, 0, "no child approval relay exists");
            }
        }
    }

    #[tokio::test]
    async fn bash_attempts_and_reports_without_ever_consulting_the_approver() {
        // Never preserves unattended confined execution. The real side
        // effect and returned output prevent a no-op from passing.
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
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
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
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "done".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("temp directory");
        let mut session = Session::new();
        session.push_user("create proof.txt and tell me what's in it");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
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
                audit.clone(),
                &mut stop,
                &always_on,
                &[],
                &[],
                unused_provider_pool(),
                crate::spawn::DEFAULT_CONCURRENCY,
                crate::spawn::DEFAULT_WRITE_CONCURRENCY,
                None,
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
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
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
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
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
        let audit = shared_audit(&audit_path);
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
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
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
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
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
                        cached_tokens: 4,
                    }),
                    ..Default::default()
                },
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "it was 1 line".into(),
                    tool_calls: vec![],
                    usage: Some(polaris_provider::Usage {
                        input_tokens: 20,
                        output_tokens: 3,
                        total_tokens: 23,
                        cached_tokens: 16,
                    }),
                    ..Default::default()
                },
            ]),
        };

        let mut session = Session::new();
        session.push_user("how many lines is a.txt");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
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
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
            &mut ctx,
        )
        .await
        .expect("should succeed");

        assert_eq!(outcome.text, "it was 1 line");
        assert_eq!(outcome.usage.input_tokens, 30);
        assert_eq!(outcome.usage.output_tokens, 8);
        assert_eq!(outcome.usage.total_tokens, 38);
        // Distinct per-response values, so a total of 20 can only come
        // from summing both. Left at 0/0, this test passed while the
        // field was being dropped outright, and every `/status` reported
        // `cache 0` no matter what the provider served.
        assert_eq!(
            outcome.usage.cached_tokens, 20,
            "cached tokens are not being carried out of the turn"
        );
    }

    #[tokio::test]
    async fn reasoning_from_a_tool_calling_turn_is_carried_into_the_session() {
        let dir = tempfile::tempdir().expect("temp directory");
        let target = dir.path().join("a.txt");
        std::fs::write(&target, "hello\n").expect("cannot write");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({ "path": target.to_str().unwrap() }),
                    }],
                    reasoning: vec![polaris_provider::ReasoningItem {
                        id: "r1".into(),
                        encrypted_content: "opaque".into(),
                    }],
                    usage: None,
                },
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "it was 1 line".into(),
                    ..Default::default()
                },
            ]),
        };

        let mut session = Session::new();
        session.push_user("how many lines is a.txt");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
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
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
            &mut ctx,
        )
        .await
        .expect("should succeed");

        // messages[0] is the user turn, messages[1] is the assistant turn that
        // called the tool - that's the one that must carry the reasoning item
        // captured alongside it.
        let tool_calling_turn = &session.messages[1];
        assert_eq!(tool_calling_turn.reasoning.len(), 1);
        assert_eq!(tool_calling_turn.reasoning[0].id, "r1");
        assert_eq!(tool_calling_turn.reasoning[0].encrypted_content, "opaque");

        // End to end: confirm the reasoning captured on session.messages[1]
        // actually replays on the wire, and lands before the function_call
        // it informed. session.messages is [user, assistant_tool_calling_turn,
        // tool_result, assistant_final], so input_items on the full history
        // must produce: user message, reasoning (from [1]), function_call
        // (from [1]), function_call_output (from [2]), final message (from
        // [3]).
        let items = polaris_provider::codex::input_items(&session.messages);
        assert_eq!(items.len(), 5, "unexpected item count: {items:?}");
        assert_eq!(items[0]["type"], "message");
        assert_eq!(items[0]["role"], "user");
        assert_eq!(
            items[1]["type"], "reasoning",
            "the reasoning item must appear before the function_call it informed"
        );
        assert!(
            items[1].get("id").is_none(),
            "id must be stripped from the replayed reasoning item"
        );
        assert_eq!(items[1]["encrypted_content"], "opaque");
        assert_eq!(items[2]["type"], "function_call");
        assert_eq!(items[3]["type"], "function_call_output");
        assert_eq!(items[4]["type"], "message");
        assert_eq!(items[4]["role"], "assistant");
    }

    #[tokio::test]
    async fn custom_threshold_meters_summaries_even_when_empty_or_archive_fails() {
        for (summary, archive_fails) in [("summary", false), (" \n", false), ("summary", true)] {
            let dir = tempfile::tempdir().unwrap();
            let mut session = Session::new();
            session.compaction_threshold = Some(1_000);
            session.push_user("old");
            session.push_assistant(&"x".repeat(10_000), vec![]);
            session.push_user("recent");
            session.push_user("latest");
            let original = serde_json::to_value(&session.messages).unwrap();
            let archived = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let calls = archived.clone();
            session.before_compact = Some(Arc::new(move |_| {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if archive_fails {
                    Err(std::io::Error::other("archive failed"))
                } else {
                    Ok(())
                }
            }));
            let p = Scripted {
                replies: Mutex::new(vec![
                    CompletionResponse {
                        hosted_web_search: Vec::new(),
                        url_citations: Vec::new(),
                        text: summary.into(),
                        usage: Some(polaris_provider::Usage {
                            input_tokens: 100,
                            output_tokens: 10,
                            total_tokens: 110,
                            cached_tokens: 50,
                        }),
                        ..Default::default()
                    },
                    CompletionResponse {
                        hosted_web_search: Vec::new(),
                        url_citations: Vec::new(),
                        text: "done".into(),
                        usage: Some(polaris_provider::Usage {
                            input_tokens: 20,
                            output_tokens: 5,
                            total_tokens: 25,
                            cached_tokens: 10,
                        }),
                        ..Default::default()
                    },
                ]),
            };
            let always_on = crate::prompt::assemble_always_on("", "", &[]);
            let mut stop = StopTracker::new(10);
            let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
            let mut ctx = ToolContext {
                sandbox: &sandbox,
                helper: &helper,
                gate: &mut gate,
                approver: &mut approver,
            };
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let outcome = run(
                &p,
                &mut session,
                shared_audit(&dir.path().join("audit.jsonl")),
                &mut stop,
                &always_on,
                &[],
                &[],
                unused_provider_pool(),
                crate::spawn::DEFAULT_CONCURRENCY,
                crate::spawn::DEFAULT_WRITE_CONCURRENCY,
                Some(tx),
                &mut ctx,
            )
            .await
            .unwrap();
            assert_eq!(outcome.text, "done");
            assert_eq!(outcome.usage.total_tokens, 135);
            assert_eq!(outcome.usage.input_tokens, 120);
            assert_eq!(outcome.usage.output_tokens, 15);
            assert_eq!(outcome.usage.cached_tokens, 60);
            assert_eq!(outcome.usage_report.reported_responses, 2);
            assert_eq!(
                archived.load(std::sync::atomic::Ordering::SeqCst),
                usize::from(!summary.trim().is_empty())
            );
            if summary.trim().is_empty() || archive_fails {
                assert_eq!(
                    serde_json::to_value(&session.messages[..4]).unwrap(),
                    original
                );
                assert!(only_model_events(&mut rx));
            } else {
                assert_eq!(session.messages.len(), 4);
                assert!(matches!(
                    next_event_now(&mut rx),
                    Some(crate::events::AgentEvent::HistoryCompacted { .. })
                ));
            }
        }
    }

    #[tokio::test]
    async fn the_compaction_decision_counts_what_the_request_will_actually_carry() {
        // `examples` rides on every request but used to sit outside the
        // tally, so a session could cross the threshold on them and still
        // send uncompacted. The error only ever went one way: the decision
        // under-counted what it was about to send.
        //
        // Compaction can only shrink `session.messages`, so the history
        // here is large enough to be worth summarizing while staying under
        // the threshold on its own. The examples are what carry it over.
        struct Capture {
            requests: Mutex<Vec<CompletionRequest>>,
        }
        #[async_trait::async_trait]
        impl Provider for Capture {
            async fn complete(
                &self,
                request: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                self.requests.lock().unwrap().push(request);
                Ok(CompletionResponse {
                    text: "reply".into(),
                    ..Default::default()
                })
            }
        }
        let dir = tempfile::tempdir().expect("temp directory");
        let body = "x".repeat(crate::compaction::MIN_TOKENS_TO_SUMMARIZE * 40);
        let history = |session: &mut Session| {
            session.push_user("turn 1");
            session.push_assistant(&body, vec![]);
            session.push_user("turn 2");
            session.push_assistant("reply 2", vec![]);
            session.push_user("turn 3");
            // Well inside the ceiling W-2 puts on `examples`, so what this
            // test proves stays reachable in production.
            session.examples = vec![polaris_provider::Message::user("y".repeat(2_400))];
        };
        let always_on = crate::prompt::assemble_always_on("", "", &[]);

        // First run: measure what the request actually carries, rather than
        // rebuilding the tally here and calibrating against a guess.
        let mut measured = Session::new();
        history(&mut measured);
        measured.compaction_threshold = Some(usize::MAX);
        let probe = Capture {
            requests: Mutex::new(Vec::new()),
        };
        {
            let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
            let mut ctx = ToolContext {
                sandbox: &sandbox,
                helper: &helper,
                gate: &mut gate,
                approver: &mut approver,
            };
            run(
                &probe,
                &mut measured,
                shared_audit(&dir.path().join("probe.jsonl")),
                &mut StopTracker::new(10),
                &always_on,
                &[],
                &[],
                unused_provider_pool(),
                crate::spawn::DEFAULT_CONCURRENCY,
                crate::spawn::DEFAULT_WRITE_CONCURRENCY,
                None,
                &mut ctx,
            )
            .await
            .expect("the probe run succeeds");
        }
        let sent = probe.requests.lock().unwrap().remove(0);

        let mut session = Session::new();
        history(&mut session);
        let examples = crate::compaction::session_tokens(&session.examples);
        let without_examples = crate::budget::always_on_tokens(&sent.system, &sent.tools)
            + crate::compaction::session_tokens(&session.messages);
        assert!(
            examples > 0 && examples <= MAX_EXAMPLE_TOKENS,
            "the examples must weigh something, within the ceiling"
        );
        // Reached only once the examples are counted, and not before.
        session.compaction_threshold = Some(without_examples + examples);

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    text: "summary".into(),
                    ..Default::default()
                },
                CompletionResponse {
                    text: "final reply".into(),
                    ..Default::default()
                },
            ]),
        };
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
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
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            Some(events_tx),
            &mut ctx,
        )
        .await
        .expect("should succeed");

        assert_eq!(
            outcome.text, "final reply",
            "the first scripted reply is the summary, so the turn compacted"
        );
        assert!(
            events_rx.try_recv().is_ok_and(|held| matches!(
                held,
                crate::events::AgentEvent::HistoryCompacted { .. }
            )),
            "counting the examples is what brings the total over the threshold"
        );
    }

    #[tokio::test]
    async fn examples_past_either_half_of_the_ceiling_stop_before_a_request_is_built() {
        struct Never;
        #[async_trait::async_trait]
        impl Provider for Never {
            async fn complete(
                &self,
                _: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                panic!("the run must refuse before it reaches the provider");
            }
        }
        let dir = tempfile::tempdir().expect("temp directory");
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        // Short enough that the pairs stay well inside the token half, so
        // the count is what refuses them; and one long message that stays
        // inside the count, so the tokens are what refuses that.
        for (examples, reason) in [
            (
                vec![polaris_provider::Message::user("ex"); MAX_EXAMPLES + 1],
                "件",
            ),
            (
                vec![polaris_provider::Message::user("y".repeat(4_000))],
                "tokens",
            ),
        ] {
            let mut session = Session::new();
            session.push_user("何かする");
            session.examples = examples;
            let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
            let mut ctx = ToolContext {
                sandbox: &sandbox,
                helper: &helper,
                gate: &mut gate,
                approver: &mut approver,
            };
            let failure = run(
                &Never,
                &mut session,
                shared_audit(&dir.path().join("audit.jsonl")),
                &mut StopTracker::new(10),
                &always_on,
                &[],
                &[],
                unused_provider_pool(),
                crate::spawn::DEFAULT_CONCURRENCY,
                crate::spawn::DEFAULT_WRITE_CONCURRENCY,
                None,
                &mut ctx,
            )
            .await
            .expect_err("oversized examples are refused");
            assert!(
                matches!(&failure, AgentError::Io(error) if error.to_string().contains(reason)),
                "{failure}"
            );
            assert_eq!(
                session.messages.len(),
                1,
                "refusing leaves the session as it was, with nothing truncated"
            );
        }
    }

    #[tokio::test]
    async fn an_always_on_floor_past_its_budget_stops_before_sending() {
        struct Never;
        #[async_trait::async_trait]
        impl Provider for Never {
            async fn complete(
                &self,
                _: CompletionRequest,
            ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
                panic!("the run must refuse before it reaches the provider");
            }
        }
        let dir = tempfile::tempdir().expect("temp directory");
        // A tool definition that grew past what the always-on set is
        // allowed. This is the breach D-03 is for: bundled definitions,
        // not a user's rules, which `cap()` cuts long before here.
        let bloated = vec![polaris_tools::ToolSpec {
            name: "bloated".into(),
            description: "d".repeat(crate::budget::BUDGET_LIMIT * 8),
            parameters: serde_json::json!({ "type": "object" }),
        }];
        let mut session = Session::new();
        session.push_user("何かする");
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let failure = run_loop(
            &Never,
            &mut session,
            shared_audit(&dir.path().join("audit.jsonl")),
            &mut StopTracker::new(10),
            "system prompt",
            &bloated,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            "root",
            "",
            false,
            None,
            &mut ctx,
            None,
        )
        .await
        .expect_err("an always-on set past its budget is refused");
        assert!(
            matches!(&failure, AgentError::Io(error)
                if error.to_string().contains(&crate::budget::BUDGET_LIMIT.to_string())),
            "{failure}"
        );
    }

    #[tokio::test]
    async fn a_turn_that_crosses_the_compaction_threshold_compacts_before_sending() {
        let dir = tempfile::tempdir().expect("temp directory");

        // Two big prior turns (well past COMPACTION_THRESHOLD once summed),
        // then a small final turn that triggers compaction before it sends.
        // `x` repeats compress heavily under BPE (o200k_base measures ~8
        // chars/token for a run of identical characters), so a `* 5`
        // multiplier here would only reach ~63k measured tokens — short of
        // COMPACTION_THRESHOLD once `always_on_tokens` and the other turns
        // are added in. `* 10` clears the threshold from this message
        // alone, confirmed against `crate::compaction::session_tokens`.
        let big = "x".repeat(crate::compaction::COMPACTION_THRESHOLD * 10);
        let mut session = Session::new();
        session.push_user("turn 1");
        session.push_assistant(&big, vec![]);
        session.push_user("turn 2");
        session.push_assistant("reply 2", vec![]);
        session.push_user("turn 3 — the new message that pushes it over");

        let p = Scripted {
            replies: Mutex::new(vec![
                // The summarization call compact() makes internally.
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "summary of turns before the kept tail".into(),
                    ..Default::default()
                },
                // The real turn's own response, sent after compaction.
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "final reply".into(),
                    ..Default::default()
                },
            ]),
        };

        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
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
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            Some(events_tx),
            &mut ctx,
        )
        .await
        .expect("should succeed");

        assert_eq!(outcome.text, "final reply");

        let event = events_rx
            .try_recv()
            .expect("a HistoryCompacted event should have been sent");
        assert!(
            matches!(event, crate::events::AgentEvent::HistoryCompacted { .. }),
            "expected HistoryCompacted, got {event:?}"
        );

        // session.messages was replaced: 1 summary + the kept tail (turn 2,
        // reply 2, turn 3) + the final assistant reply = 5.
        assert_eq!(session.messages.len(), 5);
        assert!(
            session.messages[0]
                .content
                .contains("summary of turns before the kept tail")
        );
    }

    #[tokio::test]
    async fn a_failed_compaction_call_does_not_block_the_turn() {
        // Same over-threshold setup as the test above, except the
        // provider's summarization call itself fails. Before this fix, `?`
        // propagated that `ProviderError` straight out of `run_loop`,
        // failing the whole turn — and since `session.messages` is still
        // over `COMPACTION_THRESHOLD` afterward, every subsequent turn
        // (including `/compact` itself) would retry compaction and fail
        // the same way, permanently bricking the conversation until
        // `/clear`/`/new`. The turn must instead proceed with its
        // still-oversized history rather than fail.
        let dir = tempfile::tempdir().expect("temp directory");

        let big = "x".repeat(crate::compaction::COMPACTION_THRESHOLD * 10);
        let mut session = Session::new();
        session.push_user("turn 1");
        session.push_assistant(&big, vec![]);
        session.push_user("turn 2");
        session.push_assistant("reply 2", vec![]);
        session.push_user("turn 3 — the new message that pushes it over");

        let p = FailsOnFirstCallThenSucceeds {
            calls: Mutex::new(0),
        };

        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let (events_tx, mut events_rx) = tokio::sync::mpsc::unbounded_channel();
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
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            Some(events_tx),
            &mut ctx,
        )
        .await
        .expect("a failed compaction call must not fail the turn itself");

        assert_eq!(outcome.text, "final reply");

        // No compaction actually happened — session.messages keeps its
        // original 5 entries plus the turn's own new reply, and the first
        // message is still the original "turn 1", not a summary.
        assert_eq!(session.messages.len(), 6);
        assert_eq!(session.messages[0].content, "turn 1");

        assert!(
            only_model_events(&mut events_rx),
            "no HistoryCompacted event should have been sent for a failed compaction"
        );
    }

    #[tokio::test]
    async fn run_loop_produces_the_same_result_as_run_for_an_equivalent_call() {
        // `run_loop` is the core `run` delegates to (Task 8). This pins
        // down that calling it directly, with the same pieces `run` would
        // have derived from `AlwaysOn` and passed along, produces the same
        // outcome as `run` itself.
        let p = Scripted {
            replies: Mutex::new(vec![CompletionResponse {
                hosted_web_search: Vec::new(),
                url_citations: Vec::new(),
                text: "hello from run_loop".into(),
                tool_calls: vec![],
                ..Default::default()
            }]),
        };
        let mut session = Session::new();
        session.push_user("hi");
        let dir = tempfile::tempdir().expect("temp directory");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let result = run_loop(
            &p,
            &mut session,
            audit.clone(),
            &mut stop,
            "system prompt",
            &polaris_tools::all_specs(),
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            "root",
            "hi",
            false,
            None,
            &mut ctx,
            None,
        )
        .await
        .unwrap();
        assert_eq!(result.text, "hello from run_loop");
    }

    #[tokio::test]
    async fn the_spawn_arm_runs_a_subagent_and_returns_its_result_to_the_root() {
        // The whole chain in one go: the root's loop calls `spawn`,
        // `dispatch` builds the wave, the subagent runs its own loop
        // against the same provider and the same audit log, and its
        // schema-validated result comes back as an ordinary tool result.
        //
        // This is also the regression test for the audit lock's
        // granularity. Take the lock for a whole loop instead of per
        // record (see `run`'s docs) and this test does not fail — it
        // hangs, because the subagent waits on a lock the root holds.
        let dir = tempfile::tempdir().expect("temp directory");
        let target = dir.path().join("a.rs");
        std::fs::write(&target, "fn main() {}\n").expect("cannot write");

        let agents_dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../agents/file-inspector");
        let agent_text =
            std::fs::read_to_string(agents_dir.join("SKILL.md")).expect("cannot read SKILL.md");
        let agent_types = vec![
            polaris_skills::agent_type::parse(&agent_text, "file-inspector", &agents_dir)
                .expect("agents/file-inspector does not parse"),
        ];

        let subagent_result = serde_json::json!({
            "path": target.to_str().expect("path"),
            "responsibility": "The entry point.",
            "test_file": null
        })
        .to_string();

        // One queue, served in order: the root's spawn call, the
        // subagent's single (final) turn, then the root's closing turn.
        // Production shares one provider between root and subagents the
        // same way, which is why `provider_pool` below is this same object.
        let p: Arc<dyn Provider> = Arc::new(Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "spawn".into(),
                        arguments: serde_json::json!({
                            "tasks": [{
                                "type": "file-inspector",
                                "task": "inspect a.rs"
                            }]
                        }),
                    }],
                    usage: Some(polaris_provider::Usage {
                        input_tokens: 10,
                        output_tokens: 3,
                        total_tokens: 13,
                        cached_tokens: 5,
                    }),
                    ..Default::default()
                },
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: subagent_result.clone(),
                    tool_calls: vec![],
                    usage: Some(polaris_provider::Usage {
                        input_tokens: 10,
                        output_tokens: 3,
                        total_tokens: 13,
                        cached_tokens: 5,
                    }),
                    ..Default::default()
                },
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "the subagent reported back".into(),
                    tool_calls: vec![],
                    usage: Some(polaris_provider::Usage {
                        input_tokens: 10,
                        output_tokens: 3,
                        total_tokens: 13,
                        cached_tokens: 5,
                    }),
                    ..Default::default()
                },
            ]),
        });

        let mut session = Session::new();
        session.push_user("inspect a.rs with a subagent");
        let audit_path = dir.path().join("audit.jsonl");
        let audit = shared_audit(&audit_path);
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
            p.as_ref(),
            &mut session,
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &agent_types,
            p.clone(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
            &mut ctx,
        )
        .await
        .expect("the root loop failed");
        assert_eq!(out.usage.total_tokens, 39);
        assert_eq!(out.usage.input_tokens, 30);
        assert_eq!(out.usage.output_tokens, 9);
        assert_eq!(out.usage.cached_tokens, 15);
        assert_eq!(out.usage_report.reported_responses, 3);
        assert_eq!(out.usage_report.missing_responses, 0);
        let out = out.text;
        assert_eq!(out, "the subagent reported back");

        let tool_msg = session
            .messages
            .iter()
            .find(|m| m.tool_call_id.is_some())
            .expect("no tool result was pushed");
        // The wave result is a JSON array with one entry per task (see
        // `spawn::run_wave`'s docs on why it is not newline-joined text).
        let wave: serde_json::Value = serde_json::from_str(&tool_msg.content)
            .unwrap_or_else(|e| panic!("the wave result is not JSON: {e}: {}", tool_msg.content));
        let entries = wave.as_array().expect("the wave result is not an array");
        assert_eq!(entries.len(), 1, "one entry per task: {wave}");
        assert_eq!(
            entries[0]["type"], "file-inspector",
            "the result is not labeled with the type that produced it: {wave}"
        );
        assert_eq!(entries[0]["ok"], true, "{wave}");
        assert_eq!(
            entries[0]["result"],
            serde_json::from_str::<serde_json::Value>(&subagent_result).expect("not JSON"),
            "the subagent's result did not reach the root: {wave}"
        );

        // Both callers land in the one log — the root's `spawn` call and
        // nothing bypassing `AuditLog::record` on the subagent's side.
        let log = std::fs::read_to_string(&audit_path).expect("cannot read the log");
        assert!(
            log.contains("\"tool\":\"spawn\"") && log.contains("\"caller\":\"root\""),
            "the root's spawn call was not recorded: {log}"
        );
    }

    /// The real `agents/files-md-writer` from this repository — its type
    /// definition and the JSON Schema shipped alongside it, not a copy
    /// written here. `files_md::regenerate_for_changes` looks the type up
    /// by the exact name `files-md-writer`, so a renamed fixture would
    /// leave these tests passing against a type production never reaches.
    fn files_md_writer_agent_type() -> polaris_skills::AgentType {
        let dir =
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../agents/files-md-writer");
        let text = std::fs::read_to_string(dir.join("SKILL.md")).expect("cannot read SKILL.md");
        polaris_skills::agent_type::parse(&text, "files-md-writer", &dir)
            .expect("agents/files-md-writer does not parse")
    }

    /// The scripted turns of one `files-md-writer` run: a `read` of
    /// `reads`, then output matching its schema. The `read` is what makes
    /// the run observable — a subagent that returns text and touches
    /// nothing writes no tool audit line of its own. Request metrics are
    /// separate from the tool record used to count regeneration runs.
    fn files_md_writer_turns(
        reads: &std::path::Path,
        dir: &std::path::Path,
    ) -> Vec<CompletionResponse> {
        vec![
            CompletionResponse {
                hosted_web_search: Vec::new(),
                url_citations: Vec::new(),
                text: String::new(),
                tool_calls: vec![ToolCall {
                    id: "sub-1".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({ "path": reads.display().to_string() }),
                }],
                ..Default::default()
            },
            CompletionResponse {
                hosted_web_search: Vec::new(),
                url_citations: Vec::new(),
                text: serde_json::json!({
                    "path": dir.join("files.md").display().to_string(),
                    "status": "ok"
                })
                .to_string(),
                tool_calls: vec![],
                ..Default::default()
            },
        ]
    }

    fn bash_call(command: String) -> CompletionResponse {
        CompletionResponse {
            hosted_web_search: Vec::new(),
            url_citations: Vec::new(),
            text: String::new(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "bash".into(),
                arguments: serde_json::json!({ "command": command }),
            }],
            ..Default::default()
        }
    }

    fn final_text(s: &str) -> CompletionResponse {
        CompletionResponse {
            hosted_web_search: Vec::new(),
            url_citations: Vec::new(),
            text: s.into(),
            tool_calls: vec![],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn a_root_bash_call_that_creates_a_directory_triggers_files_md_regeneration() {
        // The whole mechanism end to end: the root runs `mkdir` via `bash`,
        // the loop diffs the writable root around that call, sees a brand
        // new directory, and drives the `files-md-writer` subagent for it —
        // without the model ever having called `spawn`. Nothing about this
        // is visible in `run_loop`'s return value, so the audit log is the
        // observation point.
        let root = tempfile::tempdir().expect("temp directory");
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("policy");
        // The canonical root, so the directory the diff reports is a path
        // the subagent's own `write_root` can be confined to (on macOS a
        // temp dir's `/var/...` spelling is a symlink to `/private/var/...`).
        let newdir = sandbox.writable_roots()[0].join("newdir");
        // Present in both snapshots, so it is never itself a change.
        let seed = sandbox.writable_roots()[0].join("seed.txt");
        std::fs::write(&seed, "seed\n").expect("cannot write");

        // One queue, served in order, exactly as production shares one
        // provider between the root and its subagents: the root's `bash`
        // turn, then the regeneration subagent's two turns, then the root's
        // closing turn.
        let mut replies = vec![bash_call(format!("mkdir {}", newdir.display()))];
        replies.extend(files_md_writer_turns(&seed, &newdir));
        replies.push(final_text("done"));
        let p: Arc<dyn Provider> = Arc::new(Scripted {
            replies: Mutex::new(replies),
        });

        // The audit log lives outside the watched root on purpose —
        // written inside it, the log file itself would show up in the diff.
        let logs = tempfile::tempdir().expect("temp directory");
        let audit_path = logs.path().join("audit.jsonl");
        let audit = shared_audit(&audit_path);
        let mut session = Session::new();
        session.push_user("make a newdir");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: std::path::Path::new("/bin/true"),
            gate: &mut gate,
            approver: &mut approver,
        };

        let out = run(
            p.as_ref(),
            &mut session,
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[files_md_writer_agent_type()],
            p.clone(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
            &mut ctx,
        )
        .await
        .expect("the root loop failed")
        .text;
        assert_eq!(out, "done", "the root loop did not reach its closing turn");

        assert!(newdir.is_dir(), "the bash call did not actually run");

        let log = std::fs::read_to_string(&audit_path).expect("cannot read the log");
        let cause = log
            .lines()
            .position(|l| l.contains("\"tool\":\"bash\""))
            .unwrap_or_else(|| panic!("the causing bash call was not recorded: {log}"));
        let effect = log
            .lines()
            .position(|l| l.contains("\"caller\":\"files-md-writer\""))
            .unwrap_or_else(|| {
                panic!("the files-md-writer subagent never ran for the new directory: {log}")
            });
        // Cause before effect. The regeneration's own lines must land below
        // the call that caused them, not above it — anyone reading this log
        // later reconstructs the run from its order.
        assert!(
            cause < effect,
            "the regeneration was recorded ahead of the bash call that caused it: {log}"
        );
    }

    #[tokio::test]
    async fn disabled_root_regeneration_does_not_make_an_extra_files_md_request() {
        let root = tempfile::tempdir().expect("temp directory");
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("policy");
        let changed_file = sandbox.writable_roots()[0].join("changed.txt");
        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let p: Arc<dyn Provider> = Arc::new(BashThenAlwaysOk {
            command: format!("echo changed > {}", changed_file.display()),
            calls: calls.clone(),
        });
        let logs = tempfile::tempdir().expect("temp directory");
        let audit_path = logs.path().join("audit.jsonl");
        let audit = shared_audit(&audit_path);
        let mut session = Session::new();
        session.disable_files_md_auto_regenerate = true;
        session.push_user("change a file");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Always);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: std::path::Path::new("/bin/true"),
            gate: &mut gate,
            approver: &mut approver,
        };

        let out = run(
            p.as_ref(),
            &mut session,
            audit,
            &mut stop,
            &always_on,
            &[],
            &[files_md_writer_agent_type()],
            p.clone(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
            &mut ctx,
        )
        .await
        .expect("the root loop failed");

        assert_eq!(out.text, r#"{"path":"files.md","status":"ok"}"#);
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "only the root tool turn and its normal continuation may call the provider"
        );
        let log = std::fs::read_to_string(&audit_path).expect("cannot read the log");
        assert!(
            !log.contains("files-md-writer"),
            "disabled automatic regeneration started a files-md-writer: {log}"
        );
    }

    #[tokio::test]
    async fn manual_files_md_writer_spawn_remains_eligible_when_auto_regeneration_is_disabled() {
        let root = tempfile::tempdir().expect("temp directory");
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("policy");
        let output_path = sandbox.writable_roots()[0].join("files.md");
        let p: Arc<dyn Provider> = Arc::new(Scripted {
            replies: Mutex::new(vec![final_text(
                &serde_json::json!({ "path": output_path, "status": "ok" }).to_string(),
            )]),
        });
        let logs = tempfile::tempdir().expect("temp directory");
        let outcome = crate::spawn::run_one(
            &crate::spawn::SpawnTask {
                agent_type: "files-md-writer".into(),
                task: "write the requested file map".into(),
                write_root: Some(sandbox.writable_roots()[0].display().to_string()),
            },
            &[files_md_writer_agent_type()],
            p,
            shared_audit(&logs.path().join("audit.jsonl")),
            &sandbox,
            std::path::Path::new("/bin/true"),
            None,
        )
        .await;
        assert!(matches!(outcome, crate::spawn::TaskOutcome::Ok(_)));
    }

    #[tokio::test]
    async fn a_root_write_into_a_not_yet_existing_directory_regenerates_for_that_new_directory() {
        // `polaris_sandbox`'s helper runs `create_dir_all` before writing,
        // so a `write` to `sub/new.txt` genuinely creates `sub`. Watching
        // only `sub` could never notice: a directory never appears inside
        // its own listing, so `sub` would never reach `new_dirs`, the file
        // would land in `new_files_in_existing_dirs`, and
        // `targets_to_regenerate` would drop it because `sub/files.md` does
        // not exist yet — the directory would silently never get one, and
        // no later `bash` call could recover it either. Watching `sub`'s
        // own parent as well is what closes that gap.
        let root = tempfile::tempdir().expect("temp directory");
        let helper_dir = tempfile::tempdir().expect("temp directory");
        let helper = write_helper(helper_dir.path());
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("policy");
        // The grandparent needs existing content for its diff to mean
        // anything, and it doubles as something the subagent can read.
        let seed = sandbox.writable_roots()[0].join("seed.txt");
        std::fs::write(&seed, "seed\n").expect("cannot write");
        let sub = sandbox.writable_roots()[0].join("sub");
        let target = sub.join("new.txt");
        assert!(!sub.exists(), "the directory must not exist beforehand");

        let mut replies = vec![CompletionResponse {
            hosted_web_search: Vec::new(),
            url_citations: Vec::new(),
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
        }];
        replies.extend(files_md_writer_turns(&seed, &sub));
        replies.push(final_text("done"));
        let p: Arc<dyn Provider> = Arc::new(Scripted {
            replies: Mutex::new(replies),
        });

        let logs = tempfile::tempdir().expect("temp directory");
        let audit_path = logs.path().join("audit.jsonl");
        let audit = shared_audit(&audit_path);
        let mut session = Session::new();
        session.push_user("create sub/new.txt");
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
            p.as_ref(),
            &mut session,
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[files_md_writer_agent_type()],
            p.clone(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
            &mut ctx,
        )
        .await
        .expect("the root loop failed")
        .text;
        assert_eq!(out, "done", "the root loop did not reach its closing turn");

        assert!(
            target.is_file(),
            "the write did not actually create the file"
        );
        assert!(sub.is_dir(), "the write did not actually create sub/");

        let log = std::fs::read_to_string(&audit_path).expect("cannot read the log");
        assert!(
            log.contains("\"caller\":\"files-md-writer\""),
            "no regeneration ran for the directory the write created: {log}"
        );
    }

    #[tokio::test]
    async fn directory_flush_deduplicates_and_keeps_child_usage() {
        let root = tempfile::tempdir().unwrap();
        let dir = root.path().canonicalize().unwrap();
        std::fs::write(dir.join("files.md"), "map").unwrap();
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            std::slice::from_ref(&dir),
        )
        .unwrap();
        let p: Arc<dyn Provider> = Arc::new(Scripted {
            replies: Mutex::new(vec![CompletionResponse {
                hosted_web_search: Vec::new(),
                url_citations: Vec::new(),
                text: serde_json::json!({"path": dir.join("files.md"), "status": "ok"}).to_string(),
                usage: Some(polaris_provider::Usage {
                    input_tokens: 10,
                    output_tokens: 5,
                    total_tokens: 15,
                    cached_tokens: 0,
                }),
                ..Default::default()
            }]),
        });
        let meter = polaris_provider::UsageMeter::default();
        let measured: Arc<dyn Provider> = Arc::new(meter.wrap(p));
        let mut changes = crate::dir_watch::DirChanges {
            new_dirs: vec![dir.clone(), dir.clone()],
            new_files_in_existing_dirs: vec![dir.join("a"), dir.join("b")],
        };
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: Path::new("/bin/true"),
            gate: &mut gate,
            approver: &mut approver,
        };
        let logs = tempfile::tempdir().unwrap();
        let audit = shared_audit(&logs.path().join("audit.jsonl"));
        flush_directory_changes(
            &mut changes,
            &[files_md_writer_agent_type()],
            measured.clone(),
            audit.clone(),
            None,
            &mut ctx,
        )
        .await;
        assert_eq!(changes, crate::dir_watch::DirChanges::default());
        assert_eq!(meter.snapshot().reported_responses, 1);
        assert_eq!(meter.snapshot().usage.total_tokens, 15);
        flush_directory_changes(
            &mut changes,
            &[files_md_writer_agent_type()],
            measured,
            audit,
            None,
            &mut ctx,
        )
        .await;
        assert_eq!(meter.snapshot().reported_responses, 1);
    }

    #[tokio::test]
    async fn batched_writes_regenerate_once_before_read() {
        let root = tempfile::tempdir().expect("temp directory");
        let helper_dir = tempfile::tempdir().expect("temp directory");
        let helper = write_helper(helper_dir.path());
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("policy");
        // The grandparent needs existing content for its diff to mean
        // anything, and it doubles as something the subagent can read.
        let seed = sandbox.writable_roots()[0].join("seed.txt");
        std::fs::write(&seed, "seed\n").expect("cannot write");
        let sub = sandbox.writable_roots()[0].join("sub");
        let target = sub.join("new.txt");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("files.md"), "existing map").unwrap();

        let mut replies = vec![CompletionResponse {
            hosted_web_search: Vec::new(),
            url_citations: Vec::new(),
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
        }];
        replies[0].tool_calls.push(ToolCall {
            id: "c2".into(),
            name: "write".into(),
            arguments: serde_json::json!({"path": sub.join("other.txt"), "content": "other"}),
        });
        replies[0].tool_calls.push(ToolCall {
            id: "c3".into(),
            name: "read".into(),
            arguments: serde_json::json!({"path": sub.join("files.md")}),
        });
        replies.extend(files_md_writer_turns(&seed, &sub));
        replies.push(final_text("done"));
        let p: Arc<dyn Provider> = Arc::new(Scripted {
            replies: Mutex::new(replies),
        });

        let logs = tempfile::tempdir().expect("temp directory");
        let audit_path = logs.path().join("audit.jsonl");
        let audit = shared_audit(&audit_path);
        let mut session = Session::new();
        session.push_user("create sub/new.txt");
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
            p.as_ref(),
            &mut session,
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[files_md_writer_agent_type()],
            p.clone(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
            &mut ctx,
        )
        .await
        .expect("the root loop failed")
        .text;
        assert_eq!(out, "done", "tool history: {:?}", session.messages);

        assert!(
            target.is_file(),
            "the write did not actually create the file"
        );
        assert!(sub.is_dir(), "the write did not actually create sub/");

        let log = std::fs::read_to_string(&audit_path).expect("cannot read the log");
        assert!(
            log.contains("\"caller\":\"files-md-writer\""),
            "no regeneration ran for the directory the write created: {log}"
        );
        let entries: Vec<serde_json::Value> = log
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let regenerations: Vec<_> = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry["caller"] == "files-md-writer" && entry["tool"] == "read")
            .collect();
        assert_eq!(
            regenerations.len(),
            1,
            "duplicate directory must regenerate once"
        );
        let regeneration = regenerations[0].0;
        let writes: Vec<_> = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| entry["caller"] == "root" && entry["tool"] == "write")
            .map(|(index, _)| index)
            .collect();
        assert_eq!(writes.len(), 2);
        assert!(writes.into_iter().all(|index| index < regeneration));
        let read = entries
            .iter()
            .position(|entry| entry["caller"] == "root" && entry["tool"] == "read")
            .unwrap();
        assert!(regeneration < read, "flush before the root reads its map");
    }

    #[tokio::test]
    async fn the_same_directory_creating_call_from_a_subagent_does_not_trigger_regeneration() {
        // The guard that keeps the mechanism from feeding itself. This is
        // the identical scenario as the test above — same sandbox, same
        // `mkdir`, same discovered type — differing only in `caller`. If
        // the `caller == "root"` condition were dropped, the
        // `files-md-writer` subagent's own `write` of `files.md` (and any
        // other file it touched) would be diffed too and could regenerate
        // the very directory it was writing into.
        let root = tempfile::tempdir().expect("temp directory");
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("policy");
        let newdir = sandbox.writable_roots()[0].join("newdir");

        // Only the root's own two turns are queued. Were the hook to fire
        // here, it would consume the closing reply as the regeneration
        // subagent's turn — so both the queue position (the loop no longer
        // ends on "done") and the audit log (a `files-md-writer` line, for
        // its own tool call or for the failure `files_md` records) would
        // give it away.
        let p: Arc<dyn Provider> = Arc::new(Scripted {
            replies: Mutex::new(vec![
                bash_call(format!("mkdir {}", newdir.display())),
                final_text("done"),
            ]),
        });

        let logs = tempfile::tempdir().expect("temp directory");
        let audit_path = logs.path().join("audit.jsonl");
        let audit = shared_audit(&audit_path);
        let mut session = Session::new();
        session.push_user("make a newdir");
        let mut stop = StopTracker::new(10);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: std::path::Path::new("/bin/true"),
            gate: &mut gate,
            approver: &mut approver,
        };

        let out = run_loop(
            p.as_ref(),
            &mut session,
            audit.clone(),
            &mut stop,
            "system prompt",
            &polaris_tools::all_specs(),
            &[],
            &[files_md_writer_agent_type()],
            p.clone(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            // The only difference from the test above.
            "some-subagent",
            "make a newdir",
            false,
            None,
            &mut ctx,
            None,
        )
        .await
        .expect("the subagent loop failed")
        .text;

        assert!(newdir.is_dir(), "the bash call did not actually run");

        // Nothing attributable to the regeneration may appear: neither the
        // subagent's own tool calls (`"caller":"files-md-writer"`) nor the
        // failure line `files_md` records when a run it started goes wrong.
        let log = std::fs::read_to_string(&audit_path).expect("cannot read the log");
        assert!(
            !log.contains("files-md-writer"),
            "regeneration fired for a non-root caller: {log}"
        );
        // The same fact seen from the reply queue: the closing reply was
        // still there for the loop's own second turn, so nothing else
        // consumed a turn in between.
        assert_eq!(
            out, "done",
            "the closing reply was consumed by something else — the hook fired \
             for a non-root caller"
        );
    }

    #[test]
    fn a_write_to_a_bare_filename_watches_the_working_directory_not_an_empty_path() {
        // The regression test for the silent no-op: `{"path": "README.md"}`
        // — a model naming a new top-level file the ordinary way — has
        // `Path::parent() == Some("")`. `read_dir("")` fails, so both
        // snapshots come back empty, the diff is empty every time, and the
        // hook never fires for what is probably its most common case.
        //
        // This is checked at `pre_call_snapshot` rather than through the
        // whole loop on purpose: the correct answer here is defined
        // relative to the process's working directory, and there is exactly
        // one of those for the whole test binary — moving it would leak
        // into every other test running in parallel. So the scope and the
        // snapshot it produces are inspected directly instead.
        let cwd = std::env::current_dir().expect("cwd");
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };

        let call = call_with("write", "path", "README.md");
        let (scope, before) = pre_call_snapshot("write", &call, &ctx);

        // The working directory, plus its own parent so that a directory
        // the write itself created can be seen as new.
        let mut expected = vec![cwd.clone()];
        expected.extend(cwd.parent().map(Path::to_path_buf));
        assert_eq!(
            scope,
            crate::dir_watch::ScanScope::Shallow(expected),
            "a bare filename was not anchored to the working directory the \
             write itself resolves against"
        );
        // And the scope is a live one, not merely a well-formed value: an
        // unreadable directory would snapshot as empty on both sides and
        // leave the diff permanently blank, which is exactly the defect.
        assert!(
            !before.files.is_empty(),
            "the working directory snapshotted as empty, so no diff taken \
             around this call could ever report anything"
        );
        // What the buggy form did, pinned so the contrast is not merely
        // asserted in prose: the empty parent yields nothing at all.
        assert_eq!(
            crate::dir_watch::snapshot_for_scope(&crate::dir_watch::ScanScope::Shallow(vec![
                std::path::PathBuf::new()
            ])),
            crate::dir_watch::DirSnapshot::default()
        );

        // An absolute path is unaffected by the anchoring.
        let absolute = cwd.join("sub").join("a.txt");
        let call = call_with("write", "path", absolute.to_str().expect("path"));
        let (scope, _) = pre_call_snapshot("write", &call, &ctx);
        assert_eq!(
            scope,
            crate::dir_watch::ScanScope::Shallow(vec![cwd.join("sub"), cwd.clone()])
        );
    }

    #[test]
    fn the_cap_keeps_new_directories_first_and_reports_what_it_dropped() {
        let dirs: Vec<PathBuf> = (0..5).map(|i| PathBuf::from(format!("/d{i}"))).collect();
        let files: Vec<PathBuf> = (0..5).map(|i| PathBuf::from(format!("/f{i}"))).collect();
        let changes = crate::dir_watch::DirChanges {
            new_dirs: dirs.clone(),
            new_files_in_existing_dirs: files.clone(),
        };

        // Under the cap: untouched, nothing dropped.
        let (kept, skipped) = cap_changes(
            crate::dir_watch::DirChanges {
                new_dirs: dirs.clone(),
                new_files_in_existing_dirs: files.clone(),
            },
            10,
        );
        assert_eq!(skipped, 0);
        assert_eq!(kept.new_dirs, dirs);
        assert_eq!(kept.new_files_in_existing_dirs, files);

        // Over the cap: new directories survive first, since a brand new
        // directory has no `files.md` at all while a new file in an
        // existing one only refreshes a `files.md` that already exists.
        let (kept, skipped) = cap_changes(changes, 7);
        assert_eq!(skipped, 3);
        assert_eq!(kept.new_dirs, dirs);
        assert_eq!(kept.new_files_in_existing_dirs, files[..2].to_vec());

        // A cap smaller than the directories alone still trims them.
        let (kept, skipped) = cap_changes(
            crate::dir_watch::DirChanges {
                new_dirs: dirs.clone(),
                new_files_in_existing_dirs: files.clone(),
            },
            3,
        );
        assert_eq!(skipped, 7);
        assert_eq!(kept.new_dirs, dirs[..3].to_vec());
        assert!(kept.new_files_in_existing_dirs.is_empty());
    }

    /// Answers the first call with one fixed `bash` call and every call
    /// after it with output matching `files-md-writer`'s schema, counting
    /// every call it receives. One regeneration run is exactly one call, so
    /// the counter measures how many subagent runs a single tool call set
    /// going.
    struct BashThenAlwaysOk {
        command: String,
        calls: Arc<std::sync::atomic::AtomicUsize>,
    }

    #[async_trait::async_trait]
    impl Provider for BashThenAlwaysOk {
        async fn complete(
            &self,
            _req: CompletionRequest,
        ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
            let n = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(if n == 0 {
                bash_call(self.command.clone())
            } else {
                final_text(r#"{"path":"files.md","status":"ok"}"#)
            })
        }
    }

    #[tokio::test]
    async fn one_tool_call_cannot_start_more_regeneration_runs_than_the_cap() {
        // A single `bash` call can drop an unbounded number of directories
        // at once — `mkdir -p` chains, an archive extraction, a clone. Each
        // one would otherwise be a full serial subagent run that the
        // model's tool result waits on, so the count has to be bounded
        // before any of them start.
        let root = tempfile::tempdir().expect("temp directory");
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("policy");
        let over = MAX_REGENERATION_TARGETS_PER_CALL + 8;
        let names: Vec<String> = (0..over)
            .map(|i| {
                sandbox.writable_roots()[0]
                    .join(format!("d{i:03}"))
                    .display()
                    .to_string()
            })
            .collect();

        let calls = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let p: Arc<dyn Provider> = Arc::new(BashThenAlwaysOk {
            command: format!("mkdir {}", names.join(" ")),
            calls: calls.clone(),
        });

        let logs = tempfile::tempdir().expect("temp directory");
        let audit_path = logs.path().join("audit.jsonl");
        let audit = shared_audit(&audit_path);
        let mut session = Session::new();
        session.push_user("make a lot of directories");
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: std::path::Path::new("/bin/true"),
            gate: &mut gate,
            approver: &mut approver,
        };

        run(
            p.as_ref(),
            &mut session,
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[files_md_writer_agent_type()],
            p.clone(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
            &mut ctx,
        )
        .await
        .expect("the root loop failed");

        // Directories only: the regeneration also registers `**/files.md`
        // in a `.gitignore` at the writable root, which is a file sitting
        // alongside them.
        assert_eq!(
            std::fs::read_dir(&sandbox.writable_roots()[0])
                .expect("cannot read the root")
                .flatten()
                .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
                .count(),
            over,
            "the bash call did not create every directory"
        );

        // The root spends two calls of its own (the `bash` turn and the
        // closing turn); everything in between is one regeneration run per
        // call.
        let total = calls.load(std::sync::atomic::Ordering::SeqCst);
        assert_eq!(
            total - 2,
            MAX_REGENERATION_TARGETS_PER_CALL,
            "{over} new directories started {} regeneration runs — the cap of \
             {MAX_REGENERATION_TARGETS_PER_CALL} was not enforced",
            total - 2
        );

        // Trimming silently would be worse than the fan-out: `files.md`
        // would just be missing for some directories with nothing saying why.
        let log = std::fs::read_to_string(&audit_path).expect("cannot read the log");
        assert!(
            log.lines().any(|l| l.contains("regeneration capped")
                && l.contains(&format!(
                    "{} skipped",
                    over - MAX_REGENERATION_TARGETS_PER_CALL
                ))),
            "the truncation was not recorded: {log}"
        );
    }

    #[tokio::test]
    async fn a_response_with_no_usage_is_reported_as_missing() {
        let dir = tempfile::tempdir().expect("temp directory");
        let p = Scripted {
            replies: Mutex::new(vec![CompletionResponse {
                hosted_web_search: Vec::new(),
                url_citations: Vec::new(),
                text: "done".into(),
                tool_calls: vec![],
                usage: None,
                ..Default::default()
            }]),
        };

        let mut session = Session::new();
        session.push_user("hi");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
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
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            None,
            &mut ctx,
        )
        .await
        .expect("should succeed");

        assert_eq!(outcome.usage.total_tokens, 0);
        assert_eq!(outcome.usage_report.missing_responses, 1);
        assert_eq!(outcome.usage_report.reported_responses, 0);
    }

    #[tokio::test]
    async fn read_dispatches_tool_started_and_finished_events_in_order() {
        let dir = tempfile::tempdir().expect("temp directory");
        std::fs::write(dir.path().join("a.txt"), "hello").expect("cannot write");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({
                            "path": dir.path().join("a.txt").display().to_string(),
                        }),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "done".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let mut session = Session::new();
        session.push_user("read a.txt");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        run(
            &p,
            &mut session,
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            Some(tx),
            &mut ctx,
        )
        .await
        .expect("should succeed");

        let first = next_event(&mut rx).await.expect("no first event");
        assert!(
            matches!(first, crate::events::AgentEvent::ToolStarted { ref name, .. } if name == "read"),
            "first event was not ToolStarted(read): {first:?}"
        );
        let second = next_event(&mut rx).await.expect("no second event");
        assert!(
            matches!(
                second,
                crate::events::AgentEvent::ToolFinished { ref name, ok: true, .. } if name == "read"
            ),
            "second event was not ToolFinished(read, ok: true): {second:?}"
        );
    }

    #[tokio::test]
    async fn tool_finished_carries_the_real_result_text() {
        let dir = tempfile::tempdir().expect("temp directory");
        std::fs::write(dir.path().join("a.txt"), "hello from the file").expect("cannot write");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({
                            "path": dir.path().join("a.txt").display().to_string(),
                        }),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "done".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let mut session = Session::new();
        session.push_user("read a.txt");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        run(
            &p,
            &mut session,
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            Some(tx),
            &mut ctx,
        )
        .await
        .expect("should succeed");

        let _started = next_event(&mut rx).await.expect("no ToolStarted");
        let finished = next_event(&mut rx).await.expect("no ToolFinished");
        let crate::events::AgentEvent::ToolFinished { result, .. } = finished else {
            panic!("expected ToolFinished, got {finished:?}");
        };
        assert!(
            result.contains("hello from the file"),
            "the event carried no real result text: {result:?}"
        );
    }

    #[tokio::test]
    async fn a_failed_tool_puts_its_error_message_in_the_finished_event() {
        let dir = tempfile::tempdir().expect("temp directory");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({}),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "done".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let mut session = Session::new();
        session.push_user("read nothing");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        run(
            &p,
            &mut session,
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            Some(tx),
            &mut ctx,
        )
        .await
        .expect("should succeed");

        let _started = next_event(&mut rx).await.expect("no ToolStarted");
        let finished = next_event(&mut rx).await.expect("no ToolFinished");
        let crate::events::AgentEvent::ToolFinished { ok, result, .. } = finished else {
            panic!("expected ToolFinished, got {finished:?}");
        };
        assert!(!ok);
        assert!(
            result.contains("path is missing"),
            "the failure message was not carried: {result:?}"
        );
    }

    /// `FullAccess`では`Gate::predict`が`is_denied`を見る前に`Allowed`を
    /// 返す。diff用の事前readがその隙を突いて`.env`の中身を端末に流さない
    /// ことを固定する。
    #[tokio::test]
    async fn a_write_to_a_denied_path_does_not_echo_its_old_content_in_the_diff() {
        let dir = tempfile::tempdir().expect("temp directory");
        let target = dir.path().join(".env");
        std::fs::write(&target, "SECRET_TOKEN=hunter2\n").expect("cannot write");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "write".into(),
                        arguments: serde_json::json!({
                            "path": target.display().to_string(),
                            "content": "SECRET_TOKEN=changed\n",
                        }),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "done".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let mut session = Session::new();
        session.push_user("overwrite .env");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        // `FullAccess`なので`Gate`は`.env`でも素通しする。関心は書き込みの
        // 可否ではなく、diffに旧内容が乗らないことだけ。
        let helper_dir = tempfile::tempdir().expect("temp directory");
        let helper = write_helper(helper_dir.path());
        let sandbox =
            polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::FullAccess, &[])
                .expect("policy");
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        run(
            &p,
            &mut session,
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            Some(tx),
            &mut ctx,
        )
        .await
        .expect("should succeed");

        let _started = next_event(&mut rx).await.expect("no ToolStarted");
        let finished = next_event(&mut rx).await.expect("no ToolFinished");
        let crate::events::AgentEvent::ToolFinished {
            ok, diff, result, ..
        } = finished
        else {
            panic!("expected ToolFinished, got {finished:?}");
        };
        assert!(
            ok,
            "the denylist must not change whether the write runs: {result}"
        );
        if let Some(d) = diff {
            for line in d.hunks.iter().flat_map(|h| &h.lines) {
                let text = match line {
                    crate::events::DiffLine::Context(s)
                    | crate::events::DiffLine::Added(s)
                    | crate::events::DiffLine::Removed(s) => s,
                };
                assert!(
                    !text.contains("hunter2"),
                    "the old content of a denied path leaked into the diff: {text:?}"
                );
            }
            assert_eq!(d.removed, 0, "no old line may be read back from .env");
        }
    }

    /// A generic success helper for `edit`: discards its payload and
    /// reports success. `dispatch`'s `"edit"` arm computes the diff from
    /// `old`/`new` in the tool call's own arguments, not from the file the
    /// helper writes, so the helper's own semantics don't matter here —
    /// only that it exits 0.
    fn edit_success_helper(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("fake-edit-success-helper");
        std::fs::write(&p, "#!/bin/sh\nset -e\ncat > /dev/null\necho edited\n")
            .expect("cannot write");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        p
    }

    #[tokio::test]
    async fn writing_a_new_file_reports_a_diff_with_only_added_lines() {
        let root = tempfile::tempdir().expect("temp directory");
        let helper_dir = tempfile::tempdir().expect("temp directory");
        let helper = write_helper(helper_dir.path());
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("policy");
        let target = sandbox.writable_roots()[0].join("new.txt");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "write".into(),
                        arguments: serde_json::json!({
                            "path": target.display().to_string(),
                            "content": "line1\nline2\n",
                        }),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "done".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("temp directory");
        let mut session = Session::new();
        session.push_user("create new.txt");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        run(
            &p,
            &mut session,
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            Some(tx),
            &mut ctx,
        )
        .await
        .expect("should succeed");

        let _started = next_event(&mut rx).await.expect("no ToolStarted");
        let finished = next_event(&mut rx).await.expect("no ToolFinished");
        let crate::events::AgentEvent::ToolFinished { diff, .. } = finished else {
            panic!("expected ToolFinished, got {finished:?}");
        };
        let diff = diff.expect("write should report a diff");
        assert!(diff.is_new_file);
        assert_eq!(diff.added, 2);
        assert_eq!(diff.removed, 0);
    }

    #[tokio::test]
    async fn writing_over_an_existing_file_reports_a_diff_against_its_old_content() {
        let root = tempfile::tempdir().expect("temp directory");
        let helper_dir = tempfile::tempdir().expect("temp directory");
        let helper = write_helper(helper_dir.path());
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("policy");
        let target = sandbox.writable_roots()[0].join("existing.txt");
        std::fs::write(&target, "old line\n").expect("cannot write");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "write".into(),
                        arguments: serde_json::json!({
                            "path": target.display().to_string(),
                            "content": "new line\n",
                        }),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "done".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("temp directory");
        let mut session = Session::new();
        session.push_user("overwrite existing.txt");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        run(
            &p,
            &mut session,
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            Some(tx),
            &mut ctx,
        )
        .await
        .expect("should succeed");

        let _started = next_event(&mut rx).await.expect("no ToolStarted");
        let finished = next_event(&mut rx).await.expect("no ToolFinished");
        let crate::events::AgentEvent::ToolFinished { diff, .. } = finished else {
            panic!("expected ToolFinished, got {finished:?}");
        };
        let diff = diff.expect("overwriting should report a diff");
        assert!(!diff.is_new_file);
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
    }

    #[tokio::test]
    async fn editing_a_file_reports_a_diff_computed_from_the_old_and_new_arguments() {
        let root = tempfile::tempdir().expect("temp directory");
        let helper_dir = tempfile::tempdir().expect("temp directory");
        let helper = edit_success_helper(helper_dir.path());
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("policy");
        let target = sandbox.writable_roots()[0].join("existing.txt");
        std::fs::write(&target, "a\nb\nc\n").expect("cannot write");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "edit".into(),
                        arguments: serde_json::json!({
                            "path": target.display().to_string(),
                            "old": "a\nb\nc\n",
                            "new": "a\nB\nc\n",
                        }),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "done".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("temp directory");
        let mut session = Session::new();
        session.push_user("edit existing.txt");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        run(
            &p,
            &mut session,
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            Some(tx),
            &mut ctx,
        )
        .await
        .expect("should succeed");

        let _started = next_event(&mut rx).await.expect("no ToolStarted");
        let finished = next_event(&mut rx).await.expect("no ToolFinished");
        let crate::events::AgentEvent::ToolFinished { diff, .. } = finished else {
            panic!("expected ToolFinished, got {finished:?}");
        };
        let diff = diff.expect("edit should report a diff");
        assert!(!diff.is_new_file);
        assert_eq!(diff.added, 1);
        assert_eq!(diff.removed, 1);
    }

    /// A helper that always fails without the sandbox's special "request
    /// problem" marker, so `run_mutation` reports it as an ordinary
    /// `WriteDenied` rather than a `MutationFailed`. Either shape is a
    /// plain `Err` by the time it reaches `dispatch`, which is all this
    /// test needs: the gate allows the call through, but the mutation
    /// itself still fails.
    fn always_fails_helper(dir: &std::path::Path) -> std::path::PathBuf {
        let p = dir.join("fake-write-failure-helper");
        std::fs::write(&p, "#!/bin/sh\ncat > /dev/null\necho boom >&2\nexit 1\n")
            .expect("cannot write");
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        p
    }

    #[tokio::test]
    async fn a_failed_write_reports_no_diff() {
        // The gate allows the call through (the target is inside the
        // sandbox root), but the mutation itself fails. No diff should be
        // reported — diff computation is a best-effort side observation of
        // a successful mutation, not something owed on failure.
        let root = tempfile::tempdir().expect("temp directory");
        let helper_dir = tempfile::tempdir().expect("temp directory");
        let helper = always_fails_helper(helper_dir.path());
        let sandbox = polaris_sandbox::SandboxPolicy::new(
            polaris_sandbox::SandboxMode::WorkspaceWrite,
            &[root.path().to_path_buf()],
        )
        .expect("policy");
        let target = sandbox.writable_roots()[0].join("new.txt");

        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "write".into(),
                        arguments: serde_json::json!({
                            "path": target.display().to_string(),
                            "content": "body",
                        }),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "it failed".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("temp directory");
        let mut session = Session::new();
        session.push_user("write new.txt");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        run(
            &p,
            &mut session,
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            Some(tx),
            &mut ctx,
        )
        .await
        .expect("the loop failed");

        let _started = next_event(&mut rx).await.expect("no ToolStarted");
        let finished = next_event(&mut rx).await.expect("no ToolFinished");
        let crate::events::AgentEvent::ToolFinished { ok, diff, .. } = finished else {
            panic!("expected ToolFinished, got {finished:?}");
        };
        assert!(!ok, "the failed write should be reported as failed");
        assert!(diff.is_none(), "a failed write should not report a diff");
    }

    #[tokio::test]
    async fn a_gate_denied_write_still_sends_a_tool_finished_event() {
        // `?` inside a `match` arm early-returns the whole `dispatch`
        // function, skipping the `ToolFinished` send that comes after the
        // `match`. A gate denial must not be able to do that: the TUI's
        // live progress display pairs every `ToolStarted` with a
        // `ToolFinished`, and a denial that never resolves would show up
        // as a row stuck "in progress" forever.
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
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
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
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "I'll write elsewhere instead".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("temp directory");
        let mut session = Session::new();
        session.push_user("write outside");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        // Set to Never, closing off the path that would slip through via
        // approval, so the gate denies the write outright.
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = AlwaysAllow { asked: 0 };
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        run(
            &p,
            &mut session,
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            Some(tx),
            &mut ctx,
        )
        .await
        .expect("the denial failed the whole loop");

        let started = next_event(&mut rx).await.expect("no ToolStarted");
        assert!(
            matches!(started, crate::events::AgentEvent::ToolStarted { ref name, .. } if name == "write"),
            "first event was not ToolStarted(write): {started:?}"
        );
        let finished = next_event(&mut rx).await.expect(
            "no ToolFinished — a gate denial must not skip the event that follows ToolStarted",
        );
        let crate::events::AgentEvent::ToolFinished { ok, .. } = finished else {
            panic!("expected ToolFinished, got {finished:?}");
        };
        assert!(!ok, "a gate-denied write should be reported as failed");
    }

    #[tokio::test]
    async fn a_missing_argument_tool_call_still_sends_a_tool_finished_event() {
        // Same guarantee as above, but for the other `?` early-return
        // source in `dispatch`'s arms: extracting a required argument
        // that the model simply didn't provide.
        let p = Scripted {
            replies: Mutex::new(vec![
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({}),
                    }],
                    ..Default::default()
                },
                CompletionResponse {
                    hosted_web_search: Vec::new(),
                    url_citations: Vec::new(),
                    text: "done".into(),
                    tool_calls: vec![],
                    ..Default::default()
                },
            ]),
        };

        let dir = tempfile::tempdir().expect("temp directory");
        let mut session = Session::new();
        session.push_user("read something");
        let audit = shared_audit(&dir.path().join("audit.jsonl"));
        let mut stop = StopTracker::new(10);
        let always_on = crate::prompt::assemble_always_on("", "", &[]);
        let (sandbox, helper, mut gate, mut approver) = dummy_tool_parts();
        let mut ctx = ToolContext {
            sandbox: &sandbox,
            helper: &helper,
            gate: &mut gate,
            approver: &mut approver,
        };
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();

        run(
            &p,
            &mut session,
            audit.clone(),
            &mut stop,
            &always_on,
            &[],
            &[],
            unused_provider_pool(),
            crate::spawn::DEFAULT_CONCURRENCY,
            crate::spawn::DEFAULT_WRITE_CONCURRENCY,
            Some(tx),
            &mut ctx,
        )
        .await
        .expect("a missing argument should come back as a tool result, not a loop failure");

        let started = next_event(&mut rx).await.expect("no ToolStarted");
        assert!(
            matches!(started, crate::events::AgentEvent::ToolStarted { ref name, .. } if name == "read"),
            "first event was not ToolStarted(read): {started:?}"
        );
        let finished = next_event(&mut rx).await.expect(
            "no ToolFinished — a missing-argument error must not skip the event that follows ToolStarted",
        );
        let crate::events::AgentEvent::ToolFinished { ok, .. } = finished else {
            panic!("expected ToolFinished, got {finished:?}");
        };
        assert!(!ok, "a missing-argument call should be reported as failed");
    }
}
