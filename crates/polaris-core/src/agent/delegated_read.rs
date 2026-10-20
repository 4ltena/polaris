//! Shared execution boundary for one delegated file-reader task.

use std::path::Path;
use std::sync::Arc;

use polaris_provider::Provider;
use tokio::sync::Mutex;

use super::{AgentError, ToolContext, check_event_delivery};
use crate::audit::{AuditLog, Record};
use crate::session::Session;

pub(super) enum EventResult {
    Raw,
    Brief,
}

pub(super) struct DelegatedRead {
    pub entry: Option<serde_json::Value>,
    raw: String,
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn run(
    session: &Session,
    task: crate::spawn::SpawnTask,
    detail: String,
    target: &Path,
    caller: &str,
    event_result: EventResult,
    agent_types: &[polaris_skills::AgentType],
    provider_pool: Arc<dyn Provider>,
    audit: Arc<Mutex<AuditLog>>,
    events: Option<crate::desktop_events::EventSink>,
    child_workflow: Option<crate::spawn::ChildWorkflow>,
    spawn_concurrency: usize,
    spawn_write_concurrency: usize,
    ctx: &ToolContext<'_>,
) -> Result<DelegatedRead, AgentError> {
    if let Some(sink) = &events {
        let _ = sink.send(crate::events::AgentEvent::ToolStarted {
            name: "spawn".into(),
            detail: detail.clone(),
        });
    }
    check_event_delivery(events.as_ref())?;
    let control = session
        .issue_spawn_control(1)
        .map_err(std::io::Error::other)?;
    let raw = Box::pin(crate::spawn::run_wave_with_gate_controlled(
        vec![task],
        agent_types,
        provider_pool,
        audit.clone(),
        ctx.sandbox,
        ctx.helper,
        spawn_concurrency,
        spawn_write_concurrency,
        events.clone(),
        child_workflow,
        ctx.gate,
        session.tier_decider.as_deref(),
        1,
        session.child_memory.clone(),
        control,
    ))
    .await;
    let entry = serde_json::from_str::<serde_json::Value>(&raw)
        .ok()
        .and_then(|value| {
            value
                .as_array()
                .and_then(|entries| entries.first())
                .cloned()
        });
    let delegated = DelegatedRead { entry, raw };
    let ok = delegated
        .entry
        .as_ref()
        .and_then(|entry| entry.get("ok"))
        .and_then(serde_json::Value::as_bool)
        == Some(true);
    if let Some(sink) = &events {
        let result = match event_result {
            EventResult::Raw => delegated.raw.clone(),
            EventResult::Brief if ok => "file-reader evidence returned".into(),
            EventResult::Brief => "file-reader returned a gap".into(),
        };
        let _ = sink.send(crate::events::AgentEvent::ToolFinished {
            name: "spawn".into(),
            detail: detail.clone(),
            ok,
            result,
            diff: None,
        });
    }
    check_event_delivery(events.as_ref())?;
    audit.lock().await.record(&Record {
        tool: "spawn",
        detail: &detail,
        sandbox: None,
        target: Some(target),
        result: &delegated.raw,
        caller,
    })?;
    Ok(delegated)
}
