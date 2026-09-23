//! Pure, bounded projection of the currently active workflow task.
//!
//! Stores and providers deliberately remain outside this module. Callers pass
//! the immutable bodies captured for one real turn; missing item-5 evidence or
//! transition coverage is represented as typed incomplete data, never as an
//! empty successful slice.

use super::{
    ActiveTaskRefV1, ArtifactRefV1, ArtifactStatusV1, ChangeContractV1, EvidenceResultV1,
    FeatureSpecV1, IdeaDecisionV1, ImplementationPlanV1, Phase, ProjectPrinciplesV1, TaskGraphV1,
    TaskStateV1, VerificationCriterionV1, WorkflowArtifactGraphV1, WorkflowArtifactKindV1,
    WorkflowGatesV1, WorkflowStateV1, WorkflowStepV1, evaluate_phase_transition,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const WORKFLOW_CONTEXT_SLICE_SCHEMA_VERSION: u32 = 1;
pub const WORKFLOW_CONTEXT_SLICE_TOKEN_LIMIT: usize = 512;
const CONTEXT_ENVELOPE_RESERVE_TOKENS: usize = 128;
const MAX_CONTEXT_EVIDENCE_RECORDS: usize = 32;
const MAX_CONTEXT_OMISSIONS: usize = 16;
const MAX_CONTEXT_TEXT_BYTES: usize = 4096;
const MAX_CONTEXT_REASON_BYTES: usize = 512;

#[derive(Debug, Clone, Copy)]
pub struct WorkflowContextBodiesV1<'a> {
    pub change_contract: Option<&'a ChangeContractV1>,
    pub feature_spec: Option<&'a FeatureSpecV1>,
    pub implementation_plan: Option<&'a ImplementationPlanV1>,
    pub task_graph: Option<&'a TaskGraphV1>,
    pub project_principles: Option<&'a ProjectPrinciplesV1>,
    pub idea_decisions: &'a [IdeaDecisionV1],
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextPolicyItemV1 {
    pub id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceSummaryV1 {
    pub id: String,
    pub summary: String,
    pub artifact_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum RecentEvidenceInputV1 {
    Available {
        records: Vec<EvidenceSummaryV1>,
    },
    Incomplete {
        reason: String,
        records: Vec<EvidenceSummaryV1>,
    },
    Unavailable {
        reason: String,
    },
}

impl RecentEvidenceInputV1 {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum TransitionCoverageInputV1 {
    Available {
        evaluated: Vec<Phase>,
        permitted: Vec<Phase>,
    },
    Incomplete {
        reason: String,
        evaluated: Vec<Phase>,
        permitted: Vec<Phase>,
    },
    Unavailable {
        reason: String,
    },
}

impl TransitionCoverageInputV1 {
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::Unavailable {
            reason: reason.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveFeatureContextV1 {
    pub reference: ArtifactRefV1,
    pub title: String,
}

/// The compact routine-change alternative to a feature/plan/task chain.
/// This deliberately contains the approved body fields which define the work;
/// a reference alone would make resumed routine implementation ambiguous.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveChangeContractContextV1 {
    pub reference: ArtifactRefV1,
    pub scope: String,
    pub expected_outcome: String,
    pub criteria: Vec<CriterionContextV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveTaskContextV1 {
    pub reference: ActiveTaskRefV1,
    pub title: String,
    pub scope: Vec<String>,
    pub state: TaskStateV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequirementContextV1 {
    pub id: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CriterionContextV1 {
    pub id: String,
    pub description: String,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecentEvidenceContextV1 {
    pub state: ContextAvailabilityV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub records: Vec<EvidenceSummaryV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransitionCoverageContextV1 {
    pub state: ContextAvailabilityV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evaluated: Vec<Phase>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub permitted: Vec<Phase>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextAvailabilityV1 {
    Available,
    Incomplete,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextMissingV1 {
    ActiveChangeContractBody,
    ActiveChangeContractHashMismatch,
    ActiveFeatureRef,
    ActiveFeatureBody,
    ActiveFeatureHashMismatch,
    ActiveTaskRef,
    TaskGraphBody,
    TaskGraphHashMismatch,
    ImplementationPlanBody,
    ActiveTaskDefinitionMismatch,
    ActiveTaskInactive,
    ReferencedRequirement,
    ReferencedCriterion,
    RecentEvidence,
    TransitionCoverage,
    RequiredRecordOverBudget,
    ProjectPrinciplesBody,
    ProjectPrinciplesHashMismatch,
    UnresolvedDecisionsBody,
    UnresolvedDecisionsHashMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextOmissionV1 {
    pub kind: String,
    pub id: String,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowContextSliceV1 {
    pub schema_version: u32,
    pub phase: Phase,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<WorkflowStepV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_feature: Option<ActiveFeatureContextV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_change_contract: Option<ActiveChangeContractContextV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_task: Option<ActiveTaskContextV1>,
    pub requirements: Vec<RequirementContextV1>,
    pub criteria: Vec<CriterionContextV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub project_principles: Vec<ContextPolicyItemV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved_decisions: Vec<ContextPolicyItemV1>,
    pub recent_evidence: RecentEvidenceContextV1,
    pub permitted_transitions: TransitionCoverageContextV1,
    pub omitted: Vec<ContextOmissionV1>,
    pub missing: Vec<ContextMissingV1>,
    pub truncated: bool,
    pub complete: bool,
    pub token_budget: usize,
    pub estimated_tokens: usize,
    pub basis_hash: String,
}

/// Builds a deterministic request-only context slice from bodies captured at
/// the real-turn boundary. It performs no I/O and cannot load a newer body.
pub fn build_workflow_context_slice(
    state: &WorkflowStateV1,
    graph: &WorkflowArtifactGraphV1,
    bodies: WorkflowContextBodiesV1<'_>,
    recent_evidence: RecentEvidenceInputV1,
    transitions: TransitionCoverageInputV1,
) -> Result<WorkflowContextSliceV1, String> {
    build_with_budget(
        state,
        graph,
        bodies,
        recent_evidence,
        transitions,
        WORKFLOW_CONTEXT_SLICE_TOKEN_LIMIT,
    )
}

/// Projects the immutable runtime gate snapshot captured at a real-turn
/// boundary.  This is informational only: the phase evaluator remains the
/// authority, and a missing or stale convergence input never becomes a passed
/// evidence record here.
pub fn runtime_context_inputs(
    state: &WorkflowStateV1,
    graph: &WorkflowArtifactGraphV1,
    gates: &WorkflowGatesV1,
) -> (RecentEvidenceInputV1, TransitionCoverageInputV1) {
    let transitions = TransitionCoverageInputV1::Available {
        evaluated: Phase::ALL.to_vec(),
        permitted: Phase::ALL
            .into_iter()
            .filter(|phase| evaluate_phase_transition(state, *phase, gates, graph).is_ok())
            .collect(),
    };
    let Some(convergence) = gates.convergence.as_ref() else {
        return (
            RecentEvidenceInputV1::Incomplete {
                reason: "no current convergence evidence".into(),
                records: Vec::new(),
            },
            transitions,
        );
    };
    let expected = match current_evidence_binding(state, graph) {
        Ok(binding) => binding,
        Err(reason) => {
            return (
                RecentEvidenceInputV1::Incomplete {
                    reason,
                    records: Vec::new(),
                },
                transitions,
            );
        }
    };
    if convergence.validate().is_err() || !binding_matches_current(&convergence.binding, &expected)
    {
        return (
            RecentEvidenceInputV1::Incomplete {
                reason: "convergence evidence is stale or invalid".into(),
                records: Vec::new(),
            },
            transitions,
        );
    }
    let mut records = convergence
        .implementation_receipts
        .iter()
        .map(|receipt| EvidenceSummaryV1 {
            id: receipt.receipt_id.clone(),
            summary: "implementation receipt: passed".into(),
            artifact_hash: receipt.implementation_hash.clone(),
        })
        .chain(convergence.reviews.iter().map(|review| EvidenceSummaryV1 {
            id: review.record_id.clone(),
            summary: if review.findings.iter().any(|finding| {
                matches!(finding.disposition, super::ReviewFindingDispositionV1::Open)
            }) {
                "review: open findings".into()
            } else {
                "review: no open findings".into()
            },
            artifact_hash: review.binding.subject_hash.clone(),
        }))
        .chain(convergence.evidence.iter().map(|record| EvidenceSummaryV1 {
            id: record.record_id.clone(),
            summary: format!(
                "criterion {}: {}",
                record.criterion_id,
                evidence_result_name(record.result)
            ),
            artifact_hash: record.binding.subject_hash.clone(),
        }))
        .collect::<Vec<_>>();
    records.sort_by(|left, right| left.id.cmp(&right.id));
    if records.is_empty() || records.len() > MAX_CONTEXT_EVIDENCE_RECORDS {
        return (
            RecentEvidenceInputV1::Incomplete {
                reason: "current convergence evidence is empty or exceeds the context bound".into(),
                records: records
                    .into_iter()
                    .take(MAX_CONTEXT_EVIDENCE_RECORDS)
                    .collect(),
            },
            transitions,
        );
    }
    (RecentEvidenceInputV1::Available { records }, transitions)
}

fn current_evidence_binding(
    state: &WorkflowStateV1,
    graph: &WorkflowArtifactGraphV1,
) -> Result<super::EvidenceBindingV1, String> {
    let basis = graph.basis_hash()?;
    let (task_id, task_definition_hash) = if let Some(task) = state.active_task.as_ref() {
        graph.validate_active_task(task)?;
        (task.task_id.clone(), task.definition_hash.clone())
    } else {
        let contract = state
            .active_change_contract
            .as_ref()
            .ok_or("no active task or routine contract")?;
        graph.approved_routine_contract(contract)?;
        (contract.id.clone(), contract.content_hash.clone())
    };
    // Subject/source/diff are intentionally obtained from the persisted input;
    // this helper validates only the active identity controlled by this slice.
    // Their hashes are compared by the caller through full binding equality.
    Ok(super::EvidenceBindingV1 {
        task_id,
        task_definition_hash,
        subject_hash: String::new(),
        source_hash: String::new(),
        diff_hash: String::new(),
        artifact_graph_basis_hash: basis,
    })
}

fn binding_matches_current(
    actual: &super::EvidenceBindingV1,
    expected: &super::EvidenceBindingV1,
) -> bool {
    actual.task_id == expected.task_id
        && actual.task_definition_hash == expected.task_definition_hash
        && actual.artifact_graph_basis_hash == expected.artifact_graph_basis_hash
}

fn evidence_result_name(result: EvidenceResultV1) -> &'static str {
    match result {
        EvidenceResultV1::Passed => "passed",
        EvidenceResultV1::Failed => "failed",
        EvidenceResultV1::Stale => "stale",
        EvidenceResultV1::Unavailable => "unavailable",
    }
}

fn build_with_budget(
    state: &WorkflowStateV1,
    graph: &WorkflowArtifactGraphV1,
    bodies: WorkflowContextBodiesV1<'_>,
    recent_evidence: RecentEvidenceInputV1,
    transitions: TransitionCoverageInputV1,
    token_budget: usize,
) -> Result<WorkflowContextSliceV1, String> {
    graph.validate()?;
    let mut slice = WorkflowContextSliceV1 {
        schema_version: WORKFLOW_CONTEXT_SLICE_SCHEMA_VERSION,
        phase: state.phase,
        step: state.step,
        active_feature: None,
        active_change_contract: None,
        active_task: None,
        requirements: Vec::new(),
        criteria: Vec::new(),
        project_principles: Vec::new(),
        unresolved_decisions: Vec::new(),
        recent_evidence: RecentEvidenceContextV1 {
            state: ContextAvailabilityV1::Unavailable,
            reason: Some("evidence/convergence is not connected".into()),
            records: Vec::new(),
        },
        permitted_transitions: TransitionCoverageContextV1 {
            state: ContextAvailabilityV1::Unavailable,
            reason: Some("transition coverage is not connected".into()),
            evaluated: Vec::new(),
            permitted: Vec::new(),
        },
        omitted: Vec::new(),
        missing: Vec::new(),
        truncated: false,
        complete: false,
        token_budget,
        estimated_tokens: CONTEXT_ENVELOPE_RESERVE_TOKENS,
        basis_hash: String::new(),
    };

    if let Some(contract_ref) = state.active_change_contract.as_ref() {
        let Some(contract) = bodies.change_contract else {
            slice
                .missing
                .push(ContextMissingV1::ActiveChangeContractBody);
            return finish_slice(slice, state, graph, bodies, &recent_evidence, &transitions);
        };
        if contract.validate().is_err()
            || contract.reference() != *contract_ref
            || graph.approved_routine_contract(contract_ref).is_err()
        {
            slice
                .missing
                .push(ContextMissingV1::ActiveChangeContractHashMismatch);
            return finish_slice(slice, state, graph, bodies, &recent_evidence, &transitions);
        }
        let criteria = contract
            .verification
            .iter()
            .filter(|criterion| criterion.required)
            .map(criterion_context)
            .collect::<Vec<_>>();
        if criteria.is_empty() {
            slice.missing.push(ContextMissingV1::ReferencedCriterion);
            return finish_slice(slice, state, graph, bodies, &recent_evidence, &transitions);
        }
        let context = ActiveChangeContractContextV1 {
            reference: contract_ref.clone(),
            scope: contract.symptom_or_request.clone(),
            expected_outcome: contract.expected_outcome.clone(),
            criteria,
        };
        if !pack_required(&mut slice, &context)? {
            slice
                .missing
                .push(ContextMissingV1::RequiredRecordOverBudget);
            record_omission(&mut slice, "change_contract", &contract_ref.id, "budget")?;
            return finish_slice(slice, state, graph, bodies, &recent_evidence, &transitions);
        }
        slice.active_change_contract = Some(context);
        return finish_slice(slice, state, graph, bodies, &recent_evidence, &transitions);
    }

    let Some(feature_ref) = state.active_feature.as_ref() else {
        slice.missing.push(ContextMissingV1::ActiveFeatureRef);
        return finish_slice(slice, state, graph, bodies, &recent_evidence, &transitions);
    };
    let Some(feature) = bodies.feature_spec else {
        slice.missing.push(ContextMissingV1::ActiveFeatureBody);
        return finish_slice(slice, state, graph, bodies, &recent_evidence, &transitions);
    };
    if feature.validate().is_err()
        || feature.reference() != *feature_ref
        || graph
            .metadata(feature_ref)
            .is_none_or(|item| item.kind != WorkflowArtifactKindV1::FeatureSpec)
    {
        slice
            .missing
            .push(ContextMissingV1::ActiveFeatureHashMismatch);
        return finish_slice(slice, state, graph, bodies, &recent_evidence, &transitions);
    }
    let feature_context = ActiveFeatureContextV1 {
        reference: feature_ref.clone(),
        title: feature.title.clone(),
    };
    if !pack_required(&mut slice, &feature_context)? {
        slice
            .missing
            .push(ContextMissingV1::RequiredRecordOverBudget);
        record_omission(&mut slice, "feature", &feature_ref.id, "budget")?;
        return finish_slice(slice, state, graph, bodies, &recent_evidence, &transitions);
    }
    slice.active_feature = Some(feature_context);

    let Some(task_ref) = state.active_task.as_ref() else {
        slice.missing.push(ContextMissingV1::ActiveTaskRef);
        return finish_slice(slice, state, graph, bodies, &recent_evidence, &transitions);
    };
    let Some(task_graph) = bodies.task_graph else {
        slice.missing.push(ContextMissingV1::TaskGraphBody);
        return finish_slice(slice, state, graph, bodies, &recent_evidence, &transitions);
    };
    if task_ref.validate_against(task_graph).is_err()
        || graph
            .metadata(&task_ref.task_graph)
            .is_none_or(|item| item.kind != WorkflowArtifactKindV1::TaskGraph)
    {
        slice.missing.push(ContextMissingV1::TaskGraphHashMismatch);
        return finish_slice(slice, state, graph, bodies, &recent_evidence, &transitions);
    }
    let Some(plan) = bodies.implementation_plan else {
        slice.missing.push(ContextMissingV1::ImplementationPlanBody);
        return finish_slice(slice, state, graph, bodies, &recent_evidence, &transitions);
    };
    if task_graph.validate_with_feature(feature, plan).is_err() {
        slice
            .missing
            .push(ContextMissingV1::ActiveTaskDefinitionMismatch);
        return finish_slice(slice, state, graph, bodies, &recent_evidence, &transitions);
    }
    let Some((definition, runtime)) = task_graph.task(&task_ref.task_id) else {
        slice
            .missing
            .push(ContextMissingV1::ActiveTaskDefinitionMismatch);
        return finish_slice(slice, state, graph, bodies, &recent_evidence, &transitions);
    };
    if matches!(
        runtime.state,
        TaskStateV1::Completed | TaskStateV1::Cancelled
    ) {
        slice.missing.push(ContextMissingV1::ActiveTaskInactive);
        return finish_slice(slice, state, graph, bodies, &recent_evidence, &transitions);
    }
    let task_context = ActiveTaskContextV1 {
        reference: task_ref.clone(),
        title: definition.title.clone(),
        scope: definition.scope.clone(),
        state: runtime.state,
        blocker: runtime.blocker.clone(),
    };
    if !pack_required(&mut slice, &task_context)? {
        slice
            .missing
            .push(ContextMissingV1::RequiredRecordOverBudget);
        record_omission(&mut slice, "task", &task_ref.task_id, "budget")?;
        return finish_slice(slice, state, graph, bodies, &recent_evidence, &transitions);
    }
    slice.active_task = Some(task_context);

    let requirements = feature
        .requirements
        .iter()
        .filter(|item| definition.requirement_ids.contains(&item.id))
        .collect::<Vec<_>>();
    if requirements.len() != definition.requirement_ids.len() {
        slice.missing.push(ContextMissingV1::ReferencedRequirement);
    }
    let mut requirements = requirements;
    requirements.sort_by(|left, right| left.id.cmp(&right.id));
    for item in requirements {
        let record = RequirementContextV1 {
            id: item.id.clone(),
            description: item.description.clone(),
        };
        if pack_required(&mut slice, &record)? {
            slice.requirements.push(record);
        } else {
            slice
                .missing
                .push(ContextMissingV1::RequiredRecordOverBudget);
            record_omission(&mut slice, "requirement", &item.id, "budget")?;
        }
    }

    let criteria = feature
        .acceptance_criteria
        .iter()
        .filter(|item| definition.criterion_ids.contains(&item.id))
        .collect::<Vec<_>>();
    if criteria.len() != definition.criterion_ids.len() {
        slice.missing.push(ContextMissingV1::ReferencedCriterion);
    }
    let mut criteria = criteria;
    criteria.sort_by(|left, right| left.id.cmp(&right.id));
    for item in criteria {
        let record = criterion_context(item);
        if pack_record(&mut slice, &record)? {
            slice.criteria.push(record);
        } else {
            slice.truncated = true;
            if item.required {
                slice
                    .missing
                    .push(ContextMissingV1::RequiredRecordOverBudget);
            }
            record_omission(&mut slice, "criterion", &item.id, "budget")?;
        }
    }

    project_task_policy(
        &mut slice,
        graph,
        feature_ref,
        &task_ref.task_id,
        bodies.project_principles,
        bodies.idea_decisions,
    )?;

    finish_slice(slice, state, graph, bodies, &recent_evidence, &transitions)
}

fn finish_slice(
    mut slice: WorkflowContextSliceV1,
    state: &WorkflowStateV1,
    graph: &WorkflowArtifactGraphV1,
    bodies: WorkflowContextBodiesV1<'_>,
    recent_evidence: &RecentEvidenceInputV1,
    transitions: &TransitionCoverageInputV1,
) -> Result<WorkflowContextSliceV1, String> {
    project_evidence(&mut slice, recent_evidence)?;
    project_transitions(&mut slice, transitions)?;
    deduplicate_missing(&mut slice.missing);
    slice.complete = slice.missing.is_empty()
        && !slice.truncated
        && slice.recent_evidence.state == ContextAvailabilityV1::Available
        && slice.permitted_transitions.state == ContextAvailabilityV1::Available;
    enforce_final_budget(&mut slice, state, graph, bodies)?;
    Ok(slice)
}

/// `estimated_tokens` is only a packing aid. The request boundary must use the
/// serialized value itself: field names, omission records and hashes all count
/// too. If that final form does not fit, replace it with an intentionally
/// incomplete envelope rather than leaking an oversized partial context.
fn enforce_final_budget(
    slice: &mut WorkflowContextSliceV1,
    state: &WorkflowStateV1,
    graph: &WorkflowArtifactGraphV1,
    bodies: WorkflowContextBodiesV1<'_>,
) -> Result<(), String> {
    let serialized_tokens = |value: &WorkflowContextSliceV1| -> Result<usize, String> {
        Ok(serde_json::to_vec(value)
            .map_err(|_| "workflow context sliceを直列化できません")?
            .len()
            .div_ceil(4))
    };

    if serialized_tokens(slice)? > slice.token_budget {
        *slice = compact_failed_slice(slice.phase, slice.step, slice.token_budget);
    }

    // The count and basis hash both appear in the serialized envelope. Iterate
    // to the tiny fixed point so the final hash binds the final count rather
    // than a pre-serialization estimate.
    for _ in 0..3 {
        let actual = serialized_tokens(slice)?;
        if actual > slice.token_budget {
            return Err("workflow context sliceの最小失敗表現がtoken budgetを超えました".into());
        }
        slice.estimated_tokens = actual;
        slice.basis_hash = context_basis_hash(slice, state, graph, bodies)?;
        if serialized_tokens(slice)? == actual {
            return Ok(());
        }
    }
    Err("workflow context sliceのtoken見積りが安定しません".into())
}

fn compact_failed_slice(
    phase: Phase,
    step: Option<WorkflowStepV1>,
    token_budget: usize,
) -> WorkflowContextSliceV1 {
    WorkflowContextSliceV1 {
        schema_version: WORKFLOW_CONTEXT_SLICE_SCHEMA_VERSION,
        phase,
        step,
        active_feature: None,
        active_change_contract: None,
        active_task: None,
        requirements: Vec::new(),
        criteria: Vec::new(),
        project_principles: Vec::new(),
        unresolved_decisions: Vec::new(),
        recent_evidence: RecentEvidenceContextV1 {
            state: ContextAvailabilityV1::Unavailable,
            reason: None,
            records: Vec::new(),
        },
        permitted_transitions: TransitionCoverageContextV1 {
            state: ContextAvailabilityV1::Unavailable,
            reason: None,
            evaluated: Vec::new(),
            permitted: Vec::new(),
        },
        omitted: Vec::new(),
        missing: vec![
            ContextMissingV1::RequiredRecordOverBudget,
            ContextMissingV1::RecentEvidence,
            ContextMissingV1::TransitionCoverage,
        ],
        truncated: true,
        complete: false,
        token_budget,
        estimated_tokens: 0,
        basis_hash: String::new(),
    }
}

fn context_basis_hash(
    slice: &WorkflowContextSliceV1,
    state: &WorkflowStateV1,
    graph: &WorkflowArtifactGraphV1,
    bodies: WorkflowContextBodiesV1<'_>,
) -> Result<String, String> {
    let body_basis = (
        bodies.change_contract.map(ChangeContractV1::reference),
        bodies.feature_spec.map(FeatureSpecV1::reference),
        bodies
            .implementation_plan
            .map(ImplementationPlanV1::reference),
        bodies.task_graph.map(TaskGraphV1::reference),
        bodies
            .project_principles
            .map(ProjectPrinciplesV1::reference),
        bodies
            .idea_decisions
            .iter()
            .map(IdeaDecisionV1::reference)
            .collect::<Vec<_>>(),
    );
    let mut canonical = slice.clone();
    canonical.basis_hash.clear();
    let bytes = serde_json::to_vec(&(
        "polaris.workflow.context_slice.v1",
        state,
        graph.basis_hash()?,
        body_basis,
        canonical,
    ))
    .map_err(|_| "workflow context sliceを正規化できません")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn project_evidence(
    slice: &mut WorkflowContextSliceV1,
    input: &RecentEvidenceInputV1,
) -> Result<(), String> {
    let (state, reason, records) = match input {
        RecentEvidenceInputV1::Available { records } => {
            (ContextAvailabilityV1::Available, None, records.as_slice())
        }
        RecentEvidenceInputV1::Incomplete { reason, records } => {
            validate_reason(reason)?;
            let reason = packed_reason(slice, reason, "evidence_status")?;
            (
                ContextAvailabilityV1::Incomplete,
                reason,
                records.as_slice(),
            )
        }
        RecentEvidenceInputV1::Unavailable { reason } => {
            validate_reason(reason)?;
            let reason = packed_reason(slice, reason, "evidence_status")?;
            slice.missing.push(ContextMissingV1::RecentEvidence);
            slice.recent_evidence = RecentEvidenceContextV1 {
                state: ContextAvailabilityV1::Unavailable,
                reason,
                records: Vec::new(),
            };
            return Ok(());
        }
    };
    if records.is_empty() || records.len() > MAX_CONTEXT_EVIDENCE_RECORDS {
        slice.missing.push(ContextMissingV1::RecentEvidence);
    }
    if state != ContextAvailabilityV1::Available {
        slice.missing.push(ContextMissingV1::RecentEvidence);
    }
    let mut sorted = records.to_vec();
    sorted.sort_by(|left, right| left.id.cmp(&right.id));
    let mut seen = BTreeSet::new();
    let mut packed = Vec::new();
    for record in sorted {
        validate_context_text(&record.id)?;
        validate_context_text(&record.summary)?;
        super::validate_hash(&record.artifact_hash)?;
        if !seen.insert(record.id.clone()) {
            return Err("evidence IDが重複しています".into());
        }
        if pack_record(slice, &record)? {
            packed.push(record);
        } else {
            slice.truncated = true;
            record_omission(slice, "evidence", &record.id, "budget")?;
        }
    }
    if packed.len() != records.len() {
        slice.missing.push(ContextMissingV1::RecentEvidence);
    }
    slice.recent_evidence = RecentEvidenceContextV1 {
        state,
        reason,
        records: packed,
    };
    Ok(())
}

fn project_transitions(
    slice: &mut WorkflowContextSliceV1,
    input: &TransitionCoverageInputV1,
) -> Result<(), String> {
    let (state, reason, mut evaluated, mut permitted) = match input {
        TransitionCoverageInputV1::Available {
            evaluated,
            permitted,
        } => (
            ContextAvailabilityV1::Available,
            None,
            evaluated.clone(),
            permitted.clone(),
        ),
        TransitionCoverageInputV1::Incomplete {
            reason,
            evaluated,
            permitted,
        } => {
            validate_reason(reason)?;
            let reason = packed_reason(slice, reason, "transition_status")?;
            (
                ContextAvailabilityV1::Incomplete,
                reason,
                evaluated.clone(),
                permitted.clone(),
            )
        }
        TransitionCoverageInputV1::Unavailable { reason } => {
            validate_reason(reason)?;
            let reason = packed_reason(slice, reason, "transition_status")?;
            slice.missing.push(ContextMissingV1::TransitionCoverage);
            slice.permitted_transitions = TransitionCoverageContextV1 {
                state: ContextAvailabilityV1::Unavailable,
                reason,
                evaluated: Vec::new(),
                permitted: Vec::new(),
            };
            return Ok(());
        }
    };
    evaluated.sort();
    evaluated.dedup();
    permitted.sort();
    permitted.dedup();
    if !pack_record(slice, &(&evaluated, &permitted))? {
        slice.truncated = true;
        slice.missing.push(ContextMissingV1::TransitionCoverage);
        record_omission(slice, "transition_coverage", "phase_matrix", "budget")?;
        evaluated.clear();
        permitted.clear();
    }
    let expected = Phase::ALL.into_iter().collect::<BTreeSet<_>>();
    let actual = evaluated.iter().copied().collect::<BTreeSet<_>>();
    if state != ContextAvailabilityV1::Available
        || actual != expected
        || permitted.iter().any(|phase| !actual.contains(phase))
    {
        slice.missing.push(ContextMissingV1::TransitionCoverage);
    }
    slice.permitted_transitions = TransitionCoverageContextV1 {
        state,
        reason,
        evaluated,
        permitted,
    };
    Ok(())
}

fn criterion_context(item: &VerificationCriterionV1) -> CriterionContextV1 {
    CriterionContextV1 {
        id: item.id.clone(),
        description: item.description.clone(),
        required: item.required,
    }
}

/// The only policy bodies accepted here are exact, validated bodies already
/// present in the immutable graph. No policy is inferred from task prose.
fn project_task_policy(
    slice: &mut WorkflowContextSliceV1,
    graph: &WorkflowArtifactGraphV1,
    feature: &ArtifactRefV1,
    task_id: &str,
    principles: Option<&ProjectPrinciplesV1>,
    decisions: &[IdeaDecisionV1],
) -> Result<(), String> {
    let graph_principles = graph
        .artifacts
        .iter()
        .filter(|item| {
            item.kind == WorkflowArtifactKindV1::ProjectPrinciples
                && item.header.status == ArtifactStatusV1::Approved
        })
        .collect::<Vec<_>>();
    if graph_principles.is_empty() {
        // No project policy artifact is an explicit zero, not a missing body.
    } else {
        let Some(principles) = principles else {
            slice.missing.push(ContextMissingV1::ProjectPrinciplesBody);
            return project_unresolved_decisions(slice, graph, feature, decisions);
        };
        if principles.validate().is_err()
            || graph
                .metadata(&principles.reference())
                .is_none_or(|item| item.kind != WorkflowArtifactKindV1::ProjectPrinciples)
        {
            slice
                .missing
                .push(ContextMissingV1::ProjectPrinciplesHashMismatch);
        } else {
            for principle in principles.applicable_to(Some(&feature.id), Some(task_id)) {
                let item = ContextPolicyItemV1 {
                    id: principle.id.clone(),
                    text: principle.statement.clone(),
                };
                pack_policy_item(slice, "project_principle", item, |slice, item| {
                    slice.project_principles.push(item)
                })?;
            }
        }
    }
    project_unresolved_decisions(slice, graph, feature, decisions)
}

fn project_unresolved_decisions(
    slice: &mut WorkflowContextSliceV1,
    graph: &WorkflowArtifactGraphV1,
    feature: &ArtifactRefV1,
    decisions: &[IdeaDecisionV1],
) -> Result<(), String> {
    let expected = graph
        .metadata(feature)
        .map(|item| {
            item.header
                .created_from
                .iter()
                .filter(|reference| {
                    graph
                        .metadata(reference)
                        .is_some_and(|source| source.kind == WorkflowArtifactKindV1::IdeaDecision)
                })
                .cloned()
                .collect::<BTreeSet<_>>()
        })
        .unwrap_or_default();
    let mut supplied = BTreeSet::new();
    for decision in decisions {
        if decision.validate().is_err()
            || graph
                .metadata(&decision.reference())
                .is_none_or(|item| item.kind != WorkflowArtifactKindV1::IdeaDecision)
        {
            slice
                .missing
                .push(ContextMissingV1::UnresolvedDecisionsHashMismatch);
            continue;
        }
        if expected.contains(&decision.reference()) {
            supplied.insert(decision.reference());
        }
        if decision.is_unresolved() && expected.contains(&decision.reference()) {
            let entries = if decision.unresolved.is_empty() {
                vec![ContextPolicyItemV1 {
                    id: decision.header.id.clone(),
                    text: decision.problem.clone(),
                }]
            } else {
                decision
                    .unresolved
                    .iter()
                    .map(|item| ContextPolicyItemV1 {
                        id: item.id.clone(),
                        text: item.question.clone(),
                    })
                    .collect()
            };
            for item in entries {
                pack_policy_item(slice, "unresolved_decision", item, |slice, item| {
                    slice.unresolved_decisions.push(item)
                })?;
            }
        }
    }
    if supplied != expected {
        slice
            .missing
            .push(ContextMissingV1::UnresolvedDecisionsBody);
    }
    Ok(())
}

fn pack_policy_item<F>(
    slice: &mut WorkflowContextSliceV1,
    kind: &str,
    item: ContextPolicyItemV1,
    mut push: F,
) -> Result<(), String>
where
    F: FnMut(&mut WorkflowContextSliceV1, ContextPolicyItemV1),
{
    if pack_record(slice, &item)? {
        push(slice, item);
    } else {
        slice.truncated = true;
        slice
            .missing
            .push(ContextMissingV1::RequiredRecordOverBudget);
        record_omission(slice, kind, &item.id, "budget")?;
    }
    Ok(())
}

fn pack_required<T: Serialize>(
    slice: &mut WorkflowContextSliceV1,
    record: &T,
) -> Result<bool, String> {
    let packed = pack_record(slice, record)?;
    if !packed {
        slice.truncated = true;
    }
    Ok(packed)
}

fn pack_record<T: Serialize>(
    slice: &mut WorkflowContextSliceV1,
    record: &T,
) -> Result<bool, String> {
    let bytes = serde_json::to_vec(record).map_err(|_| "context recordを正規化できません")?;
    let tokens = bytes.len().div_ceil(4);
    if slice.estimated_tokens.saturating_add(tokens) > slice.token_budget {
        return Ok(false);
    }
    slice.estimated_tokens += tokens;
    Ok(true)
}

fn omission(kind: &str, id: &str, reason: &str) -> ContextOmissionV1 {
    ContextOmissionV1 {
        kind: kind.into(),
        id: id.into(),
        reason: reason.into(),
    }
}

/// Omission metadata is itself request context. Keep it bounded and account
/// for it exactly like a projected record so a long list of absent items
/// cannot defeat the slice budget.
fn record_omission(
    slice: &mut WorkflowContextSliceV1,
    kind: &str,
    id: &str,
    reason: &str,
) -> Result<(), String> {
    slice.truncated = true;
    if slice.omitted.len() >= MAX_CONTEXT_OMISSIONS {
        return Ok(());
    }
    let record = omission(kind, id, reason);
    if pack_record(slice, &record)? {
        slice.omitted.push(record);
    }
    Ok(())
}

fn validate_context_text(value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > MAX_CONTEXT_TEXT_BYTES || value.contains('\0') {
        Err("context textが空または上限超過です".into())
    } else {
        Ok(())
    }
}

fn validate_reason(value: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > MAX_CONTEXT_REASON_BYTES || value.contains('\0') {
        Err("context status reasonが空または上限超過です".into())
    } else {
        Ok(())
    }
}

fn packed_reason(
    slice: &mut WorkflowContextSliceV1,
    reason: &str,
    kind: &str,
) -> Result<Option<String>, String> {
    if pack_record(slice, &reason)? {
        Ok(Some(reason.into()))
    } else {
        record_omission(slice, kind, "reason", "budget")?;
        Ok(None)
    }
}

fn deduplicate_missing(missing: &mut Vec<ContextMissingV1>) {
    let mut seen = BTreeSet::new();
    missing.retain(|item| seen.insert(format!("{item:?}")));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{
        ArtifactStatusV1, ChangeContractV1, EvidenceBindingV1, EvidenceProvenanceV1,
        EvidenceRecordV1, EvidenceResultV1, EvidenceSourceKindV1, ProjectPrincipleV1,
        RequirementV1, RoutineEligibilityV1, TaskDefinitionV1, TaskRuntimeV1,
        VerificationCriterionV1, WorkflowArtifactGraphV1, WorkflowGatesV1,
    };

    struct Fixture {
        state: WorkflowStateV1,
        graph: WorkflowArtifactGraphV1,
        feature: FeatureSpecV1,
        plan: ImplementationPlanV1,
        tasks: TaskGraphV1,
    }

    fn fixture() -> Fixture {
        let requirement = RequirementV1::stable("Only the active task is projected").unwrap();
        let criterion = VerificationCriterionV1::stable("The slice is bounded", true).unwrap();
        let feature = FeatureSpecV1::draft(
            "feature-context",
            "Context slice",
            "Project the active task only",
            vec![requirement.clone()],
            vec![criterion.clone()],
        )
        .unwrap()
        .revised(ArtifactStatusV1::Approved)
        .unwrap();
        let plan = ImplementationPlanV1::draft(
            "plan-context",
            feature.reference(),
            "Use immutable snapshots",
            vec!["workflow core".into()],
        )
        .unwrap()
        .revised(ArtifactStatusV1::Approved)
        .unwrap();
        let active_id = TaskDefinitionV1::stable_id("active context task").unwrap();
        let completed_id = TaskDefinitionV1::stable_id("completed unrelated task").unwrap();
        let tasks = TaskGraphV1::draft(
            "tasks-context",
            feature.reference(),
            plan.reference(),
            vec![
                TaskDefinitionV1 {
                    id: active_id.clone(),
                    title: "Build active slice".into(),
                    scope: vec!["workflow/context.rs".into()],
                    requirement_ids: vec![requirement.id.clone()],
                    criterion_ids: vec![criterion.id.clone()],
                    dependencies: Vec::new(),
                },
                TaskDefinitionV1 {
                    id: completed_id.clone(),
                    title: "Old work".into(),
                    scope: vec!["old.rs".into()],
                    requirement_ids: Vec::new(),
                    criterion_ids: Vec::new(),
                    dependencies: Vec::new(),
                },
            ],
            vec![
                TaskRuntimeV1 {
                    task_id: active_id.clone(),
                    state: TaskStateV1::InProgress,
                    blocker: None,
                },
                TaskRuntimeV1 {
                    task_id: completed_id,
                    state: TaskStateV1::Completed,
                    blocker: None,
                },
            ],
        )
        .unwrap();
        let mut graph = WorkflowArtifactGraphV1::default();
        let feature_draft = FeatureSpecV1::draft(
            "feature-context",
            "Context slice",
            "Project the active task only",
            vec![requirement],
            vec![criterion],
        )
        .unwrap();
        graph.insert_feature_spec(&feature_draft).unwrap();
        graph.insert_feature_spec(&feature).unwrap();
        let plan_draft = ImplementationPlanV1::draft(
            "plan-context",
            feature.reference(),
            "Use immutable snapshots",
            vec!["workflow core".into()],
        )
        .unwrap();
        // Plans may cite an approved feature revision; its own revision chain
        // remains independent.
        graph.insert_implementation_plan(&plan_draft).unwrap();
        graph.insert_implementation_plan(&plan).unwrap();
        graph.insert_task_graph(&tasks).unwrap();
        let state = WorkflowStateV1 {
            active_feature: Some(feature.reference()),
            active_task: Some(ActiveTaskRefV1 {
                task_graph: tasks.reference(),
                task_id: active_id.clone(),
                definition_hash: tasks.definition_hash.clone(),
            }),
            ..WorkflowStateV1::default()
        };
        Fixture {
            state,
            graph,
            feature,
            plan,
            tasks,
        }
    }

    fn bodies(fixture: &Fixture) -> WorkflowContextBodiesV1<'_> {
        WorkflowContextBodiesV1 {
            change_contract: None,
            feature_spec: Some(&fixture.feature),
            implementation_plan: Some(&fixture.plan),
            task_graph: Some(&fixture.tasks),
            project_principles: None,
            idea_decisions: &[],
        }
    }

    fn approved_privacy_principles(graph: &mut WorkflowArtifactGraphV1) -> ProjectPrinciplesV1 {
        let draft = ProjectPrinciplesV1::draft(
            "project-principles",
            vec![ProjectPrincipleV1 {
                id: "privacy".into(),
                statement: "Never expose private source text".into(),
                feature_ids: Vec::new(),
                task_ids: Vec::new(),
            }],
        )
        .unwrap();
        let approved = draft.revised(ArtifactStatusV1::Approved).unwrap();
        graph.insert_project_principles(&draft).unwrap();
        graph.insert_project_principles(&approved).unwrap();
        approved
    }

    fn evidence() -> RecentEvidenceInputV1 {
        RecentEvidenceInputV1::Available {
            records: vec![EvidenceSummaryV1 {
                id: "evidence-1".into(),
                summary: "focused tests passed".into(),
                artifact_hash: "a".repeat(64),
            }],
        }
    }

    fn transitions() -> TransitionCoverageInputV1 {
        TransitionCoverageInputV1::Available {
            evaluated: Phase::ALL.to_vec(),
            permitted: vec![Phase::Brainstorm, Phase::Specify],
        }
    }

    fn routine_fixture() -> (WorkflowStateV1, WorkflowArtifactGraphV1, ChangeContractV1) {
        let draft = ChangeContractV1::draft(
            "routine-context",
            RoutineEligibilityV1::local_component("polaris-core"),
            "A routine projection is missing its body",
            "The context only handled feature/task artifacts",
            "The exact routine scope and criteria are visible",
            vec!["routine context is bounded".into()],
        )
        .unwrap();
        let approved = draft.revised(ArtifactStatusV1::Approved).unwrap();
        let mut graph = WorkflowArtifactGraphV1::default();
        graph.insert_change_contract(&draft).unwrap();
        graph.insert_change_contract(&approved).unwrap();
        (
            WorkflowStateV1 {
                phase: Phase::Implement,
                step: Phase::Implement.default_step(),
                active_change_contract: Some(approved.reference()),
                ..WorkflowStateV1::default()
            },
            graph,
            approved,
        )
    }

    fn current_convergence(
        state: &WorkflowStateV1,
        graph: &WorkflowArtifactGraphV1,
        criterion_id: String,
    ) -> super::super::ConvergenceInputV1 {
        let contract = state.active_change_contract.as_ref().unwrap();
        let binding = EvidenceBindingV1 {
            task_id: contract.id.clone(),
            task_definition_hash: contract.content_hash.clone(),
            subject_hash: "a".repeat(64),
            source_hash: "b".repeat(64),
            diff_hash: "c".repeat(64),
            artifact_graph_basis_hash: graph.basis_hash().unwrap(),
        };
        super::super::ConvergenceInputV1 {
            schema_version: super::super::EVIDENCE_SCHEMA_VERSION,
            binding: binding.clone(),
            required_criterion_ids: vec![criterion_id.clone()],
            implementation_receipts: Vec::new(),
            reviews: Vec::new(),
            evidence: vec![EvidenceRecordV1 {
                schema_version: super::super::EVIDENCE_SCHEMA_VERSION,
                record_id: "evidence-routine".into(),
                binding,
                criterion_id,
                result: EvidenceResultV1::Passed,
                provenance: EvidenceProvenanceV1 {
                    source_kind: EvidenceSourceKindV1::TestExecution,
                    source_ref: "context-test".into(),
                    observed_at_unix_secs: 1,
                },
            }],
        }
    }

    #[test]
    fn projects_only_the_active_task_and_its_references() {
        let fixture = fixture();
        let slice = build_workflow_context_slice(
            &fixture.state,
            &fixture.graph,
            bodies(&fixture),
            evidence(),
            transitions(),
        )
        .unwrap();
        assert!(slice.complete);
        assert_eq!(slice.requirements.len(), 1);
        assert_eq!(slice.criteria.len(), 1);
        assert_eq!(
            slice.active_task.as_ref().unwrap().scope,
            vec!["workflow/context.rs"]
        );
        assert_eq!(
            slice.active_task.as_ref().unwrap().title,
            "Build active slice"
        );
        let encoded = serde_json::to_string(&slice).unwrap();
        assert!(!encoded.contains("Old work"));
        assert!(!encoded.contains("old.rs"));
        assert!(slice.estimated_tokens <= WORKFLOW_CONTEXT_SLICE_TOKEN_LIMIT);
        assert!(
            serde_json::to_vec(&slice).unwrap().len().div_ceil(4)
                <= WORKFLOW_CONTEXT_SLICE_TOKEN_LIMIT
        );
    }

    #[test]
    fn missing_item_five_inputs_are_typed_and_fail_closed() {
        let fixture = fixture();
        let slice = build_workflow_context_slice(
            &fixture.state,
            &fixture.graph,
            bodies(&fixture),
            RecentEvidenceInputV1::unavailable("item 5 is not connected"),
            TransitionCoverageInputV1::unavailable("item 5 is not connected"),
        )
        .unwrap();
        assert!(!slice.complete);
        assert!(slice.missing.contains(&ContextMissingV1::RecentEvidence));
        assert!(
            slice
                .missing
                .contains(&ContextMissingV1::TransitionCoverage)
        );
    }

    #[test]
    fn exact_definition_hash_mismatch_is_not_a_partial_success() {
        let fixture = fixture();
        let mut state = fixture.state.clone();
        state.active_task.as_mut().unwrap().definition_hash = "f".repeat(64);
        let slice = build_workflow_context_slice(
            &state,
            &fixture.graph,
            bodies(&fixture),
            evidence(),
            transitions(),
        )
        .unwrap();
        assert!(!slice.complete);
        assert!(
            slice
                .missing
                .contains(&ContextMissingV1::TaskGraphHashMismatch)
        );
    }

    #[test]
    fn required_records_are_packed_whole_and_overflow_fails_closed() {
        let fixture = fixture();
        let slice = build_with_budget(
            &fixture.state,
            &fixture.graph,
            bodies(&fixture),
            evidence(),
            transitions(),
            CONTEXT_ENVELOPE_RESERVE_TOKENS + 1,
        )
        .unwrap();
        assert!(!slice.complete);
        assert!(slice.truncated);
        assert!(
            slice
                .missing
                .contains(&ContextMissingV1::RequiredRecordOverBudget)
        );
        assert!(slice.active_feature.is_none());
        assert!(serde_json::to_vec(&slice).unwrap().len().div_ceil(4) <= slice.token_budget);
    }

    #[test]
    fn omission_metadata_is_bounded_and_budgeted() {
        let mut slice = compact_failed_slice(Phase::Implement, None, 512);
        for index in 0..(MAX_CONTEXT_OMISSIONS * 4) {
            record_omission(
                &mut slice,
                "requirement",
                &format!("requirement-{index}"),
                "budget",
            )
            .unwrap();
        }
        assert!(slice.omitted.len() <= MAX_CONTEXT_OMISSIONS);
        assert!(slice.estimated_tokens <= slice.token_budget);
    }

    #[test]
    fn privacy_principle_is_projected_from_exact_project_artifact_after_resume() {
        let mut fixture = fixture();
        let principles = approved_privacy_principles(&mut fixture.graph);
        let slice = build_workflow_context_slice(
            &fixture.state,
            &fixture.graph,
            WorkflowContextBodiesV1 {
                change_contract: None,
                feature_spec: Some(&fixture.feature),
                implementation_plan: Some(&fixture.plan),
                task_graph: Some(&fixture.tasks),
                project_principles: Some(&principles),
                idea_decisions: &[],
            },
            evidence(),
            transitions(),
        )
        .unwrap();
        assert!(
            slice
                .project_principles
                .iter()
                .any(|item| item.id == "privacy" && item.text.contains("private"))
        );
        assert_eq!(
            slice.active_task.as_ref().unwrap().scope,
            vec!["workflow/context.rs"]
        );
    }

    #[test]
    fn policy_over_budget_is_explicitly_incomplete() {
        let mut fixture = fixture();
        let principles = approved_privacy_principles(&mut fixture.graph);
        let slice = build_with_budget(
            &fixture.state,
            &fixture.graph,
            WorkflowContextBodiesV1 {
                change_contract: None,
                feature_spec: Some(&fixture.feature),
                implementation_plan: Some(&fixture.plan),
                task_graph: Some(&fixture.tasks),
                project_principles: Some(&principles),
                idea_decisions: &[],
            },
            evidence(),
            transitions(),
            CONTEXT_ENVELOPE_RESERVE_TOKENS + 80,
        )
        .unwrap();
        assert!(!slice.complete);
        assert!(slice.truncated);
        assert!(
            slice
                .missing
                .contains(&ContextMissingV1::RequiredRecordOverBudget)
        );
        assert!(serde_json::to_vec(&slice).unwrap().len().div_ceil(4) <= slice.token_budget);
    }

    #[test]
    fn routine_contract_is_an_exact_bounded_active_context_alternative() {
        let (state, graph, contract) = routine_fixture();
        let criterion = contract.verification[0].id.clone();
        let convergence = current_convergence(&state, &graph, criterion);
        let gates = WorkflowGatesV1 {
            convergence: Some(convergence),
            ..WorkflowGatesV1::default()
        };
        let (recent, coverage) = runtime_context_inputs(&state, &graph, &gates);
        let slice = build_workflow_context_slice(
            &state,
            &graph,
            WorkflowContextBodiesV1 {
                change_contract: Some(&contract),
                feature_spec: None,
                implementation_plan: None,
                task_graph: None,
                project_principles: None,
                idea_decisions: &[],
            },
            recent,
            coverage,
        )
        .unwrap();
        let active = slice.active_change_contract.as_ref().unwrap();
        assert_eq!(active.reference, contract.reference());
        assert_eq!(active.expected_outcome, contract.expected_outcome);
        assert!(slice.active_feature.is_none());
        assert!(slice.estimated_tokens <= WORKFLOW_CONTEXT_SLICE_TOKEN_LIMIT);
        assert!(serde_json::to_vec(&slice).unwrap().len().div_ceil(4) <= 512);
    }

    #[test]
    fn routine_missing_or_tampered_body_fails_closed() {
        let (state, graph, contract) = routine_fixture();
        let missing = build_workflow_context_slice(
            &state,
            &graph,
            WorkflowContextBodiesV1 {
                change_contract: None,
                feature_spec: None,
                implementation_plan: None,
                task_graph: None,
                project_principles: None,
                idea_decisions: &[],
            },
            evidence(),
            transitions(),
        )
        .unwrap();
        assert!(
            missing
                .missing
                .contains(&ContextMissingV1::ActiveChangeContractBody)
        );
        let mut tampered = contract.clone();
        tampered.expected_outcome.push('!');
        let mismatched = build_workflow_context_slice(
            &state,
            &graph,
            WorkflowContextBodiesV1 {
                change_contract: Some(&tampered),
                feature_spec: None,
                implementation_plan: None,
                task_graph: None,
                project_principles: None,
                idea_decisions: &[],
            },
            evidence(),
            transitions(),
        )
        .unwrap();
        assert!(
            mismatched
                .missing
                .contains(&ContextMissingV1::ActiveChangeContractHashMismatch)
        );
    }

    #[test]
    fn runtime_projection_marks_stale_evidence_incomplete_and_covers_all_phases() {
        let (state, graph, contract) = routine_fixture();
        let mut convergence =
            current_convergence(&state, &graph, contract.verification[0].id.clone());
        convergence.binding.task_definition_hash = "f".repeat(64);
        let gates = WorkflowGatesV1 {
            convergence: Some(convergence),
            ..WorkflowGatesV1::default()
        };
        let (recent, coverage) = runtime_context_inputs(&state, &graph, &gates);
        assert!(matches!(recent, RecentEvidenceInputV1::Incomplete { .. }));
        let TransitionCoverageInputV1::Available {
            evaluated,
            permitted,
        } = coverage
        else {
            panic!("phase coverage must be evaluated from the frozen gate snapshot");
        };
        assert_eq!(evaluated, Phase::ALL.to_vec());
        assert!(permitted.contains(&Phase::Implement));
        assert!(!permitted.contains(&Phase::Review));
    }
}
