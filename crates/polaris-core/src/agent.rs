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

/// The `Approver` a subagent is given. Despite the name, it does not make
/// a subagent's writes auto-approved: `spawn` pairs it with
/// [`crate::approval::ApprovalPolicy::Never`], and under that policy
/// [`crate::approval::Gate::check`] returns `Err` for anything the
/// predicate flags as needing approval *before* it ever reaches an
/// `Approver`. So this `ask` is unreachable in production, and a subagent
/// write outside its declared root is refused by the gate — and by the
/// sandbox behind it, whose writable roots `spawn` builds as the same
/// object as the declaration — rather than waved through here.
///
/// It exists because `ToolContext` requires *some* `Approver`, and a
/// subagent runs in the background with nobody to prompt. `Allow` is the
/// honest answer for the one case that could reach it (a policy other than
/// `Never`, which `spawn` never sets): there is no user to consult, so
/// there is no approval to report.
pub(crate) struct AutoApprove;

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
    session.check_persistence()?;
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
        events,
        ctx,
    )
    .await
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
    events: Option<tokio::sync::mpsc::UnboundedSender<crate::events::AgentEvent>>,
    ctx: &mut ToolContext<'_>,
) -> Result<AgentOutcome, AgentError> {
    // Resolve once at the real user boundary; tool continuations retain this
    // exact snapshot. The guard releases the turn on errors and cancellation.
    session.check_persistence()?;
    let workflow_turn = session
        .workflow
        .as_ref()
        .filter(|workflow| workflow.config.enabled)
        .map(|workflow| workflow.begin_turn_from_disk(skills))
        .transpose()
        .map_err(std::io::Error::other)?;
    let workflow_system = workflow_turn
        .as_ref()
        .map(|turn| format!("{system}\n{}", turn.body));
    let system = workflow_system.as_deref().unwrap_or(system);
    session.checkpoint()?;
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
    let historical_evidence = session.prepare_history().await?;
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
    // Every invocation owns its context. Child agents invoke run_loop
    // independently and cannot inherit transport state from their parent.
    let turn_context = polaris_provider::turn_affinity::TurnContext::new();
    loop {
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
            let report = memory.retain(&mut session.messages).await;
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

        let total_tokens = crate::budget::always_on_tokens(system, tools)
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
        let res = provider
            .complete_in_turn(
                CompletionRequest {
                    system: system.to_string(),
                    messages: request_messages,
                    tools: tools.to_vec(),
                },
                &turn_context,
            )
            .await?;

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
        for call in &res.tool_calls {
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
                    call,
                    path,
                    events.as_ref(),
                )
            } else if let Some(path) = memory_path {
                dispatch_memory_read(session.tool_memory.as_ref(), call, path, events.as_ref())
                    .await
            } else {
                dispatch(
                    call,
                    skills,
                    agent_types,
                    provider_pool.clone(),
                    audit.clone(),
                    spawn_concurrency,
                    spawn_write_concurrency,
                    events.clone(),
                    child_workflow.clone(),
                    ctx,
                )
                .await
            };

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

fn dispatch_conversation_read(
    saved: Option<&crate::session_store::PersistedSession>,
    call: &polaris_provider::ToolCall,
    path: &str,
    events: Option<&tokio::sync::mpsc::UnboundedSender<crate::events::AgentEvent>>,
) -> Result<String, String> {
    if let Some(tx) = events {
        let _ = tx.send(crate::events::AgentEvent::ToolStarted {
            name: "read".into(),
            detail: call.arguments.to_string(),
        });
    }
    let result = saved
        .ok_or_else(|| "会話原文の永続保存が有効ではありません".to_string())
        .and_then(|saved| {
            saved.snapshot().map_err(|error| error.to_string())?;
            crate::conversation_memory::read_source(&saved.store, &saved.database, path)
                .map_err(|error| error.to_string())
        });
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
    events: Option<&tokio::sync::mpsc::UnboundedSender<crate::events::AgentEvent>>,
) -> Result<String, String> {
    if let Some(tx) = events {
        let _ = tx.send(crate::events::AgentEvent::ToolStarted {
            name: "read".into(),
            detail: call.arguments.to_string(),
        });
    }
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
    events: Option<tokio::sync::mpsc::UnboundedSender<crate::events::AgentEvent>>,
    workflow: Option<crate::spawn::ChildWorkflow>,
    ctx: &mut ToolContext<'_>,
) -> Result<String, String> {
    if let Some(tx) = &events {
        let _ = tx.send(crate::events::AgentEvent::ToolStarted {
            name: call.name.clone(),
            detail: call.arguments.to_string(),
        });
    }

    let mut pending_diff: Option<crate::events::Diff> = None;

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
            polaris_tools::read::read(Path::new(path), offset, limit).map_err(|e| e.to_string())
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
            if let Err(e) = ctx.gate.check(ctx.sandbox, path, ctx.approver) {
                break 'write Err(e);
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
            let old_content = if polaris_tools::path_policy::is_denied(path) {
                None
            } else {
                std::fs::read_to_string(path).ok()
            };
            let result = polaris_tools::write::write(ctx.sandbox, ctx.helper, path, content)
                .map_err(|e| e.to_string());
            if result.is_ok() {
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
            if let Err(e) = ctx.gate.check(ctx.sandbox, path, ctx.approver) {
                break 'edit Err(e);
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
            polaris_tools::bash::run(ctx.sandbox, command).map_err(|e| e.to_string())
        }
        "skill" => 'skill: {
            let q = match call.arguments["q"].as_str() {
                Some(q) => q,
                None => break 'skill Err("q is missing".to_string()),
            };
            Ok(polaris_tools::skill::lookup(skills, q))
        }
        "spawn" => 'spawn: {
            let tasks_json = match call.arguments["tasks"].as_array() {
                Some(t) => t,
                None => break 'spawn Err("tasks is missing".to_string()),
            };
            let mut tasks = Vec::with_capacity(tasks_json.len());
            for t in tasks_json {
                let agent_type = match t["type"].as_str() {
                    Some(a) => a.to_string(),
                    None => break 'spawn Err("tasks[].type is missing".to_string()),
                };
                let task = match t["task"].as_str() {
                    Some(task) => task.to_string(),
                    None => break 'spawn Err("tasks[].task is missing".to_string()),
                };
                let write_root = t["write_root"].as_str().map(str::to_string);
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
            Ok(Box::pin(crate::spawn::run_wave_scoped(
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

#[cfg(test)]
mod tests {
    use super::*;
    use polaris_provider::{CompletionResponse, ToolCall};
    use std::sync::Mutex;

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

    struct TurnOnlyProvider {
        turn_calls: std::sync::atomic::AtomicUsize,
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
        for (enabled, reference, expected_ok) in [
            (false, "user:missing", true),
            (true, "builtin:workflow-core", true),
            (true, "user:missing", false),
        ] {
            let mut session = Session::new();
            session.workflow = Some(
                SessionWorkflow::restore(
                    WorkflowConfig {
                        enabled,
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
                assert!(rx.try_recv().is_err());
            } else {
                assert_eq!(session.messages.len(), 4);
                assert!(matches!(
                    rx.try_recv().unwrap(),
                    crate::events::AgentEvent::HistoryCompacted { .. }
                ));
            }
        }
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
            events_rx.try_recv().is_err(),
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
            None,
            &mut ctx,
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
    /// nothing writes no audit line of its own, and the audit log is the
    /// only place this harness-driven mechanism is visible from outside
    /// `run_loop`.
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
            .filter(|(_, entry)| entry["caller"] == "files-md-writer")
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
            None,
            &mut ctx,
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

        let first = rx.recv().await.expect("no first event");
        assert!(
            matches!(first, crate::events::AgentEvent::ToolStarted { ref name, .. } if name == "read"),
            "first event was not ToolStarted(read): {first:?}"
        );
        let second = rx.recv().await.expect("no second event");
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

        let _started = rx.recv().await.expect("no ToolStarted");
        let finished = rx.recv().await.expect("no ToolFinished");
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

        let _started = rx.recv().await.expect("no ToolStarted");
        let finished = rx.recv().await.expect("no ToolFinished");
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

        let _started = rx.recv().await.expect("no ToolStarted");
        let finished = rx.recv().await.expect("no ToolFinished");
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

        let _started = rx.recv().await.expect("no ToolStarted");
        let finished = rx.recv().await.expect("no ToolFinished");
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

        let _started = rx.recv().await.expect("no ToolStarted");
        let finished = rx.recv().await.expect("no ToolFinished");
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

        let _started = rx.recv().await.expect("no ToolStarted");
        let finished = rx.recv().await.expect("no ToolFinished");
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

        let _started = rx.recv().await.expect("no ToolStarted");
        let finished = rx.recv().await.expect("no ToolFinished");
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

        let started = rx.recv().await.expect("no ToolStarted");
        assert!(
            matches!(started, crate::events::AgentEvent::ToolStarted { ref name, .. } if name == "write"),
            "first event was not ToolStarted(write): {started:?}"
        );
        let finished = rx.recv().await.expect(
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

        let started = rx.recv().await.expect("no ToolStarted");
        assert!(
            matches!(started, crate::events::AgentEvent::ToolStarted { ref name, .. } if name == "read"),
            "first event was not ToolStarted(read): {started:?}"
        );
        let finished = rx.recv().await.expect(
            "no ToolFinished — a missing-argument error must not skip the event that follows ToolStarted",
        );
        let crate::events::AgentEvent::ToolFinished { ok, .. } = finished else {
            panic!("expected ToolFinished, got {finished:?}");
        };
        assert!(!ok, "a missing-argument call should be reported as failed");
    }
}
