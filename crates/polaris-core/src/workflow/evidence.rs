//! Deterministic evidence and convergence records for durable workflow gates.
//!
//! This module deliberately contains no store, provider, or phase-controller
//! dependency.  A trusted owner persists the returned records and is the only
//! component that may act on an `Allowed` decision.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

use super::{
    ActiveTaskRefV1, ArtifactRefV1, ChangeContractV1, FeatureSpecV1, TaskGraphV1,
    WorkflowArtifactGraphV1,
};

pub const EVIDENCE_SCHEMA_VERSION: u32 = 1;
const MAX_ID_BYTES: usize = 128;
const MAX_TEXT_BYTES: usize = 4 * 1024;
const MAX_RECORDS: usize = 256;
const MAX_CRITERIA: usize = 128;
const MAX_FINDINGS_PER_REVIEW: usize = 128;
const MAX_GAPS: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceBindingV1 {
    pub task_id: String,
    pub task_definition_hash: String,
    pub subject_hash: String,
    pub source_hash: String,
    pub diff_hash: String,
    pub artifact_graph_basis_hash: String,
}

impl EvidenceBindingV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_id(&self.task_id, "task")?;
        validate_hash(&self.task_definition_hash, "task definition")?;
        validate_hash(&self.subject_hash, "subject")?;
        validate_hash(&self.source_hash, "source")?;
        validate_hash(&self.diff_hash, "diff")?;
        validate_hash(&self.artifact_graph_basis_hash, "artifact graph basis")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceResultV1 {
    Passed,
    Failed,
    Stale,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceSourceKindV1 {
    StaticAnalysis,
    TestExecution,
    RuntimeObservation,
    ManualInspection,
    ExternalTool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceProvenanceV1 {
    pub source_kind: EvidenceSourceKindV1,
    /// Stable, secret-free source reference.  The referenced body is resolved
    /// by the owner; this pure model never grants access to it.
    pub source_ref: String,
    pub observed_at_unix_secs: u64,
}

impl EvidenceProvenanceV1 {
    fn validate(&self) -> Result<(), String> {
        validate_text(&self.source_ref, "evidence source reference")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRecordV1 {
    pub schema_version: u32,
    pub record_id: String,
    pub binding: EvidenceBindingV1,
    pub criterion_id: String,
    pub result: EvidenceResultV1,
    pub provenance: EvidenceProvenanceV1,
}

impl EvidenceRecordV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_schema(self.schema_version)?;
        validate_id(&self.record_id, "evidence record")?;
        self.binding.validate()?;
        validate_id(&self.criterion_id, "criterion")?;
        self.provenance.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewFindingDispositionV1 {
    Open,
    Resolved,
    Assigned,
    RiskAccepted,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewFindingV1 {
    pub finding_id: String,
    pub disposition: ReviewFindingDispositionV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resolution_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub assigned_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub risk_acceptance_ref: Option<String>,
}

impl ReviewFindingV1 {
    fn validate(&self) -> Result<(), String> {
        validate_id(&self.finding_id, "review finding")?;
        match self.disposition {
            ReviewFindingDispositionV1::Open => {
                if self.resolution_ref.is_some()
                    || self.assigned_task_id.is_some()
                    || self.risk_acceptance_ref.is_some()
                {
                    return Err("open review finding must not carry a disposition reference".into());
                }
            }
            ReviewFindingDispositionV1::Resolved => {
                validate_required_ref(self.resolution_ref.as_deref(), "review resolution")?;
                if self.assigned_task_id.is_some() || self.risk_acceptance_ref.is_some() {
                    return Err("resolved review finding has conflicting disposition fields".into());
                }
            }
            ReviewFindingDispositionV1::Assigned => {
                let task_id = self
                    .assigned_task_id
                    .as_deref()
                    .ok_or("assigned review finding is missing a task")?;
                validate_id(task_id, "assigned task")?;
                if self.resolution_ref.is_some() || self.risk_acceptance_ref.is_some() {
                    return Err("assigned review finding has conflicting disposition fields".into());
                }
            }
            ReviewFindingDispositionV1::RiskAccepted => {
                validate_required_ref(self.risk_acceptance_ref.as_deref(), "risk acceptance")?;
                if self.resolution_ref.is_some() || self.assigned_task_id.is_some() {
                    return Err(
                        "risk accepted review finding has conflicting disposition fields".into(),
                    );
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ReviewRecordV1 {
    pub schema_version: u32,
    pub record_id: String,
    pub binding: EvidenceBindingV1,
    pub findings: Vec<ReviewFindingV1>,
}

impl ReviewRecordV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_schema(self.schema_version)?;
        validate_id(&self.record_id, "review record")?;
        self.binding.validate()?;
        if self.findings.len() > MAX_FINDINGS_PER_REVIEW {
            return Err("review finding count exceeds the limit".into());
        }
        let mut ids = BTreeSet::new();
        for finding in &self.findings {
            finding.validate()?;
            if !ids.insert(&finding.finding_id) {
                return Err("review finding IDs are duplicated".into());
            }
        }
        Ok(())
    }
}

/// A receipt is trusted only after a host-owned store has verified and written
/// it.  The typed authority prevents a model-provided boolean from claiming
/// that status, while the store supplies the real trust boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImplementationReceiptAuthorityV1 {
    TrustedOwner,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImplementationReceiptV1 {
    pub schema_version: u32,
    pub receipt_id: String,
    pub binding: EvidenceBindingV1,
    pub implementation_hash: String,
    pub authority: ImplementationReceiptAuthorityV1,
    pub issued_at_unix_secs: u64,
}

impl ImplementationReceiptV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_schema(self.schema_version)?;
        validate_id(&self.receipt_id, "implementation receipt")?;
        self.binding.validate()?;
        validate_hash(&self.implementation_hash, "implementation")?;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConvergenceInputV1 {
    pub schema_version: u32,
    pub binding: EvidenceBindingV1,
    pub required_criterion_ids: Vec<String>,
    #[serde(default)]
    pub implementation_receipts: Vec<ImplementationReceiptV1>,
    #[serde(default)]
    pub reviews: Vec<ReviewRecordV1>,
    #[serde(default)]
    pub evidence: Vec<EvidenceRecordV1>,
}

impl ConvergenceInputV1 {
    /// Validate record shape and boundedness.  Whether a valid record is
    /// current for this active task remains the evaluator's responsibility.
    pub fn validate(&self) -> Result<(), String> {
        validate_schema(self.schema_version)?;
        self.binding.validate()?;
        if self.required_criterion_ids.len() > MAX_CRITERIA
            || self.implementation_receipts.len() > MAX_RECORDS
            || self.reviews.len() > MAX_RECORDS
            || self.evidence.len() > MAX_RECORDS
        {
            return Err("convergence input exceeds a bounded field limit".into());
        }
        validate_unique_ids(&self.required_criterion_ids, "required criterion")?;
        let mut record_ids = BTreeSet::new();
        for receipt in &self.implementation_receipts {
            receipt.validate()?;
            if !record_ids.insert(receipt.receipt_id.as_str()) {
                return Err("convergence record IDs are duplicated".into());
            }
        }
        for review in &self.reviews {
            review.validate()?;
            if !record_ids.insert(review.record_id.as_str()) {
                return Err("convergence record IDs are duplicated".into());
            }
        }
        for evidence in &self.evidence {
            evidence.validate()?;
            if !record_ids.insert(evidence.record_id.as_str()) {
                return Err("convergence record IDs are duplicated".into());
            }
        }
        Ok(())
    }
}

/// Hashes which are observed by the trusted owner at the time it creates an
/// evidence basis.  Criterion IDs and the graph basis are deliberately absent:
/// they are derived below from immutable active artifact bodies.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ObservedEvidenceSubjectV1 {
    pub subject_hash: String,
    pub source_hash: String,
    pub diff_hash: String,
}

impl ObservedEvidenceSubjectV1 {
    fn validate(&self) -> Result<(), String> {
        validate_hash(&self.subject_hash, "subject")?;
        validate_hash(&self.source_hash, "source")?;
        validate_hash(&self.diff_hash, "diff")
    }
}

/// Exact immutable artifacts loaded by a trusted store owner for the current
/// workflow.  This is intentionally borrowed and non-serializable: a caller
/// cannot persist a claimed basis in place of the owner-resolved bodies.
#[derive(Debug, Clone, Copy)]
pub enum ActiveEvidenceArtifactsV1<'a> {
    RoutineContract {
        artifact_graph: &'a WorkflowArtifactGraphV1,
        active_contract: &'a ArtifactRefV1,
        contract: &'a ChangeContractV1,
    },
    PlannedTask {
        artifact_graph: &'a WorkflowArtifactGraphV1,
        active_feature: &'a ArtifactRefV1,
        active_task: &'a ActiveTaskRefV1,
        feature: &'a FeatureSpecV1,
        task_graph: &'a TaskGraphV1,
    },
}

/// Owner-derived identity and required verification set.  It is the only
/// acceptable basis for a persisted convergence input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnerEvidenceBasisV1 {
    pub binding: EvidenceBindingV1,
    pub required_criterion_ids: Vec<String>,
}

/// A phase-gate input which has been bound to exact active artifacts by a
/// trusted owner.  Its fields are private so stores must obtain it through
/// `validate_owner_convergence` rather than accepting a caller-selected set.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedEvidenceGateInputV1 {
    input: PhaseGateInputV1,
}

impl ValidatedEvidenceGateInputV1 {
    pub fn phase_gate_input(&self) -> &PhaseGateInputV1 {
        &self.input
    }

    pub fn into_phase_gate_input(self) -> PhaseGateInputV1 {
        self.input
    }
}

/// Derive the complete evidence basis from the exact active immutable bodies.
///
/// There is currently no typed "no verification required" task state.  An
/// empty effective criterion set is therefore always rejected.  This prevents
/// a caller from turning a non-empty task or contract into a vacuous gate.
pub fn derive_owner_evidence_basis(
    active: ActiveEvidenceArtifactsV1<'_>,
    observed: &ObservedEvidenceSubjectV1,
) -> Result<OwnerEvidenceBasisV1, String> {
    observed.validate()?;
    match active {
        ActiveEvidenceArtifactsV1::RoutineContract {
            artifact_graph,
            active_contract,
            contract,
        } => {
            artifact_graph.validate()?;
            artifact_graph.approved_routine_contract(active_contract)?;
            contract.validate()?;
            if contract.reference() != *active_contract {
                return Err(
                    "active change contract body does not match its exact reference".into(),
                );
            }
            let required_criterion_ids = required_contract_criteria(contract)?;
            let basis_hash = artifact_graph.basis_hash()?;
            Ok(OwnerEvidenceBasisV1 {
                binding: EvidenceBindingV1 {
                    task_id: active_contract.id.clone(),
                    task_definition_hash: active_contract.content_hash.clone(),
                    subject_hash: observed.subject_hash.clone(),
                    source_hash: observed.source_hash.clone(),
                    diff_hash: observed.diff_hash.clone(),
                    artifact_graph_basis_hash: basis_hash,
                },
                required_criterion_ids,
            })
        }
        ActiveEvidenceArtifactsV1::PlannedTask {
            artifact_graph,
            active_feature,
            active_task,
            feature,
            task_graph,
        } => {
            artifact_graph.validate()?;
            artifact_graph.validate_active_feature(active_feature)?;
            artifact_graph.validate_active_task(active_task)?;
            feature.validate()?;
            task_graph.validate()?;
            if feature.reference() != *active_feature
                || task_graph.reference() != active_task.task_graph
                || task_graph.feature_spec != *active_feature
            {
                return Err(
                    "active planned-task bodies do not match their exact references".into(),
                );
            }
            active_task.validate_against(task_graph)?;
            let (task, _) = task_graph
                .task(&active_task.task_id)
                .ok_or("active task is absent from the exact task graph body")?;
            let required_criterion_ids = required_task_criteria(task, feature)?;
            let basis_hash = artifact_graph.basis_hash()?;
            Ok(OwnerEvidenceBasisV1 {
                binding: EvidenceBindingV1 {
                    task_id: active_task.task_id.clone(),
                    task_definition_hash: active_task.definition_hash.clone(),
                    subject_hash: observed.subject_hash.clone(),
                    source_hash: observed.source_hash.clone(),
                    diff_hash: observed.diff_hash.clone(),
                    artifact_graph_basis_hash: basis_hash,
                },
                required_criterion_ids,
            })
        }
    }
}

/// Construct a convergence input using owner-derived binding and criteria.
/// Evidence records are accepted only after their binding has been checked
/// against the current exact active artifact graph.
pub fn validate_owner_convergence(
    active: ActiveEvidenceArtifactsV1<'_>,
    observed: &ObservedEvidenceSubjectV1,
    candidate: ConvergenceInputV1,
) -> Result<ConvergenceInputV1, String> {
    let expected = derive_owner_evidence_basis(active, observed)?;
    candidate.validate()?;
    if candidate.binding != expected.binding {
        return Err("convergence binding does not match the current active artifact basis".into());
    }
    if candidate.required_criterion_ids != expected.required_criterion_ids {
        return Err(
            "convergence criteria do not exactly match the active immutable artifacts".into(),
        );
    }
    for receipt in &candidate.implementation_receipts {
        if receipt.binding != expected.binding {
            return Err(
                "implementation receipt is not bound to the current active artifact basis".into(),
            );
        }
    }
    for review in &candidate.reviews {
        if review.binding != expected.binding {
            return Err("review record is not bound to the current active artifact basis".into());
        }
    }
    let allowed = expected
        .required_criterion_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    for record in &candidate.evidence {
        if record.binding != expected.binding {
            return Err("evidence record is not bound to the current active artifact basis".into());
        }
        if !allowed.contains(record.criterion_id.as_str()) {
            return Err(
                "evidence record criterion is outside the active immutable criterion set".into(),
            );
        }
    }
    Ok(candidate)
}

/// Build a convergence input without ever accepting caller-provided binding or
/// criterion IDs.  A trusted owner should use this when it gathers persisted
/// receipts, review records, and evidence for the current active artifacts.
pub fn build_owner_convergence(
    active: ActiveEvidenceArtifactsV1<'_>,
    observed: &ObservedEvidenceSubjectV1,
    implementation_receipts: Vec<ImplementationReceiptV1>,
    reviews: Vec<ReviewRecordV1>,
    evidence: Vec<EvidenceRecordV1>,
) -> Result<ConvergenceInputV1, String> {
    let basis = derive_owner_evidence_basis(active, observed)?;
    let candidate = ConvergenceInputV1 {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        binding: basis.binding,
        required_criterion_ids: basis.required_criterion_ids,
        implementation_receipts,
        reviews,
        evidence,
    };
    validate_owner_convergence(active, observed, candidate)
}

/// Return an owner-validated phase-gate input.  Stores can persist/evaluate
/// this value without accepting any caller-provided binding or criterion set.
pub fn validated_evidence_gate_input(
    transition: WorkflowPhaseGateV1,
    active: ActiveEvidenceArtifactsV1<'_>,
    observed: &ObservedEvidenceSubjectV1,
    candidate: ConvergenceInputV1,
) -> Result<ValidatedEvidenceGateInputV1, String> {
    let convergence = validate_owner_convergence(active, observed, candidate)?;
    Ok(ValidatedEvidenceGateInputV1 {
        input: PhaseGateInputV1 {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            transition,
            convergence,
        },
    })
}

fn required_contract_criteria(contract: &ChangeContractV1) -> Result<Vec<String>, String> {
    let mut criteria = contract
        .verification
        .iter()
        .filter(|criterion| criterion.required)
        .map(|criterion| criterion.id.clone())
        .collect::<Vec<_>>();
    criteria.sort();
    if criteria.is_empty() {
        return Err("active change contract has no required verification criteria".into());
    }
    Ok(criteria)
}

fn required_task_criteria(
    task: &super::TaskDefinitionV1,
    feature: &FeatureSpecV1,
) -> Result<Vec<String>, String> {
    let feature_criteria = feature
        .acceptance_criteria
        .iter()
        .map(|criterion| (criterion.id.as_str(), criterion))
        .collect::<BTreeMap<_, _>>();
    let mut criteria = Vec::new();
    for criterion_id in &task.criterion_ids {
        let criterion = feature_criteria
            .get(criterion_id.as_str())
            .ok_or("active task references a missing feature criterion")?;
        if criterion.required {
            criteria.push(criterion.id.clone());
        }
    }
    criteria.sort();
    criteria.dedup();
    if criteria.is_empty() {
        return Err("active task has no required verification criteria".into());
    }
    Ok(criteria)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConvergenceStateV1 {
    Converged,
    Incomplete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConvergenceGapCodeV1 {
    InvalidInput,
    BindingMismatch,
    DuplicateRecord,
    DuplicateCriterion,
    EvidenceConflict,
    RequiredEvidenceMissing,
    RequiredEvidenceFailed,
    EvidenceStale,
    EvidenceUnavailable,
    MissingReviewRecord,
    OpenReviewFinding,
    MissingImplementationReceipt,
    UntrustedImplementationReceipt,
    ReceiptBindingMismatch,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConvergenceGapV1 {
    pub code: ConvergenceGapCodeV1,
    pub reference: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConvergenceReportV1 {
    pub schema_version: u32,
    pub report_id: String,
    pub binding: EvidenceBindingV1,
    pub basis_hash: String,
    pub state: ConvergenceStateV1,
    pub required_criterion_ids: Vec<String>,
    pub review_record_ids: Vec<String>,
    pub evidence_record_ids: Vec<String>,
    pub gaps: Vec<ConvergenceGapV1>,
}

impl ConvergenceReportV1 {
    pub fn converged(&self) -> bool {
        self.state == ConvergenceStateV1::Converged
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_schema(self.schema_version)?;
        validate_id(&self.report_id, "convergence report")?;
        self.binding.validate()?;
        validate_hash(&self.basis_hash, "convergence basis")?;
        if self.required_criterion_ids.len() > MAX_CRITERIA
            || self.review_record_ids.len() > MAX_RECORDS
            || self.evidence_record_ids.len() > MAX_RECORDS
            || self.gaps.len() > MAX_GAPS
        {
            return Err("convergence report exceeds a bounded field limit".into());
        }
        validate_unique_ids(&self.required_criterion_ids, "required criterion")?;
        validate_unique_ids(&self.review_record_ids, "review record")?;
        validate_unique_ids(&self.evidence_record_ids, "evidence record")?;
        for gap in &self.gaps {
            validate_text(&gap.reference, "convergence gap reference")?;
        }
        if self.converged() && !self.gaps.is_empty() {
            return Err("a converged report cannot contain gaps".into());
        }
        Ok(())
    }
}

/// Recompute convergence from typed inputs.  There is intentionally no caller
/// supplied `converged` field in `ConvergenceInputV1`.
pub fn evaluate_convergence(input: &ConvergenceInputV1) -> ConvergenceReportV1 {
    let basis_hash = canonical_basis_hash(input);
    let mut gaps = collect_input_gaps(input);
    let bounded = gaps.is_empty();

    if bounded {
        collect_binding_gaps(input, &mut gaps);
        collect_duplicate_record_gaps(input, &mut gaps);
        collect_review_gaps(input, &mut gaps);
        collect_evidence_gaps(input, &mut gaps);
    }
    normalize_gaps(&mut gaps);

    let mut required_criterion_ids = input.required_criterion_ids.clone();
    required_criterion_ids.sort();
    required_criterion_ids.dedup();
    let mut review_record_ids = input
        .reviews
        .iter()
        .map(|record| record.record_id.clone())
        .collect::<Vec<_>>();
    review_record_ids.sort();
    review_record_ids.dedup();
    let mut evidence_record_ids = input
        .evidence
        .iter()
        .map(|record| record.record_id.clone())
        .collect::<Vec<_>>();
    evidence_record_ids.sort();
    evidence_record_ids.dedup();
    let report_id = format!("convergence-{}", &basis_hash[..20]);

    ConvergenceReportV1 {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        report_id,
        binding: input.binding.clone(),
        basis_hash,
        state: if gaps.is_empty() {
            ConvergenceStateV1::Converged
        } else {
            ConvergenceStateV1::Incomplete
        },
        required_criterion_ids,
        review_record_ids,
        evidence_record_ids,
        gaps,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowPhaseGateV1 {
    ImplementToReview,
    ReviewToVerify,
    VerifyToDeliver,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseGateInputV1 {
    pub schema_version: u32,
    pub transition: WorkflowPhaseGateV1,
    pub convergence: ConvergenceInputV1,
}

impl PhaseGateInputV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_schema(self.schema_version)?;
        self.convergence.validate()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PhaseGateBlockerCodeV1 {
    MissingTrustedReceipt,
    MissingReviewRecord,
    ReviewFindingsOpen,
    ConvergenceIncomplete,
    InvalidEvidenceInput,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseGateBlockerV1 {
    pub schema_version: u32,
    pub transition: WorkflowPhaseGateV1,
    pub code: PhaseGateBlockerCodeV1,
    pub references: Vec<String>,
    pub next_task: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhaseGateProofV1 {
    pub schema_version: u32,
    pub transition: WorkflowPhaseGateV1,
    pub binding: EvidenceBindingV1,
    pub basis_hash: String,
    pub convergence_report_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "status", content = "value")]
pub enum PhaseGateEvaluationV1 {
    Allowed(PhaseGateProofV1),
    Blocked(PhaseGateBlockerV1),
}

/// Evaluate a phase edge from persisted records.  The caller cannot turn an
/// incomplete report into an allowed gate by providing a flag: the report is
/// recomputed for every call.
pub fn evaluate_phase_gate(input: &PhaseGateInputV1) -> PhaseGateEvaluationV1 {
    let report = evaluate_convergence(&input.convergence);
    let blocker = |code, references: Vec<String>, next_task: &str| {
        PhaseGateEvaluationV1::Blocked(PhaseGateBlockerV1 {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            transition: input.transition,
            code,
            references,
            next_task: next_task.into(),
        })
    };

    if input.schema_version != EVIDENCE_SCHEMA_VERSION {
        return blocker(
            PhaseGateBlockerCodeV1::InvalidEvidenceInput,
            vec!["phase_gate_schema_version".into()],
            "Recover the phase gate storage format",
        );
    }

    match input.transition {
        WorkflowPhaseGateV1::ImplementToReview => {
            let valid = input
                .convergence
                .implementation_receipts
                .iter()
                .any(|receipt| {
                    receipt.validate().is_ok()
                        && receipt.binding == input.convergence.binding
                        && receipt.authority == ImplementationReceiptAuthorityV1::TrustedOwner
                });
            if !valid {
                let code = if input.convergence.implementation_receipts.is_empty() {
                    PhaseGateBlockerCodeV1::MissingTrustedReceipt
                } else {
                    PhaseGateBlockerCodeV1::InvalidEvidenceInput
                };
                return blocker(
                    code,
                    vec!["implementation_receipt".into()],
                    "Pin the recorded implementation outcome with a trusted owner",
                );
            }
        }
        WorkflowPhaseGateV1::ReviewToVerify => {
            if input.convergence.reviews.is_empty() {
                return blocker(
                    PhaseGateBlockerCodeV1::MissingReviewRecord,
                    vec!["review_record".into()],
                    "Save a review record bound to the current basis with a trusted owner",
                );
            }
            let open = report
                .gaps
                .iter()
                .filter(|gap| gap.code == ConvergenceGapCodeV1::OpenReviewFinding)
                .map(|gap| gap.reference.clone())
                .collect::<Vec<_>>();
            if !open.is_empty() {
                return blocker(
                    PhaseGateBlockerCodeV1::ReviewFindingsOpen,
                    open,
                    "Resolve review findings, assign them to tasks, or accept the risk",
                );
            }
            if report.gaps.iter().any(|gap| {
                matches!(
                    gap.code,
                    ConvergenceGapCodeV1::InvalidInput
                        | ConvergenceGapCodeV1::BindingMismatch
                        | ConvergenceGapCodeV1::DuplicateRecord
                )
            }) {
                return blocker(
                    PhaseGateBlockerCodeV1::InvalidEvidenceInput,
                    report
                        .gaps
                        .iter()
                        .map(|gap| gap.reference.clone())
                        .collect(),
                    "Recover review record bindings and IDs",
                );
            }
        }
        WorkflowPhaseGateV1::VerifyToDeliver => {
            if !report.converged() {
                return blocker(
                    PhaseGateBlockerCodeV1::ConvergenceIncomplete,
                    report
                        .gaps
                        .iter()
                        .map(|gap| gap.reference.clone())
                        .collect(),
                    "Provide required evidence and convergence against the current basis",
                );
            }
        }
    }

    PhaseGateEvaluationV1::Allowed(PhaseGateProofV1 {
        schema_version: EVIDENCE_SCHEMA_VERSION,
        transition: input.transition,
        binding: input.convergence.binding.clone(),
        basis_hash: report.basis_hash,
        convergence_report_id: report.report_id,
    })
}

fn collect_input_gaps(input: &ConvergenceInputV1) -> Vec<ConvergenceGapV1> {
    let mut gaps = Vec::new();
    if input.schema_version != EVIDENCE_SCHEMA_VERSION {
        gaps.push(gap(ConvergenceGapCodeV1::InvalidInput, "schema_version"));
    }
    if input.binding.validate().is_err() {
        gaps.push(gap(ConvergenceGapCodeV1::InvalidInput, "binding"));
    }
    if input.required_criterion_ids.len() > MAX_CRITERIA
        || input.implementation_receipts.len() > MAX_RECORDS
        || input.reviews.len() > MAX_RECORDS
        || input.evidence.len() > MAX_RECORDS
    {
        gaps.push(gap(ConvergenceGapCodeV1::InvalidInput, "record_limit"));
        return gaps;
    }
    for id in &input.required_criterion_ids {
        if validate_id(id, "required criterion").is_err() {
            gaps.push(gap(ConvergenceGapCodeV1::InvalidInput, id));
        }
    }
    gaps
}

fn collect_binding_gaps(input: &ConvergenceInputV1, gaps: &mut Vec<ConvergenceGapV1>) {
    for receipt in &input.implementation_receipts {
        if receipt.validate().is_err() {
            gaps.push(gap(ConvergenceGapCodeV1::InvalidInput, &receipt.receipt_id));
        } else if receipt.binding != input.binding {
            gaps.push(gap(
                ConvergenceGapCodeV1::ReceiptBindingMismatch,
                &receipt.receipt_id,
            ));
        }
    }
    for record in &input.reviews {
        if record.validate().is_err() {
            gaps.push(gap(ConvergenceGapCodeV1::InvalidInput, &record.record_id));
        } else if record.binding != input.binding {
            gaps.push(gap(
                ConvergenceGapCodeV1::BindingMismatch,
                &record.record_id,
            ));
        }
    }
    for record in &input.evidence {
        if record.validate().is_err() {
            gaps.push(gap(ConvergenceGapCodeV1::InvalidInput, &record.record_id));
        } else if record.binding != input.binding {
            gaps.push(gap(
                ConvergenceGapCodeV1::BindingMismatch,
                &record.record_id,
            ));
        }
    }
}

fn collect_duplicate_record_gaps(input: &ConvergenceInputV1, gaps: &mut Vec<ConvergenceGapV1>) {
    let mut ids = BTreeSet::new();
    for id in input
        .implementation_receipts
        .iter()
        .map(|receipt| receipt.receipt_id.as_str())
        .chain(input.reviews.iter().map(|review| review.record_id.as_str()))
        .chain(
            input
                .evidence
                .iter()
                .map(|evidence| evidence.record_id.as_str()),
        )
    {
        if !ids.insert(id) {
            gaps.push(gap(ConvergenceGapCodeV1::DuplicateRecord, id));
        }
    }
    let mut criteria = BTreeSet::new();
    for id in &input.required_criterion_ids {
        if !criteria.insert(id.as_str()) {
            gaps.push(gap(ConvergenceGapCodeV1::DuplicateCriterion, id));
        }
    }
}

fn collect_review_gaps(input: &ConvergenceInputV1, gaps: &mut Vec<ConvergenceGapV1>) {
    if input.reviews.is_empty() {
        gaps.push(gap(
            ConvergenceGapCodeV1::MissingReviewRecord,
            "review_record",
        ));
        return;
    }
    for review in &input.reviews {
        if review.binding != input.binding || review.validate().is_err() {
            continue;
        }
        for finding in &review.findings {
            if finding.disposition == ReviewFindingDispositionV1::Open {
                gaps.push(gap(
                    ConvergenceGapCodeV1::OpenReviewFinding,
                    &finding.finding_id,
                ));
            }
        }
    }
}

fn collect_evidence_gaps(input: &ConvergenceInputV1, gaps: &mut Vec<ConvergenceGapV1>) {
    let required = input
        .required_criterion_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let mut per_criterion = BTreeMap::<&str, Vec<&EvidenceRecordV1>>::new();
    for record in &input.evidence {
        if record.binding == input.binding && record.validate().is_ok() {
            per_criterion
                .entry(&record.criterion_id)
                .or_default()
                .push(record);
        }
    }
    for criterion in required {
        let Some(records) = per_criterion.get(criterion) else {
            gaps.push(gap(
                ConvergenceGapCodeV1::RequiredEvidenceMissing,
                criterion,
            ));
            continue;
        };
        if records.len() != 1 {
            gaps.push(gap(ConvergenceGapCodeV1::EvidenceConflict, criterion));
            continue;
        }
        match records[0].result {
            EvidenceResultV1::Passed => {}
            EvidenceResultV1::Failed => {
                gaps.push(gap(ConvergenceGapCodeV1::RequiredEvidenceFailed, criterion))
            }
            EvidenceResultV1::Stale => {
                gaps.push(gap(ConvergenceGapCodeV1::EvidenceStale, criterion))
            }
            EvidenceResultV1::Unavailable => {
                gaps.push(gap(ConvergenceGapCodeV1::EvidenceUnavailable, criterion))
            }
        }
    }
}

fn canonical_basis_hash(input: &ConvergenceInputV1) -> String {
    #[derive(Serialize)]
    struct Canonical<'a> {
        schema_version: u32,
        binding: &'a EvidenceBindingV1,
        required_criterion_ids: Vec<&'a str>,
        implementation_receipts: Vec<&'a ImplementationReceiptV1>,
        reviews: Vec<&'a ReviewRecordV1>,
        evidence: Vec<&'a EvidenceRecordV1>,
    }
    let mut criteria = input
        .required_criterion_ids
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>();
    criteria.sort();
    let mut receipts = input.implementation_receipts.iter().collect::<Vec<_>>();
    receipts.sort_by(|left, right| left.receipt_id.cmp(&right.receipt_id));
    let mut reviews = input.reviews.iter().collect::<Vec<_>>();
    reviews.sort_by(|left, right| left.record_id.cmp(&right.record_id));
    let mut evidence = input.evidence.iter().collect::<Vec<_>>();
    evidence.sort_by(|left, right| left.record_id.cmp(&right.record_id));
    let canonical = Canonical {
        schema_version: input.schema_version,
        binding: &input.binding,
        required_criterion_ids: criteria,
        implementation_receipts: receipts,
        reviews,
        evidence,
    };
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    format!("{:x}", Sha256::digest(bytes))
}

fn normalize_gaps(gaps: &mut Vec<ConvergenceGapV1>) {
    gaps.sort_by(|left, right| (left.code, &left.reference).cmp(&(right.code, &right.reference)));
    gaps.dedup();
    gaps.truncate(MAX_GAPS);
}

fn gap(code: ConvergenceGapCodeV1, reference: impl Into<String>) -> ConvergenceGapV1 {
    let mut reference = reference.into();
    if reference.len() > MAX_TEXT_BYTES {
        reference.truncate(MAX_TEXT_BYTES);
    }
    if reference.trim().is_empty() {
        reference = "unknown".into();
    }
    ConvergenceGapV1 { code, reference }
}

fn validate_schema(schema_version: u32) -> Result<(), String> {
    if schema_version == EVIDENCE_SCHEMA_VERSION {
        Ok(())
    } else {
        Err("unsupported evidence schema version".into())
    }
}

fn validate_hash(value: &str, label: &str) -> Result<(), String> {
    if value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err(format!("{label} hash is invalid"))
    }
}

fn validate_id(value: &str, label: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_ID_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        Err(format!("{label} ID is invalid"))
    } else {
        Ok(())
    }
}

fn validate_text(value: &str, label: &str) -> Result<(), String> {
    if value.trim().is_empty() || value.len() > MAX_TEXT_BYTES || value.contains('\0') {
        Err(format!("{label} is invalid"))
    } else {
        Ok(())
    }
}

fn validate_required_ref(value: Option<&str>, label: &str) -> Result<(), String> {
    let value = value.ok_or_else(|| format!("{label} is required"))?;
    validate_text(value, label)
}

fn validate_unique_ids(ids: &[String], label: &str) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for id in ids {
        validate_id(id, label)?;
        if !seen.insert(id) {
            return Err(format!("{label} IDs are duplicated"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{
        ArtifactStatusV1, FeatureSpecV1, ImplementationPlanV1, RequirementV1, TaskDefinitionV1,
        TaskGraphV1, TaskRuntimeV1, TaskStateV1, VerificationCriterionV1, WorkflowArtifactGraphV1,
    };

    fn hash(byte: char) -> String {
        std::iter::repeat_n(byte, 64).collect()
    }

    fn binding() -> EvidenceBindingV1 {
        EvidenceBindingV1 {
            task_id: "task-1".into(),
            task_definition_hash: hash('1'),
            subject_hash: hash('2'),
            source_hash: hash('3'),
            diff_hash: hash('4'),
            artifact_graph_basis_hash: hash('5'),
        }
    }

    fn evidence(id: &str, criterion: &str, result: EvidenceResultV1) -> EvidenceRecordV1 {
        EvidenceRecordV1 {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            record_id: id.into(),
            binding: binding(),
            criterion_id: criterion.into(),
            result,
            provenance: EvidenceProvenanceV1 {
                source_kind: EvidenceSourceKindV1::TestExecution,
                source_ref: "test::case".into(),
                observed_at_unix_secs: 1,
            },
        }
    }

    fn receipt() -> ImplementationReceiptV1 {
        ImplementationReceiptV1 {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            receipt_id: "receipt-1".into(),
            binding: binding(),
            implementation_hash: hash('6'),
            authority: ImplementationReceiptAuthorityV1::TrustedOwner,
            issued_at_unix_secs: 1,
        }
    }

    fn input() -> ConvergenceInputV1 {
        ConvergenceInputV1 {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            binding: binding(),
            required_criterion_ids: vec!["criterion-1".into()],
            implementation_receipts: vec![receipt()],
            reviews: vec![ReviewRecordV1 {
                schema_version: EVIDENCE_SCHEMA_VERSION,
                record_id: "review-baseline".into(),
                binding: binding(),
                findings: Vec::new(),
            }],
            evidence: vec![evidence(
                "evidence-1",
                "criterion-1",
                EvidenceResultV1::Passed,
            )],
        }
    }

    fn observed() -> ObservedEvidenceSubjectV1 {
        ObservedEvidenceSubjectV1 {
            subject_hash: hash('2'),
            source_hash: hash('3'),
            diff_hash: hash('4'),
        }
    }

    fn approved_contract() -> (WorkflowArtifactGraphV1, ChangeContractV1) {
        let draft = ChangeContractV1::draft(
            "change-1",
            crate::workflow::RoutineEligibilityV1::local_component("core"),
            "request",
            "cause",
            "outcome",
            vec!["verification".into()],
        )
        .unwrap();
        let approved = draft.revised(ArtifactStatusV1::Approved).unwrap();
        let mut graph = WorkflowArtifactGraphV1::default();
        graph.insert_change_contract(&draft).unwrap();
        graph.insert_change_contract(&approved).unwrap();
        (graph, approved)
    }

    fn planned_task() -> (
        WorkflowArtifactGraphV1,
        FeatureSpecV1,
        TaskGraphV1,
        crate::workflow::ActiveTaskRefV1,
    ) {
        let requirement = RequirementV1::stable("requirement").unwrap();
        let criterion = VerificationCriterionV1::stable("criterion", true).unwrap();
        let feature_draft = FeatureSpecV1::draft(
            "feature-1",
            "feature",
            "summary",
            vec![requirement.clone()],
            vec![criterion.clone()],
        )
        .unwrap();
        let feature = feature_draft.revised(ArtifactStatusV1::Approved).unwrap();
        let plan_draft = ImplementationPlanV1::draft(
            "plan-1",
            feature.reference(),
            "approach",
            vec!["scope".into()],
        )
        .unwrap();
        let plan = plan_draft.revised(ArtifactStatusV1::Approved).unwrap();
        let task_graph = TaskGraphV1::draft(
            "graph-1",
            feature.reference(),
            plan.reference(),
            vec![TaskDefinitionV1 {
                id: "task-1".into(),
                title: "task".into(),
                scope: vec!["scope".into()],
                requirement_ids: vec![requirement.id],
                criterion_ids: vec![criterion.id],
                dependencies: Vec::new(),
            }],
            vec![TaskRuntimeV1 {
                task_id: "task-1".into(),
                state: TaskStateV1::Ready,
                blocker: None,
            }],
        )
        .unwrap();
        let mut graph = WorkflowArtifactGraphV1::default();
        graph.insert_feature_spec(&feature_draft).unwrap();
        graph.insert_feature_spec(&feature).unwrap();
        graph.insert_implementation_plan(&plan_draft).unwrap();
        graph.insert_implementation_plan(&plan).unwrap();
        graph.insert_task_graph(&task_graph).unwrap();
        let active = crate::workflow::ActiveTaskRefV1 {
            task_graph: task_graph.reference(),
            task_id: "task-1".into(),
            definition_hash: task_graph.definition_hash.clone(),
        };
        (graph, feature, task_graph, active)
    }

    #[test]
    fn same_basis_passed_evidence_converges() {
        let report = evaluate_convergence(&input());
        assert!(report.converged());
        assert!(report.gaps.is_empty());
        report.validate().unwrap();
    }

    #[test]
    fn stale_or_other_task_evidence_does_not_converge() {
        let mut stale = input();
        stale.evidence[0].result = EvidenceResultV1::Stale;
        assert!(!evaluate_convergence(&stale).converged());

        let mut other_task = input();
        other_task.evidence[0].binding.task_id = "task-2".into();
        assert!(!evaluate_convergence(&other_task).converged());
    }

    #[test]
    fn required_missing_and_duplicate_conflict_are_blockers() {
        let mut missing = input();
        missing.evidence.clear();
        assert!(
            evaluate_convergence(&missing)
                .gaps
                .iter()
                .any(|gap| gap.code == ConvergenceGapCodeV1::RequiredEvidenceMissing)
        );

        let mut duplicate = input();
        duplicate.evidence.push(evidence(
            "evidence-2",
            "criterion-1",
            EvidenceResultV1::Passed,
        ));
        assert!(
            evaluate_convergence(&duplicate)
                .gaps
                .iter()
                .any(|gap| gap.code == ConvergenceGapCodeV1::EvidenceConflict)
        );
    }

    #[test]
    fn open_review_blocks_review_to_verify() {
        let mut value = input();
        value.reviews.push(ReviewRecordV1 {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            record_id: "review-1".into(),
            binding: binding(),
            findings: vec![ReviewFindingV1 {
                finding_id: "finding-1".into(),
                disposition: ReviewFindingDispositionV1::Open,
                resolution_ref: None,
                assigned_task_id: None,
                risk_acceptance_ref: None,
            }],
        });
        let decision = evaluate_phase_gate(&PhaseGateInputV1 {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            transition: WorkflowPhaseGateV1::ReviewToVerify,
            convergence: value,
        });
        assert!(matches!(
            decision,
            PhaseGateEvaluationV1::Blocked(PhaseGateBlockerV1 {
                code: PhaseGateBlockerCodeV1::ReviewFindingsOpen,
                ..
            })
        ));
    }

    #[test]
    fn missing_review_record_blocks_review_to_verify() {
        let mut value = input();
        value.reviews.clear();
        let decision = evaluate_phase_gate(&PhaseGateInputV1 {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            transition: WorkflowPhaseGateV1::ReviewToVerify,
            convergence: value,
        });
        assert!(matches!(
            decision,
            PhaseGateEvaluationV1::Blocked(PhaseGateBlockerV1 {
                code: PhaseGateBlockerCodeV1::MissingReviewRecord,
                ..
            })
        ));
    }

    #[test]
    fn stale_receipt_and_forged_converged_field_are_rejected() {
        let mut value = input();
        value.implementation_receipts[0].binding.diff_hash = hash('7');
        let decision = evaluate_phase_gate(&PhaseGateInputV1 {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            transition: WorkflowPhaseGateV1::ImplementToReview,
            convergence: value,
        });
        assert!(matches!(decision, PhaseGateEvaluationV1::Blocked(_)));

        let json = serde_json::json!({
            "schema_version": EVIDENCE_SCHEMA_VERSION,
            "binding": binding(),
            "required_criterion_ids": [],
            "converged": true
        });
        assert!(serde_json::from_value::<ConvergenceInputV1>(json).is_err());
    }

    #[test]
    fn owner_basis_derives_routine_criteria_and_current_graph_only() {
        let (graph, contract) = approved_contract();
        let active = contract.reference();
        let basis = derive_owner_evidence_basis(
            ActiveEvidenceArtifactsV1::RoutineContract {
                artifact_graph: &graph,
                active_contract: &active,
                contract: &contract,
            },
            &observed(),
        )
        .unwrap();
        assert_eq!(basis.binding.task_id, active.id);
        assert_eq!(basis.binding.task_definition_hash, active.content_hash);
        assert_eq!(
            basis.binding.artifact_graph_basis_hash,
            graph.basis_hash().unwrap()
        );
        assert_eq!(
            basis.required_criterion_ids,
            vec![contract.verification[0].id.clone()]
        );

        let mut stale_graph = graph.clone();
        stale_graph.revision += 1;
        assert!(
            derive_owner_evidence_basis(
                ActiveEvidenceArtifactsV1::RoutineContract {
                    artifact_graph: &stale_graph,
                    active_contract: &active,
                    contract: &contract,
                },
                &observed(),
            )
            .is_ok()
        );
        // A changed graph is a new basis rather than an implicitly accepted
        // old basis; the following validation rejects records from graph.
        let candidate = ConvergenceInputV1 {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            binding: basis.binding,
            required_criterion_ids: basis.required_criterion_ids,
            implementation_receipts: Vec::new(),
            reviews: Vec::new(),
            evidence: Vec::new(),
        };
        assert!(
            validate_owner_convergence(
                ActiveEvidenceArtifactsV1::RoutineContract {
                    artifact_graph: &stale_graph,
                    active_contract: &active,
                    contract: &contract,
                },
                &observed(),
                candidate,
            )
            .is_err()
        );
    }

    #[test]
    fn owner_basis_uses_only_active_task_required_feature_criteria() {
        let (graph, feature, task_graph, active_task) = planned_task();
        let active_feature = feature.reference();
        let basis = derive_owner_evidence_basis(
            ActiveEvidenceArtifactsV1::PlannedTask {
                artifact_graph: &graph,
                active_feature: &active_feature,
                active_task: &active_task,
                feature: &feature,
                task_graph: &task_graph,
            },
            &observed(),
        )
        .unwrap();
        assert_eq!(basis.binding.task_id, "task-1");
        assert_eq!(
            basis.binding.task_definition_hash,
            task_graph.definition_hash
        );
        assert_eq!(
            basis.required_criterion_ids,
            task_graph.tasks[0].criterion_ids
        );
    }

    #[test]
    fn owner_validation_rejects_criteria_or_binding_selected_by_caller() {
        let (graph, contract) = approved_contract();
        let active = contract.reference();
        let basis = derive_owner_evidence_basis(
            ActiveEvidenceArtifactsV1::RoutineContract {
                artifact_graph: &graph,
                active_contract: &active,
                contract: &contract,
            },
            &observed(),
        )
        .unwrap();
        let mut narrowed = ConvergenceInputV1 {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            binding: basis.binding.clone(),
            required_criterion_ids: Vec::new(),
            implementation_receipts: Vec::new(),
            reviews: Vec::new(),
            evidence: Vec::new(),
        };
        assert!(
            validate_owner_convergence(
                ActiveEvidenceArtifactsV1::RoutineContract {
                    artifact_graph: &graph,
                    active_contract: &active,
                    contract: &contract,
                },
                &observed(),
                narrowed.clone(),
            )
            .is_err()
        );
        narrowed.required_criterion_ids = basis.required_criterion_ids;
        narrowed.binding.diff_hash = hash('9');
        assert!(
            validate_owner_convergence(
                ActiveEvidenceArtifactsV1::RoutineContract {
                    artifact_graph: &graph,
                    active_contract: &active,
                    contract: &contract,
                },
                &observed(),
                narrowed,
            )
            .is_err()
        );
    }

    #[test]
    fn empty_effective_criteria_fail_closed() {
        let (mut graph, feature, _task_graph, active_task) = planned_task();
        let no_criteria_graph = TaskGraphV1::draft(
            "graph-2",
            feature.reference(),
            graph.latest("plan-1").unwrap().reference(),
            vec![TaskDefinitionV1 {
                id: "task-2".into(),
                title: "task".into(),
                scope: vec!["scope".into()],
                requirement_ids: Vec::new(),
                criterion_ids: Vec::new(),
                dependencies: Vec::new(),
            }],
            vec![TaskRuntimeV1 {
                task_id: "task-2".into(),
                state: TaskStateV1::Ready,
                blocker: None,
            }],
        )
        .unwrap();
        graph.insert_task_graph(&no_criteria_graph).unwrap();
        let active = crate::workflow::ActiveTaskRefV1 {
            task_graph: no_criteria_graph.reference(),
            task_id: "task-2".into(),
            definition_hash: no_criteria_graph.definition_hash.clone(),
        };
        let active_feature = feature.reference();
        assert_ne!(active, active_task);
        assert!(
            derive_owner_evidence_basis(
                ActiveEvidenceArtifactsV1::PlannedTask {
                    artifact_graph: &graph,
                    active_feature: &active_feature,
                    active_task: &active,
                    feature: &feature,
                    task_graph: &no_criteria_graph,
                },
                &observed(),
            )
            .is_err()
        );
    }
}
