//! Bounded model proposals; authority and publication stay with the host.
use super::*;
use crate::workflow::{
    ActiveTaskRefV1, ArtifactRefV1, ConvergenceGapCodeV1, ConvergenceGapV1, ConvergenceReportV1,
    Phase, SessionWorkflow, TransitionBlockerV1, WorkflowMode, WorkflowTurn, evaluate_convergence,
    evaluate_phase_transition,
};

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ProjectionAvailability {
    Available,
    Incomplete,
    Unavailable,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "snake_case")]
enum ConvergenceProjectionState {
    Missing,
    Stale,
    Current,
    Incomplete,
    Failed,
}

#[derive(serde::Serialize)]
struct ActiveIdentityProjection {
    state: ProjectionAvailability,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    change_contract: Option<ArtifactRefV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    feature: Option<ArtifactRefV1>,
    #[serde(skip_serializing_if = "Option::is_none")]
    task: Option<ActiveTaskRefV1>,
}

#[derive(serde::Serialize)]
struct ConvergenceProjection {
    state: ConvergenceProjectionState,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    report: Option<ConvergenceReportV1>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    gaps: Vec<ConvergenceGapV1>,
}

#[derive(serde::Serialize)]
struct PhaseCoverageProjection {
    state: ProjectionAvailability,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    evaluated: Vec<Phase>,
    permitted: Vec<Phase>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    blockers: Vec<TransitionBlockerV1>,
}

#[derive(serde::Serialize)]
struct WorkflowProjection {
    state: ProjectionAvailability,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase: Option<Phase>,
    #[serde(skip_serializing_if = "Option::is_none")]
    revision: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    active: Option<ActiveIdentityProjection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    convergence: Option<ConvergenceProjection>,
    #[serde(skip_serializing_if = "Option::is_none")]
    phase_coverage: Option<PhaseCoverageProjection>,
}

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
        },
        // M27 is additive to the legacy verification field.  A model cannot
        // treat a passed legacy flag as proof that current artifacts converge.
        "workflow":workflow_projection(Some(workflow))
    }).to_string()
}

pub(super) fn get(session: &Session) -> String {
    snapshot(session, "current")
}

/// Observed proof invalidation is acknowledged by the same owner as phase
/// transitions, including when the tool later fails or its outcome is unknown.
pub(super) async fn publish_observed_gates(session: &mut Session) -> Result<(), AgentError> {
    if session.desktop_workflow.is_some()
        && let Some(workflow) = session.workflow.as_ref().filter(|w| w.config.enabled)
    {
        let state = workflow.state();
        session.publish_workflow(&state, None).await?;
    }
    Ok(())
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
    // Keep the existing Delta shape intact and add the current, owner-derived
    // workflow projection.  `Delta` is deliberately unaware of M27 artifacts;
    // it remains the durable UI window rather than a second workflow store.
    let mut value = serde_json::to_value(delta).map_err(|_| "state windowを表せません")?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| "state windowを表せません".to_string())?;
    object.insert(
        "workflow".into(),
        serde_json::to_value(workflow_projection(session.workflow.as_ref()))
            .map_err(|_| "workflow状態を表せません")?,
    );
    serde_json::to_string(&value).map_err(|_| "state windowを表せません".to_string())
}

fn workflow_projection(workflow: Option<&SessionWorkflow>) -> WorkflowProjection {
    let Some(workflow) = workflow else {
        return WorkflowProjection {
            state: ProjectionAvailability::Unavailable,
            reason: Some("workflow is not enabled"),
            phase: None,
            revision: None,
            active: None,
            convergence: None,
            phase_coverage: None,
        };
    };
    let state = workflow.state();
    let active = active_identity(workflow, &state);
    let convergence = convergence_projection(workflow, &state, &active);
    let phase_coverage = phase_coverage(workflow, &state, &convergence);
    let availability = match (&active.state, &convergence.state) {
        (ProjectionAvailability::Unavailable, _) | (_, ConvergenceProjectionState::Stale) => {
            ProjectionAvailability::Unavailable
        }
        (
            _,
            ConvergenceProjectionState::Missing
            | ConvergenceProjectionState::Incomplete
            | ConvergenceProjectionState::Failed,
        ) => ProjectionAvailability::Incomplete,
        _ => ProjectionAvailability::Available,
    };
    WorkflowProjection {
        state: availability,
        reason: None,
        phase: Some(state.phase),
        revision: Some(state.revision),
        active: Some(active),
        convergence: Some(convergence),
        phase_coverage: Some(phase_coverage),
    }
}

fn active_identity(
    workflow: &SessionWorkflow,
    state: &crate::workflow::WorkflowStateV1,
) -> ActiveIdentityProjection {
    let graph = workflow.artifact_graph();
    let valid = graph.validate().is_ok()
        && state
            .active_change_contract
            .as_ref()
            .is_none_or(|reference| graph.approved_routine_contract(reference).is_ok())
        && state
            .active_feature
            .as_ref()
            .is_none_or(|reference| graph.validate_active_feature(reference).is_ok())
        && state
            .active_task
            .as_ref()
            .is_none_or(|reference| graph.validate_active_task(reference).is_ok());
    if !valid {
        return ActiveIdentityProjection {
            state: ProjectionAvailability::Unavailable,
            reason: Some("active artifact identity is not current"),
            change_contract: None,
            feature: None,
            task: None,
        };
    }
    ActiveIdentityProjection {
        state: if state.active_change_contract.is_none()
            && state.active_feature.is_none()
            && state.active_task.is_none()
        {
            ProjectionAvailability::Incomplete
        } else {
            ProjectionAvailability::Available
        },
        reason: (state.active_change_contract.is_none()
            && state.active_feature.is_none()
            && state.active_task.is_none())
        .then_some("no active artifact is selected"),
        change_contract: state.active_change_contract.clone(),
        feature: state.active_feature.clone(),
        task: state.active_task.clone(),
    }
}

fn convergence_projection(
    workflow: &SessionWorkflow,
    state: &crate::workflow::WorkflowStateV1,
    active: &ActiveIdentityProjection,
) -> ConvergenceProjection {
    let Some(input) = workflow.gates.convergence.as_ref() else {
        return ConvergenceProjection {
            state: ConvergenceProjectionState::Missing,
            reason: Some("no saved convergence evidence"),
            report: None,
            gaps: Vec::new(),
        };
    };
    let graph = workflow.artifact_graph();
    let binding_is_current = input.validate().is_ok()
        && active.state != ProjectionAvailability::Unavailable
        && graph
            .basis_hash()
            .is_ok_and(|hash| hash == input.binding.artifact_graph_basis_hash)
        && if let Some(task) = &state.active_task {
            input.binding.task_id == task.task_id
                && input.binding.task_definition_hash == task.definition_hash
        } else if let Some(contract) = &state.active_change_contract {
            input.binding.task_id == contract.id
                && input.binding.task_definition_hash == contract.content_hash
        } else {
            false
        };
    if !binding_is_current {
        return ConvergenceProjection {
            state: ConvergenceProjectionState::Stale,
            reason: Some("saved convergence does not bind to the current active artifact"),
            report: None,
            gaps: Vec::new(),
        };
    }
    let report = evaluate_convergence(input);
    let failed = report.gaps.iter().any(|gap| {
        matches!(
            gap.code,
            ConvergenceGapCodeV1::RequiredEvidenceFailed | ConvergenceGapCodeV1::EvidenceConflict
        )
    });
    let projection_state = if report.converged() {
        ConvergenceProjectionState::Current
    } else if failed {
        ConvergenceProjectionState::Failed
    } else {
        ConvergenceProjectionState::Incomplete
    };
    ConvergenceProjection {
        state: projection_state,
        reason: None,
        gaps: report.gaps.clone(),
        report: Some(report),
    }
}

fn phase_coverage(
    workflow: &SessionWorkflow,
    state: &crate::workflow::WorkflowStateV1,
    convergence: &ConvergenceProjection,
) -> PhaseCoverageProjection {
    if workflow.artifact_graph().validate().is_err() {
        return PhaseCoverageProjection {
            state: ProjectionAvailability::Unavailable,
            reason: Some("workflow artifact graph is unavailable"),
            evaluated: Vec::new(),
            permitted: Vec::new(),
            blockers: Vec::new(),
        };
    }
    let mut permitted = Vec::new();
    let mut blockers = Vec::new();
    for target in Phase::ALL {
        match evaluate_phase_transition(state, target, &workflow.gates, workflow.artifact_graph()) {
            Ok(_) => permitted.push(target),
            Err(blocker) => blockers.push(blocker),
        }
    }
    let coverage_state = match convergence.state {
        ConvergenceProjectionState::Stale => ProjectionAvailability::Unavailable,
        ConvergenceProjectionState::Missing
        | ConvergenceProjectionState::Incomplete
        | ConvergenceProjectionState::Failed => ProjectionAvailability::Incomplete,
        ConvergenceProjectionState::Current => ProjectionAvailability::Available,
    };
    PhaseCoverageProjection {
        state: coverage_state,
        reason: (coverage_state != ProjectionAvailability::Available).then_some(
            "phase coverage is incomplete until current convergence evidence is available",
        ),
        evaluated: Phase::ALL.to_vec(),
        permitted,
        blockers,
    }
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
        .publish_workflow_transition(
            &prepared.candidate.state,
            Some(polaris_provider::Message::tool_result(&call.id, &result)),
            prepared.gate_basis_hash(),
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
        .publish_workflow_transition(&prepared.candidate.state, None, prepared.gate_basis_hash())
        .await?;
    prepared.commit().map_err(std::io::Error::other)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn projection_marks_missing_evidence_incomplete_without_claiming_completion() {
        let workflow = SessionWorkflow::new(crate::workflow::WorkflowConfig {
            enabled: true,
            ..Default::default()
        });

        let value = serde_json::to_value(workflow_projection(Some(&workflow))).unwrap();

        assert_eq!(value["state"], "incomplete");
        assert_eq!(value["active"]["state"], "incomplete");
        assert_eq!(value["convergence"]["state"], "missing");
        assert_eq!(value["phase_coverage"]["state"], "incomplete");
        assert_ne!(value["convergence"]["state"], "current");
    }

    #[test]
    fn projection_keeps_an_unconfigured_workflow_unavailable() {
        let value = serde_json::to_value(workflow_projection(None)).unwrap();

        assert_eq!(value["state"], "unavailable");
        assert_eq!(value["reason"], "workflow is not enabled");
        assert!(value.get("phase_coverage").is_none());
    }
}
