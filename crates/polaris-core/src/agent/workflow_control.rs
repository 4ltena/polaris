//! Bounded model proposals; authority and publication stay with the host.
use super::*;
use crate::workflow::{Phase, WorkflowMode, WorkflowTurn};

#[derive(serde::Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub(super) enum StateOperation {
    Get,
    /// M6-1: the one state window, as the difference since `since`.
    Window {
        #[serde(default)]
        since: Option<u64>,
    },
    Transition {
        phase: Phase,
        expected_revision: u64,
    },
}

pub(super) fn adaptive(session: &Session) -> bool {
    session
        .workflow
        .as_ref()
        .is_some_and(|w| w.config.enabled && w.config.mode == WorkflowMode::Adaptive)
}

pub(super) fn parse(call: &polaris_provider::ToolCall) -> Result<StateOperation, String> {
    serde_json::from_value(call.arguments.clone()).map_err(|_| "stateの引数が不正です".into())
}

fn snapshot(session: &Session, status: &str) -> String {
    let workflow = session.workflow.as_ref().expect("enabled workflow");
    let state = workflow.state();
    serde_json::json!({
        "status":status, "phase":state.phase, "revision":state.revision,
        "spec_approval":workflow.gates.approved_spec_hash.is_some(),
        "verification":match &workflow.gates.verification {
            None => "missing", Some(e) if e.stale => "stale", Some(e) if e.passed => "passed", Some(_) => "failed"
        }
    }).to_string()
}

pub(super) fn get(session: &Session) -> String {
    snapshot(session, "current")
}

/// Answered by the run owner from the published state; the worker holds no
/// store and keeps only the revision it was last given.
pub(super) async fn window(session: &Session, since: Option<u64>) -> Result<String, String> {
    let Some(owner) = session.desktop_workflow.as_ref() else {
        return Err("state windowはこの実行経路では使えません".into());
    };
    let delta = owner
        .window(since)
        .await
        .map_err(|_| "state windowを取得できません".to_string())?;
    serde_json::to_string(&delta).map_err(|_| "state windowを表せません".to_string())
}

pub(super) fn error(code: &str, message: &str) -> String {
    serde_json::json!({"status":code,"message":message}).to_string()
}

pub(super) fn event(
    events: Option<&crate::desktop_events::EventSink>,
    call: &polaris_provider::ToolCall,
    outcome: Option<(&str, bool)>,
) {
    if let Some(tx) = events {
        let detail = call.arguments.to_string();
        let value = match outcome {
            None => crate::events::AgentEvent::ToolStarted {
                name: "state".into(),
                detail,
            },
            Some((result, ok)) => crate::events::AgentEvent::ToolFinished {
                name: "state".into(),
                detail,
                ok,
                result: result.into(),
                diff: None,
            },
        };
        let _ = tx.send(value);
    }
}

pub(super) async fn apply(
    session: &mut Session,
    turn: &WorkflowTurn,
    phase: Phase,
    revision: u64,
    call: &polaris_provider::ToolCall,
) -> Result<Result<String, String>, AgentError> {
    let workflow = session.workflow.as_ref().expect("enabled workflow");
    let mut prepared = match turn.prepare_transition(phase, revision, &workflow.gates) {
        Ok(p) => p,
        Err(e) => return Ok(Err(error("conflict", &e))),
    };
    let result = serde_json::json!({"status":"applied", "phase":prepared.candidate.state.phase,
        "revision":prepared.candidate.state.revision})
    .to_string();
    if let Err(e) = prepared.begin_persist() {
        return Ok(Err(error("conflict", &e)));
    }
    session
        .publish_workflow(
            &prepared.candidate.state,
            Some(polaris_provider::Message::tool_result(&call.id, &result)),
        )
        .await?;
    prepared.commit().map_err(std::io::Error::other)?;
    Ok(Ok(result))
}

pub(super) async fn pending(session: &mut Session, turn: &WorkflowTurn) -> Result<(), AgentError> {
    let Some(mut prepared) = turn
        .prepare_pending(&session.workflow.as_ref().expect("workflow").gates)
        .map_err(std::io::Error::other)?
    else {
        return Ok(());
    };
    prepared.begin_persist().map_err(std::io::Error::other)?;
    session
        .publish_workflow(&prepared.candidate.state, None)
        .await?;
    prepared.commit().map_err(std::io::Error::other)?;
    Ok(())
}
