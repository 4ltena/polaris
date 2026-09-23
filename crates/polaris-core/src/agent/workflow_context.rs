//! Request-only projection of exact active workflow artifacts.
//!
//! The owner captures bodies once at a real-turn boundary.  This module never
//! writes history and can therefore safely reapply the same projection when a
//! provider request is rebuilt after a tool call.

use crate::{
    request_input::BoundInput,
    workflow::{
        ArtifactRefV1, ChangeContractV1, FeatureSpecV1, IdeaDecisionV1, ImplementationPlanV1,
        ProjectPrinciplesV1, TaskGraphV1, WorkflowArtifactGraphV1, WorkflowArtifactKindV1,
        WorkflowContextBodiesV1, WorkflowContextSliceV1, WorkflowGatesV1, WorkflowStateV1,
        WorkflowTurn, build_workflow_context_slice, runtime_context_inputs,
    },
};
use polaris_provider::{Message, Role};
use serde::Serialize;
use sha2::{Digest, Sha256};

pub(crate) const ROOT_WORKFLOW_CONTEXT_MARKER_V1: &str = "<polaris-root-workflow-context-v1>";
const ROOT_WORKFLOW_CONTEXT_END_V1: &str = "</polaris-root-workflow-context-v1>";

#[derive(Debug, Clone, Default)]
pub(crate) struct WorkflowArtifactBodiesSnapshotV1 {
    pub feature_spec: Option<FeatureSpecV1>,
    pub implementation_plan: Option<ImplementationPlanV1>,
    pub task_graph: Option<TaskGraphV1>,
    pub change_contract: Option<ChangeContractV1>,
    pub project_principles: Option<ProjectPrinciplesV1>,
    pub idea_decisions: Vec<IdeaDecisionV1>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct WorkflowContextProjectionV1 {
    pub inserted: bool,
    pub input_index: usize,
    pub token_cost: usize,
    pub complete: bool,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum WorkflowContextErrorV1 {
    #[error("workflow context artifact is invalid: {0}")]
    Invalid(String),
    #[error("active workflow artifact body is missing: {0}")]
    Missing(&'static str),
    #[error("active workflow artifact body does not match: {0}")]
    Mismatch(&'static str),
    #[error("workflow turn and captured state differ")]
    TurnStateMismatch,
    #[error("a different root workflow context is already in this request")]
    ConflictingProjection,
}

/// Immutable capture for one root turn.  `complete` is intentionally allowed
/// to be false when evidence/convergence has not been wired yet; active body
/// integrity is never represented as incomplete and instead fails closed.
#[derive(Debug, Clone)]
pub(crate) struct PreparedWorkflowContextV1 {
    identity: String,
    message: Option<Message>,
    token_cost: usize,
    complete: bool,
    slice: Option<WorkflowContextSliceV1>,
}

impl PreparedWorkflowContextV1 {
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "compatibility constructor retained for focused context tests"
        )
    )]
    pub(crate) fn capture(
        turn: &WorkflowTurn,
        state: WorkflowStateV1,
        graph: WorkflowArtifactGraphV1,
        bodies: WorkflowArtifactBodiesSnapshotV1,
    ) -> Result<Self, WorkflowContextErrorV1> {
        Self::capture_with_gates(turn, state, graph, bodies, &WorkflowGatesV1::default())
    }

    pub(crate) fn capture_with_gates(
        turn: &WorkflowTurn,
        state: WorkflowStateV1,
        graph: WorkflowArtifactGraphV1,
        bodies: WorkflowArtifactBodiesSnapshotV1,
        gates: &WorkflowGatesV1,
    ) -> Result<Self, WorkflowContextErrorV1> {
        if turn.state != state {
            return Err(WorkflowContextErrorV1::TurnStateMismatch);
        }
        validate_exact_bodies(&state, &graph, &bodies)?;
        if state.active_task.is_none() && state.active_change_contract.is_none() {
            return Ok(Self {
                identity: "none".into(),
                message: None,
                token_cost: 0,
                complete: true,
                slice: None,
            });
        }
        let (recent_evidence, transitions) = runtime_context_inputs(&state, &graph, gates);
        let slice = build_workflow_context_slice(
            &state,
            &graph,
            WorkflowContextBodiesV1 {
                change_contract: bodies.change_contract.as_ref(),
                feature_spec: bodies.feature_spec.as_ref(),
                implementation_plan: bodies.implementation_plan.as_ref(),
                task_graph: bodies.task_graph.as_ref(),
                project_principles: bodies.project_principles.as_ref(),
                idea_decisions: &bodies.idea_decisions,
            },
            recent_evidence,
            transitions,
        )
        .map_err(WorkflowContextErrorV1::Invalid)?;
        let envelope = RootWorkflowEnvelopeV1 {
            schema_version: 1,
            scope: "root_only",
            workflow: &slice,
            active_change_contract: state.active_change_contract.as_ref(),
        };
        let payload = serde_json::to_string(&envelope)
            .map_err(|error| WorkflowContextErrorV1::Invalid(error.to_string()))?;
        let content =
            format!("{ROOT_WORKFLOW_CONTEXT_MARKER_V1}\n{payload}\n{ROOT_WORKFLOW_CONTEXT_END_V1}");
        Ok(Self {
            identity: format!("{:x}", Sha256::digest(content.as_bytes())),
            message: Some(Message::user(content)),
            token_cost: slice.estimated_tokens,
            complete: slice.complete,
            slice: Some(slice),
        })
    }

    pub(crate) fn token_cost(&self) -> usize {
        self.token_cost
    }
    pub(crate) fn complete(&self) -> bool {
        self.complete
    }
    pub(crate) fn slice(&self) -> Option<&WorkflowContextSliceV1> {
        self.slice.as_ref()
    }
    /// Insert exactly once immediately before the real user input.  The caller
    /// advances its ephemeral `BoundInput.index` to `input_index` when this
    /// returns `inserted: true`; no object here is persisted.
    pub(crate) fn insert_once(
        &self,
        messages: &mut Vec<Message>,
        input: &BoundInput,
    ) -> Result<WorkflowContextProjectionV1, WorkflowContextErrorV1> {
        let Some(context) = &self.message else {
            return Ok(WorkflowContextProjectionV1 {
                inserted: false,
                input_index: input.index,
                token_cost: 0,
                complete: true,
            });
        };
        if input.index > messages.len() {
            return Err(WorkflowContextErrorV1::Invalid(
                "bound input is outside request".into(),
            ));
        }
        let markers = messages
            .iter()
            .enumerate()
            .filter(|(_, message)| {
                message.role == Role::User
                    && message.content.starts_with(ROOT_WORKFLOW_CONTEXT_MARKER_V1)
            })
            .collect::<Vec<_>>();
        if markers.is_empty() {
            messages.insert(input.index, context.clone());
            return Ok(WorkflowContextProjectionV1 {
                inserted: true,
                input_index: input.index + 1,
                token_cost: self.token_cost,
                complete: self.complete,
            });
        }
        if markers.len() != 1
            || format!("{:x}", Sha256::digest(markers[0].1.content.as_bytes())) != self.identity
            || markers[0].0 + 1 != input.index
        {
            return Err(WorkflowContextErrorV1::ConflictingProjection);
        }
        Ok(WorkflowContextProjectionV1 {
            inserted: false,
            input_index: input.index,
            token_cost: self.token_cost,
            complete: self.complete,
        })
    }
}

/// Load the exact active bodies once at the real user boundary. Every read is
/// content-addressed through the session owner; a missing or stale body stops
/// the turn before any provider request.
pub(crate) async fn prepare(
    session: &crate::session::Session,
    turn: &WorkflowTurn,
) -> Result<PreparedWorkflowContextV1, WorkflowContextErrorV1> {
    let workflow = session
        .workflow
        .as_ref()
        .ok_or_else(|| WorkflowContextErrorV1::Invalid("workflow is missing".into()))?;
    let state = turn.state.clone();
    let graph = workflow.artifact_graph().clone();
    let mut bodies = WorkflowArtifactBodiesSnapshotV1::default();

    if let Some(reference) = state.active_change_contract.clone() {
        let value = session
            .read_workflow_artifact(WorkflowArtifactKindV1::ChangeContract, reference)
            .await
            .map_err(|error| WorkflowContextErrorV1::Invalid(error.to_string()))?;
        bodies.change_contract = Some(
            serde_json::from_value(value)
                .map_err(|error| WorkflowContextErrorV1::Invalid(error.to_string()))?,
        );
    }
    let approved_principles = graph
        .artifacts
        .iter()
        .filter(|item| {
            item.kind == WorkflowArtifactKindV1::ProjectPrinciples
                && item.header.status == crate::workflow::ArtifactStatusV1::Approved
        })
        .map(|item| item.reference())
        .collect::<Vec<_>>();
    if approved_principles.len() == 1 {
        let value = session
            .read_workflow_artifact(
                WorkflowArtifactKindV1::ProjectPrinciples,
                approved_principles[0].clone(),
            )
            .await
            .map_err(|error| WorkflowContextErrorV1::Invalid(error.to_string()))?;
        bodies.project_principles = Some(
            serde_json::from_value(value)
                .map_err(|error| WorkflowContextErrorV1::Invalid(error.to_string()))?,
        );
    }
    if let Some(reference) = state
        .active_task
        .as_ref()
        .map(|task| task.task_graph.clone())
    {
        let value = session
            .read_workflow_artifact(WorkflowArtifactKindV1::TaskGraph, reference)
            .await
            .map_err(|error| WorkflowContextErrorV1::Invalid(error.to_string()))?;
        let task_graph: TaskGraphV1 = serde_json::from_value(value)
            .map_err(|error| WorkflowContextErrorV1::Invalid(error.to_string()))?;
        let feature = session
            .read_workflow_artifact(
                WorkflowArtifactKindV1::FeatureSpec,
                task_graph.feature_spec.clone(),
            )
            .await
            .map_err(|error| WorkflowContextErrorV1::Invalid(error.to_string()))?;
        let plan = session
            .read_workflow_artifact(
                WorkflowArtifactKindV1::ImplementationPlan,
                task_graph.implementation_plan.clone(),
            )
            .await
            .map_err(|error| WorkflowContextErrorV1::Invalid(error.to_string()))?;
        bodies.feature_spec = Some(
            serde_json::from_value(feature)
                .map_err(|error| WorkflowContextErrorV1::Invalid(error.to_string()))?,
        );
        bodies.implementation_plan = Some(
            serde_json::from_value(plan)
                .map_err(|error| WorkflowContextErrorV1::Invalid(error.to_string()))?,
        );
        bodies.task_graph = Some(task_graph);
    } else if let Some(reference) = state.active_feature.clone() {
        let value = session
            .read_workflow_artifact(WorkflowArtifactKindV1::FeatureSpec, reference)
            .await
            .map_err(|error| WorkflowContextErrorV1::Invalid(error.to_string()))?;
        bodies.feature_spec = Some(
            serde_json::from_value(value)
                .map_err(|error| WorkflowContextErrorV1::Invalid(error.to_string()))?,
        );
    }
    if let Some(feature) = state.active_feature.as_ref() {
        let decision_refs = graph
            .metadata(feature)
            .map(|item| {
                item.header
                    .created_from
                    .iter()
                    .filter(|reference| {
                        graph.metadata(reference).is_some_and(|source| {
                            source.kind == WorkflowArtifactKindV1::IdeaDecision
                        })
                    })
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        for reference in decision_refs {
            let value = session
                .read_workflow_artifact(WorkflowArtifactKindV1::IdeaDecision, reference)
                .await
                .map_err(|error| WorkflowContextErrorV1::Invalid(error.to_string()))?;
            bodies.idea_decisions.push(
                serde_json::from_value(value)
                    .map_err(|error| WorkflowContextErrorV1::Invalid(error.to_string()))?,
            );
        }
    }
    PreparedWorkflowContextV1::capture_with_gates(turn, state, graph, bodies, &workflow.gates)
}

#[derive(Serialize)]
struct RootWorkflowEnvelopeV1<'a> {
    schema_version: u32,
    scope: &'static str,
    workflow: &'a WorkflowContextSliceV1,
    #[serde(skip_serializing_if = "Option::is_none")]
    active_change_contract: Option<&'a ArtifactRefV1>,
}

fn validate_exact_bodies(
    state: &WorkflowStateV1,
    graph: &WorkflowArtifactGraphV1,
    bodies: &WorkflowArtifactBodiesSnapshotV1,
) -> Result<(), WorkflowContextErrorV1> {
    graph.validate().map_err(WorkflowContextErrorV1::Invalid)?;
    if let Some(reference) = &state.active_change_contract {
        let contract = bodies
            .change_contract
            .as_ref()
            .ok_or(WorkflowContextErrorV1::Missing("change contract"))?;
        contract
            .validate()
            .map_err(WorkflowContextErrorV1::Invalid)?;
        if contract.reference() != *reference
            || graph
                .metadata(reference)
                .is_none_or(|item| item.kind != WorkflowArtifactKindV1::ChangeContract)
        {
            return Err(WorkflowContextErrorV1::Mismatch("change contract"));
        }
        graph
            .approved_routine_contract(reference)
            .map_err(WorkflowContextErrorV1::Invalid)?;
    }
    let Some(feature_ref) = state.active_feature.as_ref() else {
        return if state.active_task.is_some() {
            Err(WorkflowContextErrorV1::Missing("active feature reference"))
        } else {
            Ok(())
        };
    };
    let feature = bodies
        .feature_spec
        .as_ref()
        .ok_or(WorkflowContextErrorV1::Missing("feature spec"))?;
    feature
        .validate()
        .map_err(WorkflowContextErrorV1::Invalid)?;
    if feature.reference() != *feature_ref
        || graph
            .metadata(feature_ref)
            .is_none_or(|item| item.kind != WorkflowArtifactKindV1::FeatureSpec)
    {
        return Err(WorkflowContextErrorV1::Mismatch("feature spec"));
    }
    let Some(task_ref) = state.active_task.as_ref() else {
        return Ok(());
    };
    let plan = bodies
        .implementation_plan
        .as_ref()
        .ok_or(WorkflowContextErrorV1::Missing("implementation plan"))?;
    let task_graph = bodies
        .task_graph
        .as_ref()
        .ok_or(WorkflowContextErrorV1::Missing("task graph"))?;
    plan.validate().map_err(WorkflowContextErrorV1::Invalid)?;
    task_graph
        .validate()
        .map_err(WorkflowContextErrorV1::Invalid)?;
    if task_ref.validate_against(task_graph).is_err()
        || task_graph.feature_spec != feature.reference()
        || task_graph.implementation_plan != plan.reference()
        || plan.feature_spec != feature.reference()
        || graph
            .metadata(&task_ref.task_graph)
            .is_none_or(|item| item.kind != WorkflowArtifactKindV1::TaskGraph)
    {
        return Err(WorkflowContextErrorV1::Mismatch("active task"));
    }
    task_graph
        .validate_with_feature(feature, plan)
        .map_err(WorkflowContextErrorV1::Invalid)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{
        ArtifactStatusV1, ChangeContractV1, Phase, RoutineEligibilityV1, SessionWorkflow,
        WorkflowConfig, WorkflowGatesV1,
    };

    fn turn() -> WorkflowTurn {
        SessionWorkflow::restore_with_artifacts(
            WorkflowConfig::default(),
            WorkflowGatesV1::default(),
            WorkflowStateV1::default(),
            WorkflowArtifactGraphV1::default(),
        )
        .unwrap()
        .begin_turn(&[])
        .unwrap()
    }

    #[test]
    fn empty_workflow_is_request_silent() {
        let turn = turn();
        let prepared = PreparedWorkflowContextV1::capture(
            &turn,
            turn.state.clone(),
            WorkflowArtifactGraphV1::default(),
            WorkflowArtifactBodiesSnapshotV1::default(),
        )
        .unwrap();
        assert_eq!(prepared.token_cost(), 0);
        assert!(prepared.complete());
        assert!(prepared.slice().is_none());
    }

    #[test]
    fn rejects_turn_state_mismatch() {
        let state = WorkflowStateV1 {
            revision: 1,
            ..Default::default()
        };
        assert!(matches!(
            PreparedWorkflowContextV1::capture(
                &turn(),
                state,
                WorkflowArtifactGraphV1::default(),
                WorkflowArtifactBodiesSnapshotV1::default()
            ),
            Err(WorkflowContextErrorV1::TurnStateMismatch)
        ));
    }

    #[test]
    fn resumed_routine_turn_projects_the_exact_contract_body_once() {
        let draft = ChangeContractV1::draft(
            "routine-resume",
            RoutineEligibilityV1::local_component("polaris-core"),
            "Restore routine context",
            "The previous slice only carried the reference",
            "The resumed request contains the approved scope",
            vec!["scope stays bounded".into()],
        )
        .unwrap();
        let approved = draft.revised(ArtifactStatusV1::Approved).unwrap();
        let mut graph = WorkflowArtifactGraphV1::default();
        graph.insert_change_contract(&draft).unwrap();
        graph.insert_change_contract(&approved).unwrap();
        let state = WorkflowStateV1 {
            phase: Phase::Implement,
            step: Phase::Implement.default_step(),
            active_change_contract: Some(approved.reference()),
            ..WorkflowStateV1::default()
        };
        let workflow = SessionWorkflow::restore_with_artifacts(
            WorkflowConfig::default(),
            WorkflowGatesV1::default(),
            state.clone(),
            graph.clone(),
        )
        .unwrap();
        let turn = workflow.begin_turn(&[]).unwrap();
        let prepared = PreparedWorkflowContextV1::capture_with_gates(
            &turn,
            turn.state.clone(),
            graph,
            WorkflowArtifactBodiesSnapshotV1 {
                change_contract: Some(approved),
                ..WorkflowArtifactBodiesSnapshotV1::default()
            },
            &workflow.gates,
        )
        .unwrap();
        let slice = prepared.slice().unwrap();
        assert_eq!(
            slice.active_change_contract.as_ref().unwrap().scope,
            "Restore routine context"
        );
        assert!(prepared.token_cost() <= crate::workflow::WORKFLOW_CONTEXT_SLICE_TOKEN_LIMIT);
    }
}
