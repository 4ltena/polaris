//! Explicit workflow focus, independent of tool permissions and completion evidence.

use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, fmt, str::FromStr};

mod artifacts;
mod context;
mod evidence;
pub use artifacts::*;
pub use context::*;
pub use evidence::*;

pub const WORKFLOW_SCHEMA_VERSION: u32 = 1;
pub const WORKFLOW_TOKEN_LIMIT: usize = 384;

/// Selects whether workflow focus can be proposed by the model.  Manual is
/// deliberately the default so existing CLI and TUI phase controls retain
/// their established behaviour.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowMode {
    #[default]
    Manual,
    Adaptive,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    #[default]
    General,
    Brainstorm,
    Specify,
    Implement,
    Review,
    Verify,
    Deliver,
}

impl Phase {
    pub const ALL: [Self; 7] = [
        Self::General,
        Self::Brainstorm,
        Self::Specify,
        Self::Implement,
        Self::Review,
        Self::Verify,
        Self::Deliver,
    ];
    pub fn as_str(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Brainstorm => "brainstorm",
            Self::Specify => "specify",
            Self::Implement => "implement",
            Self::Review => "review",
            Self::Verify => "verify",
            Self::Deliver => "deliver",
        }
    }

    pub fn default_step(self) -> Option<WorkflowStepV1> {
        match self {
            Self::General => None,
            Self::Brainstorm => Some(WorkflowStepV1::Intake),
            Self::Specify => Some(WorkflowStepV1::Specify),
            Self::Implement => Some(WorkflowStepV1::Implement),
            Self::Review => Some(WorkflowStepV1::Review),
            Self::Verify => Some(WorkflowStepV1::Verify),
            Self::Deliver => Some(WorkflowStepV1::Deliver),
        }
    }

    pub fn accepts_step(self, step: WorkflowStepV1) -> bool {
        matches!(
            (self, step),
            (
                Self::Brainstorm,
                WorkflowStepV1::Intake
                    | WorkflowStepV1::Research
                    | WorkflowStepV1::Define
                    | WorkflowStepV1::Shape
                    | WorkflowStepV1::Decide
            ) | (
                Self::Specify,
                WorkflowStepV1::Specify
                    | WorkflowStepV1::Clarify
                    | WorkflowStepV1::Plan
                    | WorkflowStepV1::Decompose
                    | WorkflowStepV1::Analyze
            ) | (Self::Implement, WorkflowStepV1::Implement)
                | (Self::Review, WorkflowStepV1::Review)
                | (
                    Self::Verify,
                    WorkflowStepV1::Verify | WorkflowStepV1::Converge
                )
                | (Self::Deliver, WorkflowStepV1::Deliver)
        )
    }
}
impl fmt::Display for Phase {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}
impl FromStr for Phase {
    type Err = String;
    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Self::ALL
            .into_iter()
            .find(|phase| phase.as_str() == value)
            .ok_or_else(|| format!("不明な作業段階です: {value}"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStepV1 {
    Intake,
    Research,
    Define,
    Shape,
    Decide,
    Specify,
    Clarify,
    Plan,
    Decompose,
    Analyze,
    Implement,
    Review,
    Verify,
    Converge,
    Deliver,
}

impl WorkflowStepV1 {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Intake => "intake",
            Self::Research => "research",
            Self::Define => "define",
            Self::Shape => "shape",
            Self::Decide => "decide",
            Self::Specify => "specify",
            Self::Clarify => "clarify",
            Self::Plan => "plan",
            Self::Decompose => "decompose",
            Self::Analyze => "analyze",
            Self::Implement => "implement",
            Self::Review => "review",
            Self::Verify => "verify",
            Self::Converge => "converge",
            Self::Deliver => "deliver",
        }
    }
}

impl fmt::Display for WorkflowStepV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowConfig {
    pub enabled: bool,
    #[serde(default)]
    pub mode: WorkflowMode,
    pub initial_phase: Phase,
    pub always: Vec<String>,
    pub phase_skills: BTreeMap<Phase, Vec<String>>,
}

/// The fixed skill body selected for one provider request, paired with the
/// controller state observed at that request boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowProfile {
    pub state: WorkflowStateV1,
    pub body: String,
    pub ids: Vec<String>,
    pub tokens: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowStateV1 {
    pub schema_version: u32,
    pub phase: Phase,
    /// Missing in pre-M27 sessions. It is never inferred while restoring an
    /// old marker; a later explicit phase/step transition writes it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<WorkflowStepV1>,
    pub revision: u64,
    pub skill_manifest_hash: String,
    pub skills: Vec<WorkflowSkillV1>,
    /// The only artifact pointer kept in the compact workflow snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_change_contract: Option<ArtifactRefV1>,
    /// Exact immutable feature revision selected for request-only context.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_feature: Option<ArtifactRefV1>,
    /// Exact task graph revision, task ID and definition hash. Runtime state is
    /// intentionally loaded from the matching immutable task-graph snapshot.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_task: Option<ActiveTaskRefV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSkillV1 {
    pub id: String,
    pub source: String,
    pub body_hash: String,
}
impl Default for WorkflowStateV1 {
    fn default() -> Self {
        Self {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            phase: Phase::General,
            step: None,
            revision: 0,
            skill_manifest_hash: String::new(),
            skills: Vec::new(),
            active_change_contract: None,
            active_feature: None,
            active_task: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationEvidence {
    pub artifact_hash: String,
    pub passed: bool,
    pub stale: bool,
}

/// Audit evidence only. A matching entry cannot bypass the existing Gate/sandbox.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalGrant {
    pub operation: String,
    pub target: String,
    pub artifact_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowGatesV1 {
    pub schema_version: u32,
    /// Absent in legacy sessions. New sessions use the typed M27 evaluator;
    /// old sessions retain their phase behaviour until explicitly migrated.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub phase_gate_schema_version: Option<u32>,
    pub approved_spec_hash: Option<String>,
    pub verification: Option<VerificationEvidence>,
    /// Host-owned, hash-bound records used for the three evidence-bearing
    /// phase edges. Model text cannot set this field; stores publish it only
    /// through their trusted evidence operation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub convergence: Option<ConvergenceInputV1>,
    /// Trusted result of validating the exact full-spec bodies. The compact
    /// phase gate accepts it only while every bound reference and graph basis
    /// remains current.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub full_spec: Option<FullSpecGateProofV1>,
    pub external_grants: Vec<ExternalGrant>,
}
impl Default for WorkflowGatesV1 {
    fn default() -> Self {
        Self {
            schema_version: WORKFLOW_SCHEMA_VERSION,
            phase_gate_schema_version: Some(WORKFLOW_ARTIFACT_SCHEMA_VERSION),
            approved_spec_hash: None,
            verification: None,
            convergence: None,
            full_spec: None,
            external_grants: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransitionBlockerCodeV1 {
    InvalidPhaseStep,
    ForwardPhaseSkipped,
    MissingChangeContract,
    ChangeContractNotCurrent,
    ChangeRequiresFullSpecification,
    MissingEvidence,
    EvidenceNotCurrent,
    ConflictingChangePaths,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TransitionBlockerV1 {
    pub schema_version: u32,
    pub code: TransitionBlockerCodeV1,
    pub from: Phase,
    pub to: Phase,
    pub missing_refs: Vec<String>,
    pub stale_refs: Vec<ArtifactRefV1>,
    pub next_task: String,
}

impl fmt::Display for TransitionBlockerV1 {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "phase gate: {:?}; 次の作業: {}",
            self.code, self.next_task
        )
    }
}

impl std::error::Error for TransitionBlockerV1 {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TransitionGateProofV1 {
    pub basis_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FullSpecGateProofV1 {
    pub schema_version: u32,
    pub artifact_graph_basis_hash: String,
    pub feature_spec: ArtifactRefV1,
    pub implementation_plan: ArtifactRefV1,
    pub task_graph: ArtifactRefV1,
    pub task_id: String,
    pub task_definition_hash: String,
    pub decision_refs: Vec<ArtifactRefV1>,
    pub basis_hash: String,
    #[serde(default)]
    pub stale: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActiveChangePathV1 {
    None,
    Routine(ArtifactRefV1),
    Full {
        feature: ArtifactRefV1,
        task: ActiveTaskRefV1,
    },
}

/// Rejects ambiguous authority before either routine or full-spec validation.
pub fn validate_active_change_path(
    state: &WorkflowStateV1,
    graph: &WorkflowArtifactGraphV1,
) -> Result<ActiveChangePathV1, String> {
    graph.validate()?;
    match (
        state.active_change_contract.as_ref(),
        state.active_feature.as_ref(),
        state.active_task.as_ref(),
    ) {
        (None, None, None) => Ok(ActiveChangePathV1::None),
        (Some(contract), None, None) => {
            contract.validate()?;
            let metadata = graph
                .metadata(contract)
                .ok_or("active change contractがgraphにありません")?;
            if metadata.kind != WorkflowArtifactKindV1::ChangeContract
                || graph.latest(&contract.id).map(|item| item.reference()) != Some(contract.clone())
            {
                return Err("active change contractは現行revisionではありません".into());
            }
            Ok(ActiveChangePathV1::Routine(contract.clone()))
        }
        (None, Some(feature), Some(task)) => {
            graph.validate_active_feature(feature)?;
            graph.validate_active_task(task)?;
            Ok(ActiveChangePathV1::Full {
                feature: feature.clone(),
                task: task.clone(),
            })
        }
        (Some(_), Some(_), _) | (Some(_), _, Some(_)) => {
            Err("routine contractとfull-spec artifactを同時にactiveにできません".into())
        }
        _ => Err("full-spec pathにはactive featureとactive taskの両方が必要です".into()),
    }
}

/// Validates exact immutable bodies and returns the only proof accepted by the
/// compact Specify→Implement gate. No store or provider access occurs here.
pub fn validate_full_spec_for_implementation(
    state: &WorkflowStateV1,
    graph: &WorkflowArtifactGraphV1,
    feature: &FeatureSpecV1,
    plan: &ImplementationPlanV1,
    task_graph: &TaskGraphV1,
    decisions: &[IdeaDecisionV1],
) -> Result<FullSpecGateProofV1, String> {
    let ActiveChangePathV1::Full {
        feature: active_feature,
        task: active_task,
    } = validate_active_change_path(state, graph)?
    else {
        return Err("full-spec pathがactiveではありません".into());
    };
    feature.validate()?;
    plan.validate()?;
    task_graph.validate_with_feature(feature, plan)?;
    active_task.validate_against(task_graph)?;
    if feature.reference() != active_feature
        || plan.reference() != task_graph.implementation_plan
        || task_graph.reference() != active_task.task_graph
    {
        return Err("full-spec本文がactive exact revisionと一致しません".into());
    }
    for reference in [
        feature.reference(),
        plan.reference(),
        task_graph.reference(),
    ] {
        let metadata = graph
            .metadata(&reference)
            .ok_or("full-spec artifactがgraphにありません")?;
        if metadata.header.status != ArtifactStatusV1::Approved
            || graph.latest(&reference.id).map(|item| item.reference()) != Some(reference)
        {
            return Err("full-spec artifactは現行のApproved revisionではありません".into());
        }
    }
    if !feature.required_clarifications.is_empty() {
        return Err("required clarificationが未解決です".into());
    }
    if !feature.consistency_errors.is_empty() {
        return Err("spec consistency errorが残っています".into());
    }
    let (_, runtime) = task_graph
        .task(&active_task.task_id)
        .ok_or("active taskがtask graphにありません")?;
    if matches!(
        runtime.state,
        TaskStateV1::Completed | TaskStateV1::Cancelled
    ) {
        return Err("completed/cancelled taskは実装を開始できません".into());
    }
    let expected = feature
        .header
        .created_from
        .iter()
        .filter(|reference| {
            graph
                .metadata(reference)
                .is_some_and(|item| item.kind == WorkflowArtifactKindV1::IdeaDecision)
        })
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let supplied = decisions
        .iter()
        .map(IdeaDecisionV1::reference)
        .collect::<std::collections::BTreeSet<_>>();
    if supplied != expected {
        return Err("featureに結び付くIdeaDecision本文が不足または過剰です".into());
    }
    for decision in decisions {
        decision.validate()?;
        let reference = decision.reference();
        let metadata = graph
            .metadata(&reference)
            .ok_or("IdeaDecisionがgraphにありません")?;
        if decision.is_unresolved()
            || metadata.header.status != ArtifactStatusV1::Approved
            || graph.latest(&reference.id).map(|item| item.reference()) != Some(reference)
        {
            return Err("未解決または旧revisionのIdeaDecisionがあります".into());
        }
    }
    let decision_refs = supplied.into_iter().collect::<Vec<_>>();
    let graph_basis = graph.basis_hash()?;
    let basis_bytes = serde_json::to_vec(&(
        "polaris.workflow.full_spec_gate.v1",
        &graph_basis,
        &active_feature,
        &plan.reference(),
        &active_task,
        &decision_refs,
    ))
    .map_err(|_| "full-spec gate basisを正規化できません")?;
    Ok(FullSpecGateProofV1 {
        schema_version: WORKFLOW_ARTIFACT_SCHEMA_VERSION,
        artifact_graph_basis_hash: graph_basis,
        feature_spec: active_feature,
        implementation_plan: plan.reference(),
        task_graph: active_task.task_graph,
        task_id: active_task.task_id,
        task_definition_hash: active_task.definition_hash,
        decision_refs,
        basis_hash: crate::conversation_state::content_hash(&basis_bytes),
        stale: false,
    })
}

/// M27's first vertical slice gates entry into implementation. Review,
/// evidence and convergence gates are connected by goal item 5; this function
/// does not claim those records exist before then.
pub fn evaluate_phase_transition(
    state: &WorkflowStateV1,
    target: Phase,
    gates: &WorkflowGatesV1,
    graph: &WorkflowArtifactGraphV1,
) -> Result<TransitionGateProofV1, TransitionBlockerV1> {
    let block = |code, missing_refs: Vec<String>, stale_refs: Vec<ArtifactRefV1>, next: &str| {
        TransitionBlockerV1 {
            schema_version: WORKFLOW_ARTIFACT_SCHEMA_VERSION,
            code,
            from: state.phase,
            to: target,
            missing_refs,
            stale_refs,
            next_task: next.into(),
        }
    };
    if let Some(step) = state.step
        && !state.phase.accepts_step(step)
    {
        return Err(block(
            TransitionBlockerCodeV1::InvalidPhaseStep,
            Vec::new(),
            Vec::new(),
            "phaseとstepの保存状態を修復する",
        ));
    }
    graph.validate().map_err(|_| {
        block(
            TransitionBlockerCodeV1::ChangeContractNotCurrent,
            Vec::new(),
            state.active_change_contract.clone().into_iter().collect(),
            "artifact graphを復旧する",
        )
    })?;
    if validate_active_change_path(state, graph).is_err() {
        return Err(block(
            TransitionBlockerCodeV1::ConflictingChangePaths,
            Vec::new(),
            Vec::new(),
            "routineまたはfull-specのactive pathを一つだけ選び直す",
        ));
    }
    if state
        .active_feature
        .as_ref()
        .is_some_and(|reference| graph.validate_active_feature(reference).is_err())
        || state
            .active_task
            .as_ref()
            .is_some_and(|reference| graph.validate_active_task(reference).is_err())
    {
        return Err(block(
            TransitionBlockerCodeV1::ChangeContractNotCurrent,
            Vec::new(),
            Vec::new(),
            "active feature/taskのexact artifact参照を復旧する",
        ));
    }
    if gates.phase_gate_schema_version.is_some()
        && state.phase == target
        && matches!(target, Phase::Verify | Phase::Deliver)
        && validate_saved_advanced_phase(state, gates, graph).is_err()
    {
        return Err(block(
            TransitionBlockerCodeV1::MissingEvidence,
            vec!["workflow_evidence".into()],
            Vec::new(),
            "現行taskに結び付くphase evidenceを保存する",
        ));
    }
    if gates.phase_gate_schema_version.is_none() || state.phase == target {
        return gate_proof(state, target, gates, graph).map_err(|_| {
            block(
                TransitionBlockerCodeV1::ChangeContractNotCurrent,
                Vec::new(),
                Vec::new(),
                "workflow gateの保存状態を復旧する",
            )
        });
    }
    if gates.phase_gate_schema_version != Some(WORKFLOW_ARTIFACT_SCHEMA_VERSION) {
        return Err(block(
            TransitionBlockerCodeV1::ChangeContractNotCurrent,
            Vec::new(),
            Vec::new(),
            "対応するphase gate形式へ移行する",
        ));
    }

    // Entering an implementation-capable phase always requires either the
    // current lightweight contract or a complete approved spec/plan/task
    // chain. A move from review/verify back to implementation is recovery in
    // phase order, but it still restores write capability.
    if target == Phase::Implement {
        let routine = state
            .active_change_contract
            .as_ref()
            .map(|reference| graph.approved_routine_contract(reference));
        let full = approved_full_spec(state, graph, gates.full_spec.as_ref());
        if routine.is_none() && full.is_err() {
            return Err(block(
                TransitionBlockerCodeV1::MissingChangeContract,
                vec![
                    "change_contract".into(),
                    "feature_spec".into(),
                    "implementation_plan".into(),
                    "task_graph".into(),
                ],
                Vec::new(),
                "routine変更契約または完全仕様artifactをtrusted ownerで承認する",
            ));
        }
        if let Some(result) = routine {
            match result {
                Ok(_) => {}
                Err(reason) if reason.contains("完全仕様経路") && full.is_ok() => {}
                Err(reason) if reason.contains("完全仕様経路") => {
                    return Err(block(
                        TransitionBlockerCodeV1::ChangeRequiresFullSpecification,
                        vec![
                            "feature_spec".into(),
                            "implementation_plan".into(),
                            "task_graph".into(),
                        ],
                        Vec::new(),
                        "完全仕様経路へ昇格する",
                    ));
                }
                Err(_) if full.is_ok() => {}
                Err(_) => {
                    let reference = state
                        .active_change_contract
                        .as_ref()
                        .expect("routine result has a reference");
                    return Err(block(
                        TransitionBlockerCodeV1::ChangeContractNotCurrent,
                        Vec::new(),
                        vec![reference.clone()],
                        "現行revisionのroutine変更契約を承認する",
                    ));
                }
            }
        } else if let Err(reason) = full {
            if reason.contains("status") {
                return Err(block(
                    TransitionBlockerCodeV1::ChangeRequiresFullSpecification,
                    vec![
                        "feature_spec".into(),
                        "implementation_plan".into(),
                        "task_graph".into(),
                    ],
                    Vec::new(),
                    "完全仕様経路へ昇格する",
                ));
            }
            return Err(block(
                TransitionBlockerCodeV1::ChangeContractNotCurrent,
                Vec::new(),
                Vec::new(),
                "現行revisionの完全仕様artifactを復旧する",
            ));
        }
    }

    let from = phase_rank(state.phase);
    let to = phase_rank(target);
    // Any backward move is a recovery action and never grants completion.
    if to < from {
        return gate_proof(state, target, gates, graph).map_err(|_| {
            block(
                TransitionBlockerCodeV1::ChangeContractNotCurrent,
                Vec::new(),
                Vec::new(),
                "workflow gateの保存状態を復旧する",
            )
        });
    }
    if !allowed_forward_edge(state.phase, target) {
        return Err(block(
            TransitionBlockerCodeV1::ForwardPhaseSkipped,
            Vec::new(),
            Vec::new(),
            "次のphaseを完了してから進む",
        ));
    }
    if let Some(transition) = evidence_transition(state.phase, target) {
        let Some(convergence) = gates.convergence.as_ref() else {
            return Err(block(
                TransitionBlockerCodeV1::MissingEvidence,
                vec!["workflow_evidence".into()],
                Vec::new(),
                "現行taskに結び付くreview・verification証拠を保存する",
            ));
        };
        if validate_convergence_binding(state, graph, convergence).is_err() {
            return Err(block(
                TransitionBlockerCodeV1::EvidenceNotCurrent,
                Vec::new(),
                Vec::new(),
                "証拠を現行taskとartifact graphへ結び直す",
            ));
        }
        let input = PhaseGateInputV1 {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            transition,
            convergence: convergence.clone(),
        };
        if let PhaseGateEvaluationV1::Blocked(blocked) = evaluate_phase_gate(&input) {
            return Err(block(
                TransitionBlockerCodeV1::MissingEvidence,
                blocked.references,
                Vec::new(),
                &blocked.next_task,
            ));
        }
    }
    gate_proof(state, target, gates, graph).map_err(|_| {
        block(
            TransitionBlockerCodeV1::ChangeContractNotCurrent,
            Vec::new(),
            Vec::new(),
            "workflow gateの保存状態を復旧する",
        )
    })
}

fn approved_full_spec(
    state: &WorkflowStateV1,
    graph: &WorkflowArtifactGraphV1,
    proof: Option<&FullSpecGateProofV1>,
) -> Result<(), String> {
    let feature_ref = state
        .active_feature
        .as_ref()
        .ok_or("active feature is missing")?;
    let task_ref = state.active_task.as_ref().ok_or("active task is missing")?;
    let feature = graph
        .metadata(feature_ref)
        .ok_or("active feature is missing")?;
    let task = graph
        .metadata(&task_ref.task_graph)
        .ok_or("active task graph is missing")?;
    if feature.kind != WorkflowArtifactKindV1::FeatureSpec
        || task.kind != WorkflowArtifactKindV1::TaskGraph
        || feature.header.status != ArtifactStatusV1::Approved
        || task.header.status != ArtifactStatusV1::Approved
    {
        return Err("full specification status is not approved".into());
    }
    let plan = task
        .header
        .created_from
        .iter()
        .filter_map(|reference| graph.metadata(reference))
        .find(|metadata| metadata.kind == WorkflowArtifactKindV1::ImplementationPlan)
        .ok_or("approved implementation plan is missing")?;
    if plan.header.status != ArtifactStatusV1::Approved
        || !plan.header.created_from.contains(feature_ref)
        || !task.header.created_from.contains(feature_ref)
    {
        return Err("full specification status or lineage is not approved".into());
    }
    let proof = proof.ok_or("full-spec proof is missing")?;
    if proof.implementation_plan != plan.reference() {
        return Err("full-spec proof implementation plan is stale".into());
    }
    if graph.latest(&feature_ref.id).map(|item| item.reference()) != Some(feature_ref.clone())
        || graph.latest(&plan.header.id).map(|item| item.reference()) != Some(plan.reference())
        || graph
            .latest(&task_ref.task_graph.id)
            .map(|item| item.reference())
            != Some(task_ref.task_graph.clone())
    {
        return Err("full specification contains a superseded revision".into());
    }
    validate_full_spec_proof(state, graph, proof)?;
    Ok(())
}

fn validate_full_spec_proof(
    state: &WorkflowStateV1,
    graph: &WorkflowArtifactGraphV1,
    proof: &FullSpecGateProofV1,
) -> Result<(), String> {
    if proof.schema_version != WORKFLOW_ARTIFACT_SCHEMA_VERSION || proof.stale {
        return Err("full-spec proof is stale or unsupported".into());
    }
    let feature = state
        .active_feature
        .as_ref()
        .ok_or("active feature is missing")?;
    let task = state.active_task.as_ref().ok_or("active task is missing")?;
    if proof.artifact_graph_basis_hash != graph.basis_hash()?
        || proof.feature_spec != *feature
        || proof.task_graph != task.task_graph
        || proof.task_id != task.task_id
        || proof.task_definition_hash != task.definition_hash
    {
        return Err("full-spec proof binding is stale".into());
    }
    let expected_decisions = graph
        .metadata(feature)
        .ok_or("active feature is missing")?
        .header
        .created_from
        .iter()
        .filter(|reference| {
            graph
                .metadata(reference)
                .is_some_and(|item| item.kind == WorkflowArtifactKindV1::IdeaDecision)
        })
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    if proof
        .decision_refs
        .iter()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>()
        != expected_decisions
        || proof.decision_refs.len() != expected_decisions.len()
    {
        return Err("full-spec proofのIdeaDecision集合が一致しません".into());
    }
    let active = ActiveTaskRefV1 {
        task_graph: proof.task_graph.clone(),
        task_id: proof.task_id.clone(),
        definition_hash: proof.task_definition_hash.clone(),
    };
    let basis_bytes = serde_json::to_vec(&(
        "polaris.workflow.full_spec_gate.v1",
        &proof.artifact_graph_basis_hash,
        &proof.feature_spec,
        &proof.implementation_plan,
        &active,
        &proof.decision_refs,
    ))
    .map_err(|_| "full-spec proofを正規化できません")?;
    if proof.basis_hash != crate::conversation_state::content_hash(&basis_bytes) {
        return Err("full-spec proof hashが一致しません".into());
    }
    for reference in &proof.decision_refs {
        let metadata = graph
            .metadata(reference)
            .ok_or("full-spec proofのIdeaDecisionがgraphにありません")?;
        if metadata.kind != WorkflowArtifactKindV1::IdeaDecision
            || metadata.header.status != ArtifactStatusV1::Approved
            || graph.latest(&reference.id).map(|item| item.reference()) != Some(reference.clone())
        {
            return Err("full-spec proofのIdeaDecisionが現行Approvedではありません".into());
        }
    }
    Ok(())
}

fn evidence_transition(from: Phase, to: Phase) -> Option<WorkflowPhaseGateV1> {
    match (from, to) {
        (Phase::Implement, Phase::Review) => Some(WorkflowPhaseGateV1::ImplementToReview),
        (Phase::Review, Phase::Verify) => Some(WorkflowPhaseGateV1::ReviewToVerify),
        (Phase::Verify, Phase::Deliver) => Some(WorkflowPhaseGateV1::VerifyToDeliver),
        _ => None,
    }
}

fn validate_convergence_binding(
    state: &WorkflowStateV1,
    graph: &WorkflowArtifactGraphV1,
    convergence: &ConvergenceInputV1,
) -> Result<(), String> {
    convergence.validate()?;
    if convergence.required_criterion_ids.is_empty() {
        return Err("required criteria are empty".into());
    }
    if convergence.binding.artifact_graph_basis_hash != graph.basis_hash()? {
        return Err("evidence graph basis is stale".into());
    }
    if let Some(task) = &state.active_task {
        if convergence.binding.task_id != task.task_id
            || convergence.binding.task_definition_hash != task.definition_hash
        {
            return Err("evidence task binding is stale".into());
        }
        return Ok(());
    }
    let contract = state
        .active_change_contract
        .as_ref()
        .ok_or("evidence has no active artifact")?;
    if convergence.binding.task_id != contract.id
        || convergence.binding.task_definition_hash != contract.content_hash
    {
        return Err("evidence contract binding is stale".into());
    }
    Ok(())
}

fn validate_saved_advanced_phase(
    state: &WorkflowStateV1,
    gates: &WorkflowGatesV1,
    graph: &WorkflowArtifactGraphV1,
) -> Result<(), String> {
    let transition = match state.phase {
        Phase::Verify => WorkflowPhaseGateV1::ReviewToVerify,
        Phase::Deliver => WorkflowPhaseGateV1::VerifyToDeliver,
        _ => return Ok(()),
    };
    let convergence = gates
        .convergence
        .as_ref()
        .ok_or("verify/deliver phaseには現行evidenceが必要です")?;
    validate_convergence_binding(state, graph, convergence)?;
    let input = PhaseGateInputV1 {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        transition,
        convergence: convergence.clone(),
    };
    match evaluate_phase_gate(&input) {
        PhaseGateEvaluationV1::Allowed(_) => Ok(()),
        PhaseGateEvaluationV1::Blocked(blocked) => Err(format!(
            "verify/deliver phase gateが未達です: {}",
            blocked.next_task
        )),
    }
}

fn gate_proof(
    state: &WorkflowStateV1,
    target: Phase,
    gates: &WorkflowGatesV1,
    graph: &WorkflowArtifactGraphV1,
) -> Result<TransitionGateProofV1, String> {
    let bytes = serde_json::to_vec(&(
        state.phase,
        state.step,
        target,
        &state.active_change_contract,
        &state.active_feature,
        &state.active_task,
        gates.phase_gate_schema_version,
        &gates.convergence,
        &gates.full_spec,
        graph.revision,
        graph.basis_hash()?,
    ))
    .map_err(|_| "phase gate basisを正規化できません")?;
    Ok(TransitionGateProofV1 {
        basis_hash: crate::conversation_state::content_hash(&bytes),
    })
}

fn phase_rank(phase: Phase) -> u8 {
    match phase {
        Phase::General => 0,
        Phase::Brainstorm => 1,
        Phase::Specify => 2,
        Phase::Implement => 3,
        Phase::Review => 4,
        Phase::Verify => 5,
        Phase::Deliver => 6,
    }
}

fn allowed_forward_edge(from: Phase, to: Phase) -> bool {
    matches!(
        (from, to),
        (
            Phase::General,
            Phase::Brainstorm | Phase::Specify | Phase::Review
        ) | (Phase::Brainstorm, Phase::Specify)
            | (Phase::Specify, Phase::Implement)
            | (Phase::Implement, Phase::Review)
            | (Phase::Review, Phase::Verify)
            | (Phase::Verify, Phase::Deliver)
    )
}
impl WorkflowGatesV1 {
    pub fn revalidate_artifact(&mut self, current_hash: &str) {
        if let Some(evidence) = &mut self.verification {
            // Once stale, only a new verification can make the evidence current.
            evidence.stale |= evidence.artifact_hash != current_hash;
        }
    }

    pub fn revalidate_graph(&mut self, graph: &WorkflowArtifactGraphV1) -> Result<(), String> {
        let current = graph.basis_hash()?;
        if let Some(proof) = &mut self.full_spec {
            proof.stale |= proof.artifact_graph_basis_hash != current;
        }
        if let Some(verification) = &mut self.verification {
            verification.stale = true;
        }
        // Convergence is immutable evidence. Keep it for audit; its old graph
        // basis is rejected by validate_convergence_binding.
        Ok(())
    }

    pub fn for_fork(&self, artifact_hash: &str) -> Self {
        let mut child = self.clone();
        child.external_grants.clear();
        child.revalidate_artifact(artifact_hash);
        child
    }
}

/// Runtime turn boundary. Models and retrieved text have no transition API.
/// The controller does not own gates, so focus changes cannot grant permission.
#[derive(Debug, Clone, Default)]
pub struct Workflow {
    state: WorkflowStateV1,
    pending_phase: Option<Phase>,
    reservation: Option<WorkflowReservation>,
    next_reservation: u64,
    active: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct WorkflowReservation {
    id: u64,
    phase: Phase,
    from_revision: u64,
    publishing: bool,
}
impl Workflow {
    pub fn restore(state: WorkflowStateV1) -> Result<Self, String> {
        if state.schema_version != WORKFLOW_SCHEMA_VERSION {
            return Err("未対応のworkflow保存形式です".into());
        }
        if state
            .step
            .is_some_and(|step| !state.phase.accepts_step(step))
        {
            return Err("workflow phaseとstepが一致しません".into());
        }
        if let Some(reference) = &state.active_change_contract {
            reference.validate()?;
        }
        if let Some(reference) = &state.active_feature {
            reference.validate()?;
        }
        if let Some(reference) = &state.active_task {
            reference.validate()?;
        }
        if state.active_task.is_some() && state.active_feature.is_none() {
            return Err("active taskにはactive feature参照が必要です".into());
        }
        Ok(Self {
            state,
            ..Self::default()
        })
    }
    pub fn state(&self) -> &WorkflowStateV1 {
        &self.state
    }
    pub fn pending_phase(&self) -> Option<Phase> {
        self.pending_phase
    }

    /// Called only by explicit UI/CLI input or structured parent configuration.
    fn request_phase(&mut self, phase: Phase) -> Result<(), String> {
        // A manual request received before durable publishing wins over a
        // model proposal. Once publishing has started it becomes the next
        // boundary request and can never rewrite the acknowledged revision.
        if self
            .reservation
            .is_some_and(|reservation| !reservation.publishing)
        {
            self.reservation = None;
        }
        if self.active {
            self.pending_phase = Some(phase);
        } else {
            self.apply_phase(phase)?;
        }
        Ok(())
    }
    fn apply_phase(&mut self, phase: Phase) -> Result<(), String> {
        if self.state.phase != phase {
            let revision = self
                .state
                .revision
                .checked_add(1)
                .ok_or("workflow revisionが上限に達しました")?;
            self.state.phase = phase;
            self.state.step = phase.default_step();
            self.state.revision = revision;
            self.state.skill_manifest_hash.clear();
        }
        Ok(())
    }
    pub fn next_phase(&self) -> Phase {
        self.pending_phase.unwrap_or(self.state.phase)
    }

    /// Resolve skills and validate their budget BEFORE opening a turn. The hash
    /// freezes the resolved bodies; callers retain that same snapshot for tools.
    pub fn begin_turn(&mut self, manifest_hash: String) -> Result<WorkflowStateV1, String> {
        // Kept for the in-memory legacy controller API. Durable session
        // callers use `start_turn` and checkpoint a pending phase first.
        if self.active {
            return Err("作業ターンが既に実行中です".into());
        }
        if let Some(phase) = self.pending_phase {
            self.apply_phase(phase)?;
            self.pending_phase = None;
        }
        self.start_turn(manifest_hash)
    }
    fn start_turn(&mut self, manifest_hash: String) -> Result<WorkflowStateV1, String> {
        if self.active {
            return Err("作業ターンが既に実行中です".into());
        }
        if manifest_hash.is_empty() {
            return Err("skill manifest hashがありません".into());
        }
        self.state.skill_manifest_hash = manifest_hash;
        self.active = true;
        Ok(self.state.clone())
    }
    pub fn finish_turn(&mut self) {
        self.active = false;
    }
}

/// Shared controller permits an explicit UI phase request while a turn future
/// owns the session. A turn guard releases it even if that future is dropped.
#[derive(Debug, Clone)]
pub struct SessionWorkflow {
    pub config: WorkflowConfig,
    controller: std::sync::Arc<std::sync::Mutex<Workflow>>,
    pub gates: WorkflowGatesV1,
    artifact_graph: WorkflowArtifactGraphV1,
}
impl SessionWorkflow {
    pub fn new(config: WorkflowConfig) -> Self {
        match Self::try_new(config.clone()) {
            Ok(workflow) => workflow,
            Err(_) => {
                // An infallible compatibility constructor must never turn an
                // advanced configured phase into a gate bypass. Callers that
                // surface configuration errors use `try_new`; older internal
                // callers receive the nearest safe specification phase.
                let mut safe = config;
                safe.initial_phase = Phase::Specify;
                Self::try_new(safe).expect("specify is a valid empty-graph phase")
            }
        }
    }

    pub fn try_new(config: WorkflowConfig) -> Result<Self, String> {
        let state = WorkflowStateV1 {
            phase: config.initial_phase,
            step: config.initial_phase.default_step(),
            ..Default::default()
        };
        Self::restore(config, WorkflowGatesV1::default(), state)
    }
    pub fn restore(
        config: WorkflowConfig,
        gates: WorkflowGatesV1,
        state: WorkflowStateV1,
    ) -> Result<Self, String> {
        Self::restore_with_artifacts(config, gates, state, WorkflowArtifactGraphV1::default())
    }

    pub fn restore_with_artifacts(
        config: WorkflowConfig,
        gates: WorkflowGatesV1,
        state: WorkflowStateV1,
        artifact_graph: WorkflowArtifactGraphV1,
    ) -> Result<Self, String> {
        if gates.schema_version != WORKFLOW_SCHEMA_VERSION {
            return Err("未対応のgate保存形式です".into());
        }
        if gates
            .phase_gate_schema_version
            .is_some_and(|version| version != WORKFLOW_ARTIFACT_SCHEMA_VERSION)
        {
            return Err("未対応のphase gate保存形式です".into());
        }
        artifact_graph.validate()?;
        if gates.phase_gate_schema_version.is_some() {
            validate_active_change_path(&state, &artifact_graph)?;
            validate_saved_advanced_phase(&state, &gates, &artifact_graph)?;
        }
        if gates.phase_gate_schema_version.is_some()
            && state.phase == Phase::Implement
            && state.active_change_contract.is_none()
        {
            approved_full_spec(&state, &artifact_graph, gates.full_spec.as_ref()).map_err(
                |_| {
                    "承認済みroutine contractまたは完全仕様なしで実装phaseから開始できません"
                        .to_string()
                },
            )?;
        }
        if let Some(reference) = &state.active_change_contract {
            artifact_graph.approved_routine_contract(reference)?;
        }
        if let Some(reference) = &state.active_feature {
            artifact_graph.validate_active_feature(reference)?;
        }
        if let Some(reference) = &state.active_task {
            artifact_graph.validate_active_task(reference)?;
        }
        Ok(Self {
            config,
            gates,
            artifact_graph,
            controller: std::sync::Arc::new(std::sync::Mutex::new(Workflow::restore(state)?)),
        })
    }
    pub fn request_phase(&self, phase: Phase) -> Result<(), String> {
        let mut controller = self
            .controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?;
        evaluate_phase_transition(&controller.state, phase, &self.gates, &self.artifact_graph)
            .map_err(|blocker| blocker.to_string())?;
        controller.request_phase(phase)
    }
    pub fn state(&self) -> WorkflowStateV1 {
        self.controller
            .lock()
            .expect("workflow controller poisoned")
            .state()
            .clone()
    }

    /// Returns whether this exact persisted workflow state may expose or
    /// execute a mutation.  This is intentionally stricter than phase text:
    /// a model cannot regain write authority merely by reaching (or claiming)
    /// `implement`.  The active path and its current trusted evidence must
    /// still validate at the point a tool schema is built and again at
    /// dispatch.
    pub fn mutation_authorized(&self) -> bool {
        mutation_authorized(&self.state(), &self.gates, &self.artifact_graph)
    }
    pub fn for_fork(&self, artifact_hash: &str) -> Result<Self, String> {
        Self::restore_with_artifacts(
            self.config.clone(),
            self.gates.for_fork(artifact_hash),
            self.state(),
            self.artifact_graph.clone(),
        )
    }

    pub fn artifact_graph(&self) -> &WorkflowArtifactGraphV1 {
        &self.artifact_graph
    }

    /// Builds the post-publication runtime without mutating the current owner.
    /// Only an approved routine contract can become the active compact ref.
    pub fn with_approved_change_contract(
        &self,
        artifact_graph: WorkflowArtifactGraphV1,
        reference: ArtifactRefV1,
    ) -> Result<Self, String> {
        artifact_graph.approved_routine_contract(&reference)?;
        let mut state = self.state();
        if state.active_change_contract.as_ref() == Some(&reference)
            && self.artifact_graph == artifact_graph
        {
            return Self::restore_with_artifacts(
                self.config.clone(),
                self.gates.clone(),
                state,
                artifact_graph,
            );
        }
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or("workflow revisionが上限に達しました")?;
        state.active_change_contract = Some(reference);
        state.active_feature = None;
        state.active_task = None;
        state.skill_manifest_hash.clear();
        state.skills.clear();
        let mut gates = self.gates.clone();
        gates.full_spec = None;
        gates.convergence = None;
        if let Some(verification) = &mut gates.verification {
            verification.stale = true;
        }
        Self::restore_with_artifacts(self.config.clone(), gates, state, artifact_graph)
    }

    /// Selects one exact approved feature/task revision. The immutable bodies
    /// are validated by the store before this method is called; this compact
    /// state retains only the references needed to reproduce that validation.
    pub fn with_active_task_artifacts(
        &self,
        artifact_graph: WorkflowArtifactGraphV1,
        feature: ArtifactRefV1,
        task: ActiveTaskRefV1,
    ) -> Result<Self, String> {
        artifact_graph.validate_active_feature(&feature)?;
        artifact_graph.validate_active_task(&task)?;
        let mut state = self.state();
        if state.active_feature.as_ref() == Some(&feature)
            && state.active_task.as_ref() == Some(&task)
            && self.artifact_graph == artifact_graph
        {
            return Self::restore_with_artifacts(
                self.config.clone(),
                self.gates.clone(),
                state,
                artifact_graph,
            );
        }
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or("workflow revisionが上限に達しました")?;
        state.active_feature = Some(feature);
        state.active_task = Some(task);
        state.active_change_contract = None;
        state.skill_manifest_hash.clear();
        state.skills.clear();
        let mut gates = self.gates.clone();
        gates.convergence = None;
        gates.full_spec = None;
        if let Some(verification) = &mut gates.verification {
            verification.stale = true;
        }
        Self::restore_with_artifacts(self.config.clone(), gates, state, artifact_graph)
    }

    /// Publishes a host-derived full-spec proof after exact bodies were
    /// validated by `validate_full_spec_for_implementation`.
    pub fn with_full_spec_proof_trusted(&self, proof: FullSpecGateProofV1) -> Result<Self, String> {
        validate_full_spec_proof(&self.state(), &self.artifact_graph, &proof)?;
        if self.gates.full_spec.as_ref() == Some(&proof) {
            return Self::restore_with_artifacts(
                self.config.clone(),
                self.gates.clone(),
                self.state(),
                self.artifact_graph.clone(),
            );
        }
        let mut state = self.state();
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or("workflow revisionが上限に達しました")?;
        state.skill_manifest_hash.clear();
        state.skills.clear();
        let mut gates = self.gates.clone();
        gates.full_spec = Some(proof);
        Self::restore_with_artifacts(
            self.config.clone(),
            gates,
            state,
            self.artifact_graph.clone(),
        )
    }

    /// Publishes host-derived evidence input into the runtime snapshot. The
    /// owner must first derive criterion IDs from the exact active artifact
    /// body; this method then binds the record set to the compact state and
    /// current graph basis before it can affect a phase gate.
    pub fn with_convergence_trusted(
        &self,
        convergence: ConvergenceInputV1,
    ) -> Result<Self, String> {
        validate_convergence_binding(&self.state(), &self.artifact_graph, &convergence)?;
        if self.gates.convergence.as_ref() == Some(&convergence) {
            return Self::restore_with_artifacts(
                self.config.clone(),
                self.gates.clone(),
                self.state(),
                self.artifact_graph.clone(),
            );
        }
        let mut state = self.state();
        state.revision = state
            .revision
            .checked_add(1)
            .ok_or("workflow revisionが上限に達しました")?;
        state.skill_manifest_hash.clear();
        state.skills.clear();
        let mut gates = self.gates.clone();
        gates.convergence = Some(convergence);
        Self::restore_with_artifacts(
            self.config.clone(),
            gates,
            state,
            self.artifact_graph.clone(),
        )
    }
    pub fn begin_turn(&self, skills: &[polaris_skills::Skill]) -> Result<WorkflowTurn, String> {
        let mut controller = self
            .controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?;
        let mut candidates = polaris_skills::workflow_profile::recommended_bindings();
        candidates.extend_from_slice(skills);
        let phases: Vec<Phase> = match self.config.mode {
            WorkflowMode::Manual => {
                let mut phases = vec![controller.state.phase];
                if let Some(pending) = controller.pending_phase
                    && pending != controller.state.phase
                {
                    phases.push(pending);
                }
                phases
            }
            // Freeze every phase at the real-turn boundary. A later adaptive
            // transition only selects from this snapshot; it never reads disk
            // or a changing catalog in the middle of a user turn.
            WorkflowMode::Adaptive => Phase::ALL.to_vec(),
        };
        let profiles = resolve_profiles(&self.config, &candidates, &phases)?;
        // The resolver's skill-only limit is necessary but not sufficient:
        // validate the final state diagnostics for every snapshot that an
        // adaptive turn may select before any provider request is sent.
        for (phase, frozen) in &profiles {
            let mut snapshot = controller.state.clone();
            snapshot.phase = *phase;
            snapshot.step = phase.default_step();
            build_profile(snapshot, frozen, &self.gates)?;
        }
        let current = profiles
            .get(&controller.state.phase)
            .expect("current phase is always resolved");
        let had_manifest = !controller.state.skill_manifest_hash.is_empty();
        let previous_skills = controller.state.skills.clone();
        controller.start_turn(current.manifest_hash.clone())?;
        controller.state.skills = current.entries.clone();
        let profile = build_profile(controller.state.clone(), current, &self.gates)?;
        let changed_skill_ids = if had_manifest {
            changed_skill_ids(&previous_skills, &profile.state.skills)
        } else {
            Vec::new()
        };
        Ok(WorkflowTurn {
            body: profile.body.clone(),
            ids: profile.ids.clone(),
            tokens: profile.tokens,
            state: profile.state.clone(),
            changed_skill_ids,
            mode: self.config.mode,
            config: self.config.clone(),
            candidates,
            profiles,
            artifact_graph: self.artifact_graph.clone(),
            controller: self.controller.clone(),
        })
    }

    /// Refresh only mandatory user skills at a new real turn. Optional catalog
    /// search is unaffected, and an in-flight turn retains its earlier snapshot.
    pub fn begin_turn_from_disk(
        &self,
        skills: &[polaris_skills::Skill],
    ) -> Result<WorkflowTurn, String> {
        self.begin_turn(&self.refresh_required_skills(skills)?)
    }

    /// Reads mandatory user skills once at a real-turn boundary. Callers that
    /// retry a child schema response retain this returned snapshot rather than
    /// reopening host paths in the retry.
    pub(crate) fn refresh_required_skills(
        &self,
        skills: &[polaris_skills::Skill],
    ) -> Result<Vec<polaris_skills::Skill>, String> {
        let controller = self
            .controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?;
        let phases: Vec<Phase> = match self.config.mode {
            WorkflowMode::Manual => {
                let mut phases = vec![controller.state.phase];
                if let Some(pending) = controller.pending_phase
                    && pending != controller.state.phase
                {
                    phases.push(pending);
                }
                phases
            }
            WorkflowMode::Adaptive => Phase::ALL.to_vec(),
        };
        drop(controller);
        let names: Vec<&str> = self
            .config
            .always
            .iter()
            .chain(
                phases
                    .iter()
                    .flat_map(|phase| self.config.phase_skills.get(phase).into_iter().flatten()),
            )
            .map(String::as_str)
            .collect();
        let mut refreshed = skills.to_vec();
        for skill in &mut refreshed {
            if names.iter().any(|name| {
                *name == skill.name || name.strip_prefix("user:") == Some(skill.name.as_str())
            }) {
                let text = std::fs::read_to_string(&skill.path).map_err(|error| {
                    format!("必須skill {}を読み込めません: {error}", skill.name)
                })?;
                let (name, description, body) =
                    polaris_skills::frontmatter::parse(&text, &skill.name)
                        .map_err(|error| error.to_string())?;
                skill.name = name;
                skill.description = description;
                skill.body = body;
            }
        }
        Ok(refreshed)
    }
}

/// The single authority predicate for file-changing capabilities.  Missing,
/// legacy, stale, or malformed workflow state fails closed.  Read-only tools
/// deliberately do not consult this predicate.
pub fn mutation_authorized(
    state: &WorkflowStateV1,
    gates: &WorkflowGatesV1,
    graph: &WorkflowArtifactGraphV1,
) -> bool {
    if state.phase != Phase::Implement
        || gates.phase_gate_schema_version != Some(WORKFLOW_ARTIFACT_SCHEMA_VERSION)
        || validate_active_change_path(state, graph).is_err()
    {
        return false;
    }
    match validate_active_change_path(state, graph) {
        Ok(ActiveChangePathV1::Routine(reference)) => {
            graph.approved_routine_contract(&reference).is_ok()
        }
        Ok(ActiveChangePathV1::Full { .. }) => {
            approved_full_spec(state, graph, gates.full_spec.as_ref()).is_ok()
        }
        Ok(ActiveChangePathV1::None) | Err(_) => false,
    }
}
pub struct WorkflowTurn {
    pub body: String,
    pub ids: Vec<String>,
    pub tokens: usize,
    pub state: WorkflowStateV1,
    pub changed_skill_ids: Vec<String>,
    mode: WorkflowMode,
    config: WorkflowConfig,
    candidates: Vec<polaris_skills::Skill>,
    profiles: BTreeMap<Phase, FrozenWorkflowProfile>,
    artifact_graph: WorkflowArtifactGraphV1,
    controller: std::sync::Arc<std::sync::Mutex<Workflow>>,
}

#[derive(Debug, Clone)]
struct FrozenWorkflowProfile {
    body: String,
    manifest_hash: String,
    ids: Vec<String>,
    entries: Vec<WorkflowSkillV1>,
}

impl WorkflowTurn {
    /// Metadata from the catalog frozen at this real-turn boundary. Bodies
    /// remain private to workflow profile resolution; optional discovery only
    /// receives names, descriptions, stable IDs, and source identities.
    pub(crate) fn catalog_entries(&self) -> Vec<polaris_tools::skill::SkillCatalogEntry> {
        skill_catalog_entries(&self.candidates)
    }

    /// Complete skills frozen at the real-turn boundary. This is deliberately
    /// only exposed within core so exact `skill` lookup can disclose the same
    /// body whose metadata was advertised, including refreshed mandatory and
    /// built-in entries.
    pub(crate) fn skill_corpus(&self) -> &[polaris_skills::Skill] {
        &self.candidates
    }
}

pub(crate) fn skill_catalog_entries(
    skills: &[polaris_skills::Skill],
) -> Vec<polaris_tools::skill::SkillCatalogEntry> {
    skills
        .iter()
        .map(|skill| {
            let source_identity = skill.path.to_string_lossy().into_owned();
            let id = if source_identity.starts_with("builtin:") {
                format!("builtin:{}", skill.name)
            } else {
                format!("user:{}", skill.name)
            };
            polaris_tools::skill::SkillCatalogEntry {
                id,
                source_identity,
                name: normalize_metadata(&skill.name),
                description: normalize_metadata(&skill.description),
            }
        })
        .collect()
}

impl WorkflowTurn {
    /// Refreshes the manual turn's single fixed profile after its owner has
    /// durably committed a phase that was pending before this real turn
    /// started. The profile still comes from this turn's captured catalog;
    /// callers must not use this after the first provider request.
    pub fn refresh_manual_profile(&mut self, gates: &WorkflowGatesV1) -> Result<(), String> {
        if self.mode != WorkflowMode::Manual {
            return Err("adaptive modeではmanual profileを更新できません".into());
        }
        let state = self
            .controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?
            .state
            .clone();
        let frozen = self.frozen_for(state.phase)?;
        let profile = build_profile(state, &frozen, gates)?;
        self.body = profile.body;
        self.ids = profile.ids;
        self.tokens = profile.tokens;
        self.state = profile.state;
        Ok(())
    }

    /// Rebinds a still-open real turn to an owner-acknowledged workflow
    /// artifact snapshot without reopening skill files. This is used after a
    /// routine contract is durably published in the same turn; the next model
    /// request may then evaluate the real phase gate against that exact graph.
    pub fn refresh_runtime(&mut self, workflow: &SessionWorkflow) -> Result<(), String> {
        if self.config != workflow.config || self.mode != workflow.config.mode {
            return Err("workflow turnの設定が保存済みruntimeと一致しません".into());
        }
        let state = workflow.state();
        let frozen = self.frozen_for(state.phase)?;
        let profile = build_profile(state, &frozen, &workflow.gates)?;
        self.body = profile.body;
        self.ids = profile.ids;
        self.tokens = profile.tokens;
        self.state = profile.state;
        self.artifact_graph = workflow.artifact_graph.clone();
        self.controller = workflow.controller.clone();
        Ok(())
    }

    /// Returns the profile for the currently committed focus. Pending manual
    /// work is deliberately not applied here: its owner must checkpoint it
    /// through `prepare_pending` before the next provider request.
    pub fn profile(&self, gates: &WorkflowGatesV1) -> Result<WorkflowProfile, String> {
        if self.mode == WorkflowMode::Manual {
            return Ok(WorkflowProfile {
                state: self.state.clone(),
                body: self.body.clone(),
                ids: self.ids.clone(),
                tokens: self.tokens,
            });
        }
        let state = self
            .controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?
            .state
            .clone();
        let frozen = self
            .profiles
            .get(&state.phase)
            .ok_or("この実ターンに固定された段階skillがありません")?;
        build_profile(state, frozen, gates)
    }

    /// Prepares a manual request that was queued while the real turn was
    /// active. It has the same durable reservation boundary as a model
    /// proposal, but does not need a model-supplied revision.
    pub fn prepare_pending(
        &self,
        gates: &WorkflowGatesV1,
    ) -> Result<Option<PreparedWorkflowTransition>, String> {
        let mut controller = self
            .controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?;
        let Some(phase) = controller.pending_phase else {
            return Ok(None);
        };
        self.prepare(&mut controller, phase, gates, true).map(Some)
    }

    /// Reserves an adaptive model proposal without changing the committed
    /// state. The caller persists `candidate.state` outside the mutex, then
    /// calls `commit` only after its durable ACK.
    pub fn prepare_transition(
        &self,
        phase: Phase,
        expected_revision: u64,
        gates: &WorkflowGatesV1,
    ) -> Result<PreparedWorkflowTransition, String> {
        if self.mode != WorkflowMode::Adaptive {
            return Err("manual modeではmodelによるworkflow変更はできません".into());
        }
        let mut controller = self
            .controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?;
        if controller.state.revision != expected_revision {
            return Err("workflow revisionが競合しました".into());
        }
        if controller.pending_phase.is_some() {
            return Err("手動のworkflow変更が保留中です".into());
        }
        self.prepare(&mut controller, phase, gates, false)
    }

    fn prepare(
        &self,
        controller: &mut Workflow,
        phase: Phase,
        gates: &WorkflowGatesV1,
        pending: bool,
    ) -> Result<PreparedWorkflowTransition, String> {
        if controller.reservation.is_some() {
            return Err("workflow変更は保存確認待ちです".into());
        }
        let previous = controller.state.clone();
        let gate_proof = evaluate_phase_transition(&previous, phase, gates, &self.artifact_graph)
            .map_err(|blocker| blocker.to_string())?;
        let frozen = self.frozen_for(phase)?;
        let revision = if phase == previous.phase {
            previous.revision
        } else {
            previous
                .revision
                .checked_add(1)
                .ok_or("workflow revisionが上限に達しました")?
        };
        let mut candidate_state = previous.clone();
        candidate_state.phase = phase;
        candidate_state.step = phase.default_step();
        candidate_state.revision = revision;
        let candidate = build_profile(candidate_state, &frozen, gates)?;
        let reservation_id = controller.next_reservation;
        controller.next_reservation = controller.next_reservation.wrapping_add(1);
        controller.reservation = Some(WorkflowReservation {
            id: reservation_id,
            phase,
            from_revision: previous.revision,
            publishing: false,
        });
        Ok(PreparedWorkflowTransition {
            previous,
            candidate,
            reservation_id,
            pending,
            gate_basis_hash: gate_proof.basis_hash,
            controller: self.controller.clone(),
            finished: false,
        })
    }

    /// Resolves a manually queued phase from the catalog snapshot captured at
    /// real-turn start. This never reads disk or a changed skill catalog.
    fn frozen_for(&self, phase: Phase) -> Result<FrozenWorkflowProfile, String> {
        if let Some(frozen) = self.profiles.get(&phase) {
            return Ok(frozen.clone());
        }
        resolve_profiles(&self.config, &self.candidates, &[phase])?
            .remove(&phase)
            .ok_or_else(|| "この実ターンに固定された段階skillがありません".into())
    }
}

fn normalize_metadata(value: &str) -> String {
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// A candidate state reserved for a single durable checkpoint. This owns no
/// mutex guard and is therefore safe to carry across an async persistence
/// await. Dropping it releases an uncommitted reservation.
pub struct PreparedWorkflowTransition {
    pub previous: WorkflowStateV1,
    pub candidate: WorkflowProfile,
    reservation_id: u64,
    pending: bool,
    gate_basis_hash: String,
    controller: std::sync::Arc<std::sync::Mutex<Workflow>>,
    finished: bool,
}

impl PreparedWorkflowTransition {
    pub fn is_noop(&self) -> bool {
        self.previous.phase == self.candidate.state.phase
            && self.previous.revision == self.candidate.state.revision
    }

    pub fn gate_basis_hash(&self) -> &str {
        &self.gate_basis_hash
    }

    /// Marks the reservation as externally publishing. Manual changes after
    /// this point queue for the following boundary instead of rewriting this
    /// candidate.
    pub fn begin_persist(&mut self) -> Result<(), String> {
        let id = self.reservation_id;
        let mut controller = self
            .controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?;
        let reservation = controller
            .reservation
            .as_mut()
            .filter(|reservation| reservation.id == id)
            .ok_or("workflow変更の予約が失効しました")?;
        reservation.publishing = true;
        Ok(())
    }

    pub fn commit(mut self) -> Result<WorkflowStateV1, String> {
        let id = self.reservation_id;
        let mut controller = self
            .controller
            .lock()
            .map_err(|_| "workflow状態を取得できません")?;
        let reservation = controller
            .reservation
            .as_ref()
            .copied()
            .filter(|reservation| reservation.id == id && reservation.publishing)
            .ok_or("workflow変更の予約が失効しました")?;
        if reservation.from_revision != self.previous.revision {
            return Err("workflow変更の保存前revisionが一致しません".into());
        }
        controller.reservation = None;
        controller.state = self.candidate.state.clone();
        if self.pending && controller.pending_phase == Some(reservation.phase) {
            controller.pending_phase = None;
        }
        self.finished = true;
        Ok(controller.state.clone())
    }

    pub fn cancel(mut self) {
        self.release();
    }

    fn release(&mut self) {
        let id = self.reservation_id;
        if let Ok(mut controller) = self.controller.lock()
            && controller
                .reservation
                .is_some_and(|reservation| reservation.id == id)
        {
            controller.reservation = None;
        }
        self.finished = true;
    }
}

impl Drop for PreparedWorkflowTransition {
    fn drop(&mut self) {
        if !self.finished {
            self.release();
        }
    }
}

fn resolve_profiles(
    config: &WorkflowConfig,
    candidates: &[polaris_skills::Skill],
    phases: &[Phase],
) -> Result<BTreeMap<Phase, FrozenWorkflowProfile>, String> {
    phases
        .iter()
        .map(|phase| {
            let profile = polaris_skills::workflow_profile::resolve(
                &config.always,
                config
                    .phase_skills
                    .get(phase)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]),
                candidates,
            )
            .map_err(|error| error.to_string())?;
            let entries = profile
                .entries
                .into_iter()
                .map(|entry| WorkflowSkillV1 {
                    id: entry.id,
                    source: entry.source,
                    body_hash: entry.body_hash,
                })
                .collect();
            Ok((
                *phase,
                FrozenWorkflowProfile {
                    body: profile.body,
                    manifest_hash: profile.manifest_hash,
                    ids: profile.ids,
                    entries,
                },
            ))
        })
        .collect()
}

fn build_profile(
    mut state: WorkflowStateV1,
    frozen: &FrozenWorkflowProfile,
    gates: &WorkflowGatesV1,
) -> Result<WorkflowProfile, String> {
    state.skill_manifest_hash = frozen.manifest_hash.clone();
    state.skills = frozen.entries.clone();
    let verification = match &gates.verification {
        None => "missing",
        Some(evidence) if evidence.stale => "stale",
        Some(evidence) if evidence.passed => "passed",
        Some(_) => "failed",
    };
    // `key=value`, matching the environment block's own shape, and the
    // caveat only where it has something to guard. Spelled out in prose on
    // every iteration it cost 28 tokens, of which the parenthetical alone
    // was 8 — and with no grants there is nothing for it to prevent being
    // misread. With grants it is what stops the count reading as that many
    // free passes.
    let grants = gates.external_grants.len();
    let caveat = if grants == 0 { "" } else { " (still gated)" };
    let body = format!(
        "{}\nphase={} spec={} verification={verification} grants={grants}{caveat}\n",
        frozen.body,
        state.phase,
        if gates.approved_spec_hash.is_some() {
            "recorded"
        } else {
            "missing"
        },
    );
    let tokens = crate::budget::count_tokens(&body);
    if tokens > WORKFLOW_TOKEN_LIMIT {
        return Err(format!(
            "段階別skillと状態表示が上限を超えました: {tokens}/{WORKFLOW_TOKEN_LIMIT} tokens"
        ));
    }
    Ok(WorkflowProfile {
        state,
        body,
        ids: frozen.ids.clone(),
        tokens,
    })
}

fn changed_skill_ids(previous: &[WorkflowSkillV1], next: &[WorkflowSkillV1]) -> Vec<String> {
    let mut changed = Vec::new();
    for entry in next.iter().chain(previous) {
        if (!next.contains(entry) || !previous.contains(entry)) && !changed.contains(&entry.id) {
            changed.push(entry.id.clone());
        }
    }
    changed
}
impl Drop for WorkflowTurn {
    fn drop(&mut self) {
        self.controller
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .finish_turn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_gates() -> WorkflowGatesV1 {
        WorkflowGatesV1 {
            phase_gate_schema_version: None,
            ..WorkflowGatesV1::default()
        }
    }

    fn artifact_gates() -> WorkflowGatesV1 {
        WorkflowGatesV1::default()
    }

    fn legacy_workflow(config: WorkflowConfig) -> SessionWorkflow {
        let state = WorkflowStateV1 {
            phase: config.initial_phase,
            ..Default::default()
        };
        SessionWorkflow::restore_with_artifacts(
            config,
            legacy_gates(),
            state,
            WorkflowArtifactGraphV1::default(),
        )
        .unwrap()
    }

    fn approved_contract(
        eligibility: RoutineEligibilityV1,
    ) -> (WorkflowArtifactGraphV1, ArtifactRefV1) {
        let draft = ChangeContractV1::draft(
            "change-1",
            eligibility,
            "workflow transition has no artifact gate",
            "the transition controller only checks revision",
            "implementation requires an approved routine contract",
            vec!["the phase gate matrix passes".into()],
        )
        .unwrap();
        let approved = draft.revised(ArtifactStatusV1::Approved).unwrap();
        let reference = approved.reference();
        let mut graph = WorkflowArtifactGraphV1::default();
        graph.insert_change_contract(&draft).unwrap();
        graph.insert_change_contract(&approved).unwrap();
        (graph, reference)
    }

    #[test]
    fn mutation_authority_requires_current_implement_contract_or_full_task_proof() {
        let (graph, contract) = approved_contract(RoutineEligibilityV1::local_component("core"));
        let mut routine = WorkflowStateV1 {
            phase: Phase::Implement,
            step: Some(WorkflowStepV1::Implement),
            active_change_contract: Some(contract.clone()),
            ..Default::default()
        };
        assert!(mutation_authorized(&routine, &artifact_gates(), &graph));

        routine.phase = Phase::Specify;
        assert!(!mutation_authorized(&routine, &artifact_gates(), &graph));
        routine.phase = Phase::Implement;
        routine.active_change_contract = Some(ArtifactRefV1 {
            revision: contract.revision.saturating_sub(1),
            ..contract.clone()
        });
        assert!(!mutation_authorized(&routine, &artifact_gates(), &graph));

        let fixture = full_spec_fixture(TaskStateV1::Ready, false, false, false);
        let proof = validate_full_spec_for_implementation(
            &fixture.state,
            &fixture.graph,
            &fixture.feature,
            &fixture.plan,
            &fixture.tasks,
            &fixture.decisions,
        )
        .unwrap();
        let mut full = fixture.state;
        full.phase = Phase::Implement;
        full.step = Some(WorkflowStepV1::Implement);
        let gates = WorkflowGatesV1 {
            full_spec: Some(proof),
            ..artifact_gates()
        };
        assert!(mutation_authorized(&full, &gates, &fixture.graph));
    }

    struct FullSpecFixture {
        graph: WorkflowArtifactGraphV1,
        feature: FeatureSpecV1,
        plan: ImplementationPlanV1,
        tasks: TaskGraphV1,
        decisions: Vec<IdeaDecisionV1>,
        state: WorkflowStateV1,
    }

    fn full_spec_fixture(
        task_state: TaskStateV1,
        clarification: bool,
        consistency_error: bool,
        unresolved_decision: bool,
    ) -> FullSpecFixture {
        let mut graph = WorkflowArtifactGraphV1::default();
        let decisions = if unresolved_decision {
            let draft = IdeaDecisionV1::draft(
                "decision-full",
                "Select the implementation boundary",
                vec!["The boundary must be explicit".into()],
                Vec::new(),
                Vec::new(),
                Vec::new(),
                IdeaDecisionOutcomeV1::Clarify,
                vec![UnresolvedDecisionV1 {
                    id: "boundary".into(),
                    question: "Which boundary is current?".into(),
                }],
            )
            .unwrap();
            let approved = draft.revised(ArtifactStatusV1::Approved).unwrap();
            graph.insert_idea_decision(&draft).unwrap();
            graph.insert_idea_decision(&approved).unwrap();
            vec![approved]
        } else {
            Vec::new()
        };
        let requirement = RequirementV1::stable("The active task is exact").unwrap();
        let criterion = VerificationCriterionV1::stable("The gate is enforced", true).unwrap();
        let feature_draft = FeatureSpecV1::draft_with_context(
            "feature-full",
            "Full specification",
            "Gate implementation on exact artifacts",
            vec![requirement.clone()],
            vec![criterion.clone()],
            decisions.iter().map(IdeaDecisionV1::reference).collect(),
            clarification
                .then(|| UnresolvedDecisionV1 {
                    id: "clarify-scope".into(),
                    question: "Which scope is required?".into(),
                })
                .into_iter()
                .collect(),
            consistency_error
                .then(|| "task scope conflicts with feature scope".into())
                .into_iter()
                .collect(),
        )
        .unwrap();
        let feature = feature_draft.revised(ArtifactStatusV1::Approved).unwrap();
        graph.insert_feature_spec(&feature_draft).unwrap();
        graph.insert_feature_spec(&feature).unwrap();
        let plan_draft = ImplementationPlanV1::draft(
            "plan-full",
            feature.reference(),
            "Use the exact task graph",
            vec!["workflow".into()],
        )
        .unwrap();
        let plan = plan_draft.revised(ArtifactStatusV1::Approved).unwrap();
        graph.insert_implementation_plan(&plan_draft).unwrap();
        graph.insert_implementation_plan(&plan).unwrap();
        let task_id = TaskDefinitionV1::stable_id("full implementation task").unwrap();
        let tasks_draft = TaskGraphV1::draft(
            "tasks-full",
            feature.reference(),
            plan.reference(),
            vec![TaskDefinitionV1 {
                id: task_id.clone(),
                title: "Implement the gate".into(),
                scope: vec!["workflow".into()],
                requirement_ids: vec![requirement.id],
                criterion_ids: vec![criterion.id],
                dependencies: Vec::new(),
            }],
            vec![TaskRuntimeV1 {
                task_id: task_id.clone(),
                state: task_state,
                blocker: None,
            }],
        )
        .unwrap();
        let tasks = tasks_draft.revised(ArtifactStatusV1::Approved).unwrap();
        graph.insert_task_graph(&tasks_draft).unwrap();
        graph.insert_task_graph(&tasks).unwrap();
        let state = WorkflowStateV1 {
            phase: Phase::Specify,
            step: Some(WorkflowStepV1::Specify),
            active_feature: Some(feature.reference()),
            active_task: Some(ActiveTaskRefV1 {
                task_graph: tasks.reference(),
                task_id,
                definition_hash: tasks.definition_hash.clone(),
            }),
            ..Default::default()
        };
        FullSpecFixture {
            graph,
            feature,
            plan,
            tasks,
            decisions,
            state,
        }
    }

    #[test]
    fn full_spec_gate_requires_exact_proof_and_rejects_unresolved_inputs() {
        let fixture = full_spec_fixture(TaskStateV1::Ready, false, false, false);
        let proof = validate_full_spec_for_implementation(
            &fixture.state,
            &fixture.graph,
            &fixture.feature,
            &fixture.plan,
            &fixture.tasks,
            &fixture.decisions,
        )
        .unwrap();
        assert_eq!(
            evaluate_phase_transition(
                &fixture.state,
                Phase::Implement,
                &artifact_gates(),
                &fixture.graph,
            )
            .unwrap_err()
            .code,
            TransitionBlockerCodeV1::MissingChangeContract
        );
        let gates = WorkflowGatesV1 {
            full_spec: Some(proof),
            ..artifact_gates()
        };
        assert!(
            evaluate_phase_transition(&fixture.state, Phase::Implement, &gates, &fixture.graph,)
                .is_ok()
        );

        for invalid in [
            full_spec_fixture(TaskStateV1::Ready, true, false, false),
            full_spec_fixture(TaskStateV1::Ready, false, true, false),
            full_spec_fixture(TaskStateV1::Ready, false, false, true),
            full_spec_fixture(TaskStateV1::Completed, false, false, false),
            full_spec_fixture(TaskStateV1::Cancelled, false, false, false),
        ] {
            assert!(
                validate_full_spec_for_implementation(
                    &invalid.state,
                    &invalid.graph,
                    &invalid.feature,
                    &invalid.plan,
                    &invalid.tasks,
                    &invalid.decisions,
                )
                .is_err()
            );
        }
    }

    #[test]
    fn simultaneous_routine_and_full_paths_are_rejected() {
        let mut fixture = full_spec_fixture(TaskStateV1::Ready, false, false, false);
        let (routine_graph, contract) =
            approved_contract(RoutineEligibilityV1::local_component("core"));
        for metadata in routine_graph.artifacts {
            fixture.graph.artifacts.push(metadata);
            fixture.graph.revision += 1;
        }
        fixture.graph.validate().unwrap();
        fixture.state.active_change_contract = Some(contract);
        assert!(validate_active_change_path(&fixture.state, &fixture.graph).is_err());
        assert_eq!(
            evaluate_phase_transition(
                &fixture.state,
                Phase::Implement,
                &artifact_gates(),
                &fixture.graph,
            )
            .unwrap_err()
            .code,
            TransitionBlockerCodeV1::ConflictingChangePaths
        );
    }

    #[test]
    fn artifact_revision_invalidates_full_spec_proof_and_old_active_refs() {
        let mut fixture = full_spec_fixture(TaskStateV1::Ready, false, false, false);
        let proof = validate_full_spec_for_implementation(
            &fixture.state,
            &fixture.graph,
            &fixture.feature,
            &fixture.plan,
            &fixture.tasks,
            &fixture.decisions,
        )
        .unwrap();
        let replacement = fixture.feature.revised(ArtifactStatusV1::Approved).unwrap();
        fixture.graph.insert_feature_spec(&replacement).unwrap();
        let impact = fixture
            .graph
            .stale_impact_after_revision(&replacement.reference())
            .unwrap();
        assert!(impact.gate_proof_stale && impact.evidence_stale);
        let mut gates = WorkflowGatesV1 {
            full_spec: Some(proof),
            ..artifact_gates()
        };
        gates.revalidate_graph(&fixture.graph).unwrap();
        assert!(gates.full_spec.as_ref().unwrap().stale);
        assert!(
            validate_full_spec_for_implementation(
                &fixture.state,
                &fixture.graph,
                &fixture.feature,
                &fixture.plan,
                &fixture.tasks,
                &fixture.decisions,
            )
            .is_err()
        );
        assert_eq!(
            evaluate_phase_transition(&fixture.state, Phase::Implement, &gates, &fixture.graph,)
                .unwrap_err()
                .code,
            TransitionBlockerCodeV1::ConflictingChangePaths
        );
    }

    #[test]
    fn advanced_initial_phases_require_current_evidence() {
        for phase in [Phase::Verify, Phase::Deliver] {
            assert!(
                SessionWorkflow::try_new(WorkflowConfig {
                    initial_phase: phase,
                    ..Default::default()
                })
                .is_err()
            );
            let state = WorkflowStateV1 {
                phase,
                step: phase.default_step(),
                ..Default::default()
            };
            assert_eq!(
                evaluate_phase_transition(
                    &state,
                    phase,
                    &artifact_gates(),
                    &WorkflowArtifactGraphV1::default(),
                )
                .unwrap_err()
                .code,
                TransitionBlockerCodeV1::MissingEvidence
            );
        }
    }

    #[test]
    fn gate_proof_binds_active_feature_and_task_refs() {
        let base = WorkflowStateV1::default();
        let mut focused = base.clone();
        focused.active_feature = Some(ArtifactRefV1 {
            id: "feature-1".into(),
            revision: 1,
            content_hash: "a".repeat(64),
        });
        focused.active_task = Some(ActiveTaskRefV1 {
            task_graph: ArtifactRefV1 {
                id: "tasks-1".into(),
                revision: 1,
                content_hash: "b".repeat(64),
            },
            task_id: "task-1".into(),
            definition_hash: "c".repeat(64),
        });
        let graph = WorkflowArtifactGraphV1::default();
        let gates = legacy_gates();
        assert_ne!(
            gate_proof(&base, Phase::General, &gates, &graph)
                .unwrap()
                .basis_hash,
            gate_proof(&focused, Phase::General, &gates, &graph)
                .unwrap()
                .basis_hash
        );
    }

    #[test]
    fn m27_gate_requires_a_current_approved_routine_contract_to_enter_implement() {
        assert!(
            evaluate_phase_transition(
                &WorkflowStateV1::default(),
                Phase::Review,
                &artifact_gates(),
                &WorkflowArtifactGraphV1::default(),
            )
            .is_ok(),
            "read-only review is a valid entry phase and does not claim implementation completion"
        );
        let state = WorkflowStateV1 {
            phase: Phase::Specify,
            step: Some(WorkflowStepV1::Specify),
            ..Default::default()
        };
        let missing = evaluate_phase_transition(
            &state,
            Phase::Implement,
            &artifact_gates(),
            &WorkflowArtifactGraphV1::default(),
        )
        .unwrap_err();
        assert_eq!(missing.code, TransitionBlockerCodeV1::MissingChangeContract);
        let review_state = WorkflowStateV1 {
            phase: Phase::Review,
            step: Some(WorkflowStepV1::Review),
            ..Default::default()
        };
        assert_eq!(
            evaluate_phase_transition(
                &review_state,
                Phase::Implement,
                &artifact_gates(),
                &WorkflowArtifactGraphV1::default(),
            )
            .unwrap_err()
            .code,
            TransitionBlockerCodeV1::MissingChangeContract
        );

        let (graph, reference) = approved_contract(RoutineEligibilityV1::local_component("core"));
        let state = WorkflowStateV1 {
            active_change_contract: Some(reference),
            ..state
        };
        assert!(
            evaluate_phase_transition(&state, Phase::Implement, &artifact_gates(), &graph).is_ok()
        );
        assert!(
            evaluate_phase_transition(
                &WorkflowStateV1 {
                    active_change_contract: state.active_change_contract.clone(),
                    ..review_state
                },
                Phase::Implement,
                &artifact_gates(),
                &graph,
            )
            .is_ok()
        );
    }

    #[test]
    fn m27_gate_rejects_forward_skips_and_nonroutine_contracts_without_using_grants() {
        let (graph, reference) = approved_contract(RoutineEligibilityV1 {
            security: true,
            ..RoutineEligibilityV1::local_component("core")
        });
        let state = WorkflowStateV1 {
            phase: Phase::Specify,
            step: Some(WorkflowStepV1::Specify),
            active_change_contract: Some(reference),
            ..Default::default()
        };
        let mut gates = artifact_gates();
        gates.external_grants.push(ExternalGrant {
            operation: "write".into(),
            target: "workspace".into(),
            artifact_hash: "a".repeat(64),
        });
        assert_eq!(
            evaluate_phase_transition(&state, Phase::Implement, &gates, &graph)
                .unwrap_err()
                .code,
            TransitionBlockerCodeV1::ChangeRequiresFullSpecification
        );
        assert_eq!(
            evaluate_phase_transition(&state, Phase::Verify, &gates, &graph)
                .unwrap_err()
                .code,
            TransitionBlockerCodeV1::ForwardPhaseSkipped
        );
    }

    #[test]
    fn legacy_workflow_json_keeps_none_step_and_does_not_invent_a_gate() {
        let state: WorkflowStateV1 = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "phase": "implement",
            "revision": 2,
            "skill_manifest_hash": "",
            "skills": []
        }))
        .unwrap();
        let gates: WorkflowGatesV1 = serde_json::from_value(serde_json::json!({
            "schema_version": 1,
            "approved_spec_hash": null,
            "verification": null,
            "external_grants": []
        }))
        .unwrap();
        assert_eq!(state.step, None);
        assert_eq!(state.active_change_contract, None);
        assert_eq!(gates.phase_gate_schema_version, None);
        assert!(
            SessionWorkflow::restore_with_artifacts(
                WorkflowConfig::default(),
                gates,
                state,
                WorkflowArtifactGraphV1::default(),
            )
            .is_ok()
        );
    }

    #[test]
    fn phase_step_mismatch_is_never_restored() {
        assert!(
            SessionWorkflow::restore_with_artifacts(
                WorkflowConfig::default(),
                legacy_gates(),
                WorkflowStateV1 {
                    phase: Phase::Implement,
                    step: Some(WorkflowStepV1::Review),
                    ..Default::default()
                },
                WorkflowArtifactGraphV1::default(),
            )
            .is_err()
        );
    }

    #[test]
    fn required_user_skill_reloads_only_at_turn_boundary_and_missing_file_fails() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("SKILL.md");
        let text = |body: &str| format!("---\nname: rule\ndescription: Test rule\n---\n{body}\n");
        std::fs::write(&path, text("Keep identifiers exact.")).unwrap();
        let skill = polaris_skills::Skill {
            name: "rule".into(),
            description: "Test rule".into(),
            body: "stale catalog text".into(),
            path: path.clone(),
        };
        let runtime = legacy_workflow(WorkflowConfig {
            enabled: true,
            always: vec!["user:rule".into()],
            ..Default::default()
        });
        let first = runtime
            .begin_turn_from_disk(std::slice::from_ref(&skill))
            .unwrap();
        assert!(first.body.contains("Keep identifiers exact."));
        std::fs::write(&path, text("Preserve source evidence.")).unwrap();
        assert!(!first.body.contains("Preserve source evidence."));
        drop(first);
        let second = runtime
            .begin_turn_from_disk(std::slice::from_ref(&skill))
            .unwrap();
        assert!(second.body.contains("Preserve source evidence."));
        assert_eq!(second.changed_skill_ids, vec!["user:rule"]);
        drop(second);
        std::fs::remove_file(&path).unwrap();
        assert!(runtime.begin_turn_from_disk(&[skill]).is_err());
    }
    #[test]
    fn new_session_uses_configured_phase_but_resume_keeps_saved_focus() {
        let config = WorkflowConfig {
            initial_phase: Phase::Review,
            ..Default::default()
        };
        assert_eq!(legacy_workflow(config.clone()).state().phase, Phase::Review);
        let saved = WorkflowStateV1 {
            phase: Phase::Verify,
            revision: 7,
            ..Default::default()
        };
        let runtime = SessionWorkflow::restore(config, legacy_gates(), saved.clone()).unwrap();
        assert_eq!(runtime.state(), saved);
    }

    #[test]
    fn new_gated_session_rejects_implement_without_a_contract_but_allows_review_entry() {
        let config = WorkflowConfig {
            initial_phase: Phase::Implement,
            ..Default::default()
        };
        assert!(SessionWorkflow::try_new(config.clone()).is_err());
        assert_eq!(SessionWorkflow::new(config).state().phase, Phase::Specify);
        assert!(
            SessionWorkflow::try_new(WorkflowConfig {
                initial_phase: Phase::Review,
                ..Default::default()
            })
            .is_ok()
        );
    }
    #[test]
    fn resume_identifies_changed_skill_body_and_source_by_name() {
        let config = WorkflowConfig {
            enabled: true,
            always: vec!["user:rule".into()],
            ..Default::default()
        };
        let mut skill = polaris_skills::Skill {
            name: "rule".into(),
            description: "Rule".into(),
            body: "Keep original facts.".into(),
            path: "/first/rule/SKILL.md".into(),
        };
        let first = legacy_workflow(config.clone());
        let turn = first.begin_turn(&[skill.clone()]).unwrap();
        let saved = turn.state.clone();
        assert!(turn.changed_skill_ids.is_empty());
        drop(turn);
        skill.path = "/second/rule/SKILL.md".into();
        skill.body = "Keep original facts and scope.".into();
        let resumed = SessionWorkflow::restore(config, legacy_gates(), saved.clone()).unwrap();
        let next = resumed.begin_turn(&[skill]).unwrap();
        assert_eq!(next.changed_skill_ids, vec!["user:rule"]);
        assert_ne!(next.state.skill_manifest_hash, saved.skill_manifest_hash);
        assert_eq!(next.state.skills[0].source, "/second/rule/SKILL.md");
    }
    #[test]
    fn shipped_profile_with_gate_diagnostics_is_bounded_and_cancel_safe() {
        for phase in Phase::ALL {
            let mut config = WorkflowConfig {
                enabled: true,
                always: vec!["builtin:workflow-core".into()],
                ..Default::default()
            };
            if phase != Phase::General {
                config
                    .phase_skills
                    .insert(phase, vec![format!("builtin:{phase}")]);
            }
            let runtime = SessionWorkflow::restore(
                config,
                legacy_gates(),
                WorkflowStateV1 {
                    phase,
                    ..Default::default()
                },
            )
            .unwrap();
            let turn = runtime.begin_turn(&[]).unwrap();
            assert!(turn.body.contains("spec=missing"));
            assert!(turn.tokens <= WORKFLOW_TOKEN_LIMIT);
            runtime.request_phase(Phase::Review).unwrap();
            assert_eq!(runtime.state(), turn.state);
            drop(turn);
            let checkpoint = runtime.begin_turn(&[]).unwrap();
            let mut pending = checkpoint
                .prepare_pending(&legacy_gates())
                .unwrap()
                .unwrap();
            pending.begin_persist().unwrap();
            pending.commit().unwrap();
            drop(checkpoint);
            assert_eq!(runtime.begin_turn(&[]).unwrap().state.phase, Phase::Review);
        }
    }
    /// The count alone reads as that many free passes. The caveat is what
    /// says a grant still goes through the ordinary permission checks, and
    /// it is carried only where there is a grant to misread — a line that
    /// says "still gated" beside `grants=0` spends tokens every iteration
    /// guarding nothing.
    #[test]
    fn the_grant_caveat_rides_along_only_when_there_is_a_grant() {
        let build = |grants: Vec<ExternalGrant>| {
            let runtime = SessionWorkflow::restore(
                WorkflowConfig {
                    enabled: true,
                    always: vec!["builtin:workflow-core".into()],
                    ..Default::default()
                },
                WorkflowGatesV1 {
                    external_grants: grants,
                    ..WorkflowGatesV1::default()
                },
                WorkflowStateV1::default(),
            )
            .unwrap();
            runtime.begin_turn(&[]).unwrap().body.clone()
        };

        let none = build(Vec::new());
        assert!(none.contains("grants=0"), "{none}");
        assert!(
            !none.contains("still gated"),
            "nothing to guard, so nothing is carried: {none}"
        );

        let one = build(vec![ExternalGrant {
            operation: "fetch".into(),
            target: "https://example.invalid/".into(),
            artifact_hash: "0".repeat(64),
        }]);
        assert!(one.contains("grants=1"), "{one}");
        assert!(
            one.contains("still gated"),
            "a grant must not read as a free pass: {one}"
        );
    }

    #[test]
    fn focus_is_explicit_and_never_grants_completion() {
        let gates = legacy_gates();
        let mut workflow = Workflow::default();
        workflow.request_phase(Phase::Deliver).unwrap();
        assert_eq!(workflow.state().phase, Phase::Deliver);
        assert!(gates.approved_spec_hash.is_none());
        assert!(gates.verification.is_none());
        assert!(gates.external_grants.is_empty());
        assert!("automatic".parse::<Phase>().is_err());
    }
    #[test]
    fn tool_roundtrips_freeze_focus_and_manifest_until_next_user() {
        let mut workflow = Workflow::default();
        let first = workflow.begin_turn("first".into()).unwrap();
        workflow.request_phase(Phase::Review).unwrap();
        workflow.request_phase(Phase::Verify).unwrap();
        assert_eq!(workflow.state(), &first);
        assert!(workflow.begin_turn("invalid".into()).is_err());
        workflow.finish_turn();
        assert_eq!(workflow.state(), &first);
        let second = workflow.begin_turn("second".into()).unwrap();
        assert_eq!(second.phase, Phase::Verify);
        assert_eq!(second.revision, 1);
        assert_eq!(second.skill_manifest_hash, "second");
    }
    #[test]
    fn adaptive_transition_reserves_then_commits_without_mutating_first() {
        let runtime = legacy_workflow(WorkflowConfig {
            enabled: true,
            mode: WorkflowMode::Adaptive,
            always: vec!["builtin:workflow-core".into()],
            phase_skills: Phase::ALL
                .into_iter()
                .filter(|phase| *phase != Phase::General)
                .map(|phase| (phase, vec![format!("builtin:{phase}")]))
                .collect(),
            ..Default::default()
        });
        let turn = runtime.begin_turn(&[]).unwrap();
        let before = turn.profile(&legacy_gates()).unwrap();
        let mut prepared = turn
            .prepare_transition(Phase::Implement, before.state.revision, &legacy_gates())
            .unwrap();
        assert_eq!(prepared.previous, before.state);
        assert_eq!(prepared.candidate.state.phase, Phase::Implement);
        assert_eq!(runtime.state(), before.state);
        prepared.begin_persist().unwrap();
        let committed = prepared.commit().unwrap();
        assert_eq!(committed.phase, Phase::Implement);
        assert_eq!(committed.revision, 1);
        assert_eq!(turn.profile(&legacy_gates()).unwrap().state, committed);
    }

    #[test]
    fn manual_request_invalidates_unpublished_model_reservation() {
        let runtime = legacy_workflow(WorkflowConfig {
            enabled: true,
            mode: WorkflowMode::Adaptive,
            always: vec!["builtin:workflow-core".into()],
            ..Default::default()
        });
        let turn = runtime.begin_turn(&[]).unwrap();
        let mut prepared = turn
            .prepare_transition(Phase::Review, 0, &legacy_gates())
            .unwrap();
        runtime.request_phase(Phase::Verify).unwrap();
        assert!(prepared.begin_persist().is_err());
        let pending = turn.prepare_pending(&legacy_gates()).unwrap().unwrap();
        assert_eq!(pending.candidate.state.phase, Phase::Verify);
    }

    #[test]
    fn manual_request_during_publish_waits_for_the_next_boundary() {
        let runtime = legacy_workflow(WorkflowConfig {
            enabled: true,
            mode: WorkflowMode::Adaptive,
            always: vec!["builtin:workflow-core".into()],
            ..Default::default()
        });
        let turn = runtime.begin_turn(&[]).unwrap();
        let mut prepared = turn
            .prepare_transition(Phase::Review, 0, &legacy_gates())
            .unwrap();
        prepared.begin_persist().unwrap();
        runtime.request_phase(Phase::Verify).unwrap();
        assert_eq!(prepared.commit().unwrap().phase, Phase::Review);
        let pending = turn.prepare_pending(&legacy_gates()).unwrap().unwrap();
        assert_eq!(pending.candidate.state.phase, Phase::Verify);
    }

    #[test]
    fn profile_does_not_apply_pending_phase_without_checkpoint() {
        let runtime = legacy_workflow(WorkflowConfig {
            enabled: true,
            always: vec!["builtin:workflow-core".into()],
            ..Default::default()
        });
        let turn = runtime.begin_turn(&[]).unwrap();
        runtime.request_phase(Phase::Review).unwrap();
        assert_eq!(
            turn.profile(&legacy_gates()).unwrap().state.phase,
            Phase::General
        );
        let mut pending = turn.prepare_pending(&legacy_gates()).unwrap().unwrap();
        assert_eq!(pending.candidate.state.phase, Phase::Review);
        pending.begin_persist().unwrap();
        pending.commit().unwrap();
        assert_eq!(runtime.state().phase, Phase::Review);
    }

    #[test]
    fn manual_pending_refreshes_only_at_the_next_real_turn_boundary() {
        let runtime = legacy_workflow(WorkflowConfig {
            enabled: true,
            always: vec!["builtin:workflow-core".into()],
            phase_skills: Phase::ALL
                .into_iter()
                .filter(|phase| *phase != Phase::General)
                .map(|phase| (phase, vec![format!("builtin:{phase}")]))
                .collect(),
            ..Default::default()
        });
        let first = runtime.begin_turn(&[]).unwrap();
        let general_body = first.body.clone();
        runtime.request_phase(Phase::Review).unwrap();
        // A request during a real turn never rewrites that turn's prompt.
        assert_eq!(first.profile(&legacy_gates()).unwrap().body, general_body);
        drop(first);

        let mut next = runtime.begin_turn(&[]).unwrap();
        let mut pending = next.prepare_pending(&legacy_gates()).unwrap().unwrap();
        pending.begin_persist().unwrap();
        pending.commit().unwrap();
        next.refresh_manual_profile(&legacy_gates()).unwrap();
        assert_eq!(next.state.phase, Phase::Review);
        assert!(next.body.contains("builtin:review"));

        runtime.request_phase(Phase::Verify).unwrap();
        assert_eq!(next.profile(&legacy_gates()).unwrap().body, next.body);
    }

    #[test]
    fn pending_same_phase_is_a_durable_noop_and_is_consumed() {
        let runtime = legacy_workflow(WorkflowConfig {
            enabled: true,
            always: vec!["builtin:workflow-core".into()],
            ..Default::default()
        });
        let turn = runtime.begin_turn(&[]).unwrap();
        runtime.request_phase(Phase::General).unwrap();
        let pending = turn.prepare_pending(&legacy_gates()).unwrap().unwrap();
        assert!(pending.is_noop());
        let mut pending = pending;
        pending.begin_persist().unwrap();
        pending.commit().unwrap();
        assert!(turn.prepare_pending(&legacy_gates()).unwrap().is_none());
    }

    #[test]
    fn noop_transition_is_invalidated_by_manual_change_before_persisting() {
        let runtime = legacy_workflow(WorkflowConfig {
            enabled: true,
            mode: WorkflowMode::Adaptive,
            always: vec!["builtin:workflow-core".into()],
            ..Default::default()
        });
        let turn = runtime.begin_turn(&[]).unwrap();
        let mut prepared = turn
            .prepare_transition(Phase::General, 0, &legacy_gates())
            .unwrap();
        assert!(prepared.is_noop());
        runtime.request_phase(Phase::Review).unwrap();
        assert!(prepared.begin_persist().is_err());
        assert_eq!(
            turn.prepare_pending(&legacy_gates())
                .unwrap()
                .unwrap()
                .candidate
                .state
                .phase,
            Phase::Review
        );
    }

    #[test]
    fn noop_transition_queues_manual_change_after_persisting() {
        let runtime = legacy_workflow(WorkflowConfig {
            enabled: true,
            mode: WorkflowMode::Adaptive,
            always: vec!["builtin:workflow-core".into()],
            ..Default::default()
        });
        let turn = runtime.begin_turn(&[]).unwrap();
        let mut prepared = turn
            .prepare_transition(Phase::General, 0, &legacy_gates())
            .unwrap();
        prepared.begin_persist().unwrap();
        runtime.request_phase(Phase::Review).unwrap();
        assert_eq!(prepared.commit().unwrap().phase, Phase::General);
        assert_eq!(
            turn.prepare_pending(&legacy_gates())
                .unwrap()
                .unwrap()
                .candidate
                .state
                .phase,
            Phase::Review
        );
    }
    #[test]
    fn fork_drops_grants_and_changed_artifacts_stay_stale() {
        let parent = WorkflowGatesV1 {
            verification: Some(VerificationEvidence {
                artifact_hash: "old".into(),
                passed: true,
                stale: false,
            }),
            external_grants: vec![ExternalGrant {
                operation: "push".into(),
                target: "main".into(),
                artifact_hash: "old".into(),
            }],
            ..Default::default()
        };
        let mut child = parent.for_fork("new");
        assert!(child.external_grants.is_empty());
        child.revalidate_artifact("old");
        assert!(child.verification.unwrap().stale);
        assert!(!parent.verification.unwrap().stale);
        assert_eq!(parent.external_grants.len(), 1);
    }
    #[test]
    fn unsupported_state_is_not_resumed() {
        assert!(
            Workflow::restore(WorkflowStateV1 {
                schema_version: 2,
                ..Default::default()
            })
            .is_err()
        );
    }
}
