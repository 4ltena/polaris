//! Bounded production bridge from immutable workflow artifacts to verification
//! records.
//!
//! The bridge only projects facts already present in exact, validated artifact
//! bodies.  It never turns a judge answer into approval or a phase decision.
//! Owners persist its inputs and feed evidence to the separate convergence
//! authority.

use std::collections::{BTreeMap, BTreeSet};

use polaris_provider::decision::{Decider, Question};
use serde_json::json;
use sha2::{Digest, Sha256};

use crate::{
    common_tests::{CaseReport, RunReport},
    workflow::{
        ActiveTaskRefV1, ArtifactRefV1, ChangeContractV1, CriterionEvidenceKindV1,
        EvidenceBindingV1, EvidenceProvenanceV1, EvidenceRecordV1, EvidenceResultV1,
        EvidenceSourceKindV1, FeatureSpecV1, TaskGraphV1,
    },
};

use super::{
    AllTestsScopeV1, CandidateHintsV1, CandidateSetV1, DecisionUsageV1, EvidenceKindV1,
    PlanDecisionV1, RustTestExecutionOutcomeV1, RustTestExecutionPlanV1, SCHEMA_VERSION,
    SCORE_CUTOFF, ScopeCompletenessV1, StaticCapabilityCatalogV1, StaticEvidenceStateV1,
    StaticEvidenceV1, TestDecisionRecordV1, TestEntryV1, TestInventoryV1, TestScoreAnswerV1,
    TestSelectionV1, VerificationNeedV1, VerificationRouteCandidateV1, VerificationRouteV1,
    candidate_union, stable_id_hash,
    store::{VerificationBindingV1, VerificationRecordRefV1, VerificationStore},
};

const RUST_FRAMEWORK: &str = "rust";
const SCORE_LEVELS: [&str; 4] = [
    "not relevant",
    "weakly related",
    "covers a material part",
    "directly verifies the criterion",
];

/// Exact bodies accepted as the source of verification needs.  The caller
/// cannot supply a prose restatement or an arbitrary criterion ID.
pub enum ActiveVerificationArtifactsV1<'a> {
    RoutineContract {
        active_contract: &'a ArtifactRefV1,
        contract: &'a ChangeContractV1,
    },
    PlannedTask {
        active_feature: &'a ArtifactRefV1,
        active_task: &'a ActiveTaskRefV1,
        feature: &'a FeatureSpecV1,
        task_graph: &'a TaskGraphV1,
    },
}

/// Derive needs directly from exact immutable IDs and criterion descriptions.
/// A planned task with no exact requirement or criterion binding is rejected;
/// guessing that relation would widen verification coverage silently.
pub fn derive_verification_needs(
    active: ActiveVerificationArtifactsV1<'_>,
) -> Result<Vec<VerificationNeedV1>, String> {
    let mut needs = match active {
        ActiveVerificationArtifactsV1::RoutineContract {
            active_contract,
            contract,
        } => {
            active_contract.validate()?;
            contract.validate()?;
            if contract.reference() != *active_contract {
                return Err(
                    "active change contract body does not match its exact reference".into(),
                );
            }
            contract
                .verification
                .iter()
                .filter(|criterion| criterion.required)
                .map(|criterion| VerificationNeedV1 {
                    schema_version: SCHEMA_VERSION,
                    requirement_id: active_contract.id.clone(),
                    task_id: active_contract.id.clone(),
                    criterion_id: criterion.id.clone(),
                    observable_failure: criterion.effective_observable_failure().into(),
                    symbols: Vec::new(),
                    evidence_kinds: criterion_evidence_kinds(criterion),
                })
                .collect()
        }
        ActiveVerificationArtifactsV1::PlannedTask {
            active_feature,
            active_task,
            feature,
            task_graph,
        } => {
            active_feature.validate()?;
            feature.validate()?;
            task_graph.validate()?;
            if feature.reference() != *active_feature || task_graph.feature_spec != *active_feature
            {
                return Err("active feature body does not match the exact task graph".into());
            }
            active_task.validate_against(task_graph)?;
            let (task, _) = task_graph
                .task(&active_task.task_id)
                .ok_or("active task is absent from the exact task graph")?;
            if task.requirement_ids.is_empty() || task.criterion_ids.is_empty() {
                return Err(
                    "active task has no exact requirement/criterion verification binding".into(),
                );
            }
            let criteria: BTreeMap<_, _> = feature
                .acceptance_criteria
                .iter()
                .map(|criterion| (criterion.id.as_str(), criterion))
                .collect();
            let mut derived = Vec::new();
            for requirement_id in &task.requirement_ids {
                for criterion_id in &task.criterion_ids {
                    let criterion = criteria
                        .get(criterion_id.as_str())
                        .ok_or("active task criterion is absent from the exact feature body")?;
                    if criterion.required {
                        derived.push(VerificationNeedV1 {
                            schema_version: SCHEMA_VERSION,
                            requirement_id: requirement_id.clone(),
                            task_id: task.id.clone(),
                            criterion_id: criterion.id.clone(),
                            observable_failure: criterion.effective_observable_failure().into(),
                            symbols: task.scope.clone(),
                            evidence_kinds: criterion_evidence_kinds(criterion),
                        });
                    }
                }
            }
            derived
        }
    };
    needs.sort_by(|left, right| {
        (&left.task_id, &left.requirement_id, &left.criterion_id).cmp(&(
            &right.task_id,
            &right.requirement_id,
            &right.criterion_id,
        ))
    });
    needs.dedup_by(|left, right| {
        left.task_id == right.task_id
            && left.requirement_id == right.requirement_id
            && left.criterion_id == right.criterion_id
    });
    if needs.is_empty() {
        return Err("active artifact has no required verification criteria".into());
    }
    for need in &needs {
        need.validate().map_err(|error| error.to_string())?;
    }
    Ok(needs)
}

fn criterion_evidence_kinds(
    criterion: &crate::workflow::VerificationCriterionV1,
) -> BTreeSet<EvidenceKindV1> {
    criterion
        .effective_evidence_kinds()
        .into_iter()
        .map(|kind| match kind {
            CriterionEvidenceKindV1::Static => EvidenceKindV1::Static,
            CriterionEvidenceKindV1::ExistingTest => EvidenceKindV1::ExistingTest,
            CriterionEvidenceKindV1::CommonCase => EvidenceKindV1::CommonCase,
            CriterionEvidenceKindV1::NewRegression => EvidenceKindV1::NewRegression,
            CriterionEvidenceKindV1::ManualReview => EvidenceKindV1::ManualReview,
        })
        .collect()
}

/// Construct a complete Rust inventory with a content address derived from
/// every entry.  An incomplete listing must use the explicit offline-All
/// path; this function has no partial mode by design.
pub fn build_complete_rust_inventory(entries: Vec<TestEntryV1>) -> Result<TestInventoryV1, String> {
    let inventory = TestInventoryV1::complete_rust(entries).map_err(|error| error.to_string())?;
    if inventory.framework != RUST_FRAMEWORK {
        return Err("Rust inventory framework identity is invalid".into());
    }
    Ok(inventory)
}

/// Persist the exact inputs which the owner already validated.  The returned
/// references are immutable and bound to the caller's project/source hashes.
pub fn persist_inventory_and_candidates(
    store: &VerificationStore,
    binding: &VerificationBindingV1,
    inventory: &TestInventoryV1,
    hints: &CandidateHintsV1,
) -> Result<
    (
        VerificationRecordRefV1,
        CandidateSetV1,
        VerificationRecordRefV1,
    ),
    String,
> {
    binding.validate().map_err(|error| error.to_string())?;
    inventory.validate().map_err(|error| error.to_string())?;
    let candidates = candidate_union(inventory, hints);
    let inventory_ref = store
        .put_inventory(binding, inventory)
        .map_err(|error| error.to_string())?;
    let candidates_ref = store
        .put_candidates(binding, inventory, &candidates)
        .map_err(|error| error.to_string())?;
    Ok((inventory_ref, candidates, candidates_ref))
}

/// The complete result of a single bounded decision batch.  `selection` is
/// safe to execute; `decision` is absent on every judge failure.
pub struct JevSelectionResultV1 {
    pub question_hash: String,
    pub decision: Option<TestDecisionRecordV1>,
    pub decision_ref: Option<VerificationRecordRefV1>,
    pub selection: TestSelectionV1,
}

/// A route result is always deterministic when the Choice answer is absent,
/// malformed, stale, or fails to persist.
pub struct PlanSelectionResultV1 {
    pub question_hash: String,
    pub decision: Option<PlanDecisionV1>,
    pub decision_ref: Option<VerificationRecordRefV1>,
    pub selected: VerificationRouteCandidateV1,
}

/// Select one evidence route with a typed Choice.  Existing tests are the
/// deterministic safe default, followed by common cases, new regression, and
/// manual review. This does not select or narrow existing tests themselves.
pub async fn select_verification_route(
    decider: Option<&dyn Decider>,
    store: &VerificationStore,
    binding: &VerificationBindingV1,
    need: &VerificationNeedV1,
    candidates: &[VerificationRouteCandidateV1],
) -> Result<PlanSelectionResultV1, String> {
    binding.validate().map_err(|error| error.to_string())?;
    let fallback = deterministic_route_fallback(candidates)?;
    let (state, questions, question_hash) = build_route_choice(need, candidates)
        .ok_or("verification route candidates are invalid or empty")?;
    let decision = match decider {
        Some(decider) => {
            let observation = decider.observe(&state, &questions).await;
            observation.result.ok().and_then(|reply| {
                let answer = reply.answers.get("route")?;
                if answer
                    .confidence
                    .is_none_or(|value| value < super::LOW_CONFIDENCE)
                {
                    return None;
                }
                let choice = answer.choice.as_ref()?;
                candidates
                    .iter()
                    .any(|candidate| &candidate.id == choice)
                    .then(|| PlanDecisionV1 {
                        schema_version: SCHEMA_VERSION,
                        record_id: format!(
                            "route-{}",
                            &stable_id_hash(&[question_hash.clone(), binding.diff_hash.clone()])
                                [..16]
                        ),
                        provider: observation.identity.backend.clone(),
                        model: observation
                            .identity
                            .model
                            .clone()
                            .unwrap_or_else(|| "typed-decider".into()),
                        question_hash: question_hash.clone(),
                        diff_hash: binding.diff_hash.clone(),
                        candidate_hash: route_candidate_hash(candidates),
                        selected_candidate_id: choice.clone(),
                        usage: DecisionUsageV1 {
                            input_tokens: reply.input_tokens,
                            output_tokens: reply.output_tokens,
                            micro_usd: None,
                            latency_ms: None,
                            missing_usage: reply.input_tokens.is_none()
                                || reply.output_tokens.is_none(),
                        },
                    })
            })
        }
        None => None,
    };
    let decision_ref = decision
        .as_ref()
        .and_then(|decision| store.put_plan_decision(binding, candidates, decision).ok());
    let selected = decision_ref
        .as_ref()
        .and_then(|reference| {
            store
                .load_plan_decision(reference, binding, candidates)
                .ok()
        })
        .and_then(|decision| {
            candidates
                .iter()
                .find(|candidate| candidate.id == decision.selected_candidate_id)
                .cloned()
        })
        .unwrap_or(fallback);
    Ok(PlanSelectionResultV1 {
        question_hash,
        decision,
        decision_ref,
        selected,
    })
}

fn deterministic_route_fallback(
    candidates: &[VerificationRouteCandidateV1],
) -> Result<VerificationRouteCandidateV1, String> {
    let mut ordered = candidates.to_vec();
    ordered.sort_by(|left, right| {
        route_rank(left.route)
            .cmp(&route_rank(right.route))
            .then(left.id.cmp(&right.id))
    });
    ordered
        .into_iter()
        .next()
        .ok_or("verification route candidates are empty".into())
}

fn route_rank(route: VerificationRouteV1) -> usize {
    VerificationRouteV1::SAFE_ORDER
        .iter()
        .position(|candidate| *candidate == route)
        .expect("all verification routes have a safe rank")
}

fn route_candidate_hash(candidates: &[VerificationRouteCandidateV1]) -> String {
    stable_id_hash(
        &candidates
            .iter()
            .map(|candidate| candidate.id.clone())
            .collect::<Vec<_>>(),
    )
}

fn build_route_choice(
    need: &VerificationNeedV1,
    candidates: &[VerificationRouteCandidateV1],
) -> Option<(serde_json::Value, BTreeMap<String, Question>, String)> {
    need.validate().ok()?;
    if candidates.is_empty()
        || candidates.len() > 255
        || candidates
            .iter()
            .any(|candidate| candidate.id.is_empty() || candidate.source_ref.is_empty())
    {
        return None;
    }
    let mut criteria = BTreeMap::new();
    for candidate in candidates {
        if criteria
            .insert(
                candidate.id.clone(),
                format!("{:?}: {}", candidate.route, candidate.source_ref),
            )
            .is_some()
        {
            return None;
        }
    }
    let state = json!({
        "criterion_id": need.criterion_id,
        "observable_failure": need.observable_failure,
        "candidate_hash": route_candidate_hash(candidates),
    });
    let questions = BTreeMap::from([(
        "route".into(),
        Question::Choice {
            instructions: "Choose the one most direct verification route for this exact criterion."
                .into(),
            criteria,
        },
    )]);
    let question_hash = format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&(state.clone(), &questions)).ok()?)
    );
    Some((state, questions, question_hash))
}

/// Ask one bounded batch of typed Score questions.  A decider answer remains
/// advisory: every malformed, missing, stale, or failed answer resolves via
/// `select_tests_offline` to an explicit All selection.
pub async fn select_with_advisory_jev(
    decider: Option<&dyn Decider>,
    store: &VerificationStore,
    binding: &VerificationBindingV1,
    need: &VerificationNeedV1,
    inventory: &TestInventoryV1,
    candidates: &CandidateSetV1,
    all_scope: AllTestsScopeV1,
) -> JevSelectionResultV1 {
    let (state, questions, question_hash) = build_jev_batch(need, inventory, candidates)
        .unwrap_or_else(|| (json!({}), BTreeMap::new(), String::new()));
    let decision = match decider {
        Some(decider) if !questions.is_empty() && !question_hash.is_empty() => {
            let observation = decider.observe(&state, &questions).await;
            observation.result.ok().and_then(|reply| {
                decision_record(
                    &observation.identity,
                    binding,
                    inventory,
                    candidates,
                    &question_hash,
                    reply,
                )
            })
        }
        _ => None,
    };
    let decision_ref = decision.as_ref().and_then(|record| {
        store
            .put_jev_decision(binding, inventory, candidates, record)
            .ok()
    });
    // Do not use an unpersisted answer.  The later execution can reproduce
    // this selection only from the immutable, hash-bound record.
    let selection = store.select_tests_offline(
        decision_ref.as_ref(),
        binding,
        inventory,
        candidates,
        &question_hash,
        SCORE_CUTOFF,
        all_scope,
    );
    JevSelectionResultV1 {
        question_hash,
        decision,
        decision_ref,
        selection,
    }
}

fn build_jev_batch(
    need: &VerificationNeedV1,
    inventory: &TestInventoryV1,
    candidates: &CandidateSetV1,
) -> Option<(serde_json::Value, BTreeMap<String, Question>, String)> {
    need.validate().ok()?;
    inventory.validate().ok()?;
    candidates.validate_against(inventory).ok()?;
    if !inventory.complete
        || !candidates.inventory_complete
        || candidates.ids.is_empty()
        || candidates.ids.len() > super::MAX_CANDIDATES_PER_NEED
    {
        return None;
    }
    let entries: BTreeMap<_, _> = inventory
        .entries
        .iter()
        .map(|entry| (entry.id.as_str(), entry))
        .collect();
    let mut questions = BTreeMap::new();
    let mut candidate_view = Vec::new();
    for candidate_id in &candidates.ids {
        let entry = entries.get(candidate_id.as_str())?;
        if entry.dynamic_or_unknown {
            return None;
        }
        let key = question_key(candidate_id);
        questions.insert(
            key,
            Question::Score {
                instructions: format!(
                    "Rate whether this exact Rust test directly verifies criterion {}: {}. Test: {}::{}.",
                    need.criterion_id, need.observable_failure, entry.package, entry.exact_name
                ),
                criteria: SCORE_LEVELS.iter().map(|value| (*value).into()).collect(),
            },
        );
        candidate_view.push(json!({
            "id": entry.id,
            "package": entry.package,
            "target": entry.target,
            "exact_name": entry.exact_name,
            "symbols": entry.symbols,
        }));
    }
    let state = json!({
        "schema_version": SCHEMA_VERSION,
        "requirement_id": need.requirement_id,
        "task_id": need.task_id,
        "criterion_id": need.criterion_id,
        "observable_failure": need.observable_failure,
        "candidates": candidate_view,
    });
    let encoded = serde_json::to_vec(&(state.clone(), &questions)).ok()?;
    let question_hash = format!("{:x}", Sha256::digest(encoded));
    Some((state, questions, question_hash))
}

/// Rebuild the exact typed-question identity for a host-provided Jev record.
/// This exposes no model call and does not accept a caller-authored hash.
pub(crate) fn test_selection_question_hash(
    need: &VerificationNeedV1,
    inventory: &TestInventoryV1,
    candidates: &CandidateSetV1,
) -> Option<String> {
    build_jev_batch(need, inventory, candidates).map(|(_, _, hash)| hash)
}

fn question_key(candidate_id: &str) -> String {
    format!("candidate-{}", stable_id_hash(&[candidate_id.into()]))
}

fn decision_record(
    identity: &polaris_provider::decision::DecisionIdentity,
    binding: &VerificationBindingV1,
    inventory: &TestInventoryV1,
    candidates: &CandidateSetV1,
    question_hash: &str,
    reply: polaris_provider::decision::Decision,
) -> Option<TestDecisionRecordV1> {
    binding.validate().ok()?;
    let mut answers = Vec::new();
    for candidate_id in &candidates.ids {
        let answer = reply.answers.get(&question_key(candidate_id))?;
        let score = answer.score?;
        if !score.is_finite() || !(0.0..=3.0).contains(&score) {
            return None;
        }
        if answer.probabilities.len() != SCORE_LEVELS.len()
            || (0..SCORE_LEVELS.len())
                .any(|index| !answer.probabilities.contains_key(&index.to_string()))
            || answer
                .probabilities
                .values()
                .any(|value| !(0.0..=1.0).contains(value))
            || (answer.probabilities.values().sum::<f64>() - 1.0).abs() >= 0.01
        {
            return None;
        }
        let confidence = answer.confidence?;
        answers.push(TestScoreAnswerV1 {
            candidate_id: candidate_id.clone(),
            score,
            probabilities: answer.probabilities.clone(),
            confidence: Some(confidence),
        });
    }
    let record = TestDecisionRecordV1 {
        schema_version: SCHEMA_VERSION,
        record_id: format!(
            "jev-{}",
            &stable_id_hash(&[question_hash.into(), binding.diff_hash.clone()])[..16]
        ),
        provider: identity.backend.clone(),
        model: identity
            .model
            .clone()
            .unwrap_or_else(|| "typed-decider".into()),
        question_hash: question_hash.into(),
        diff_hash: binding.diff_hash.clone(),
        inventory_hash: inventory.inventory_hash.clone(),
        candidate_hash: stable_id_hash(&candidates.ids),
        answers,
        usage: DecisionUsageV1 {
            input_tokens: reply.input_tokens,
            output_tokens: reply.output_tokens,
            micro_usd: None,
            latency_ms: None,
            missing_usage: reply.input_tokens.is_none() || reply.output_tokens.is_none(),
        },
    };
    record.validate().ok()?;
    Some(record)
}

/// Convert only a complete, capability-proven static success to durable
/// workflow evidence.  A static analyzer is not a gate: the owner must still
/// gather all required evidence and evaluate convergence separately.
pub(crate) fn static_satisfied_evidence_record(
    binding: &EvidenceBindingV1,
    criterion_id: &str,
    evidence: &StaticEvidenceV1,
    catalog: &StaticCapabilityCatalogV1,
    observed_at_unix_secs: u64,
) -> Result<EvidenceRecordV1, String> {
    binding.validate()?;
    evidence.validate().map_err(|error| error.to_string())?;
    let capability = catalog
        .resolve(&evidence.analyzer_id)
        .ok_or("static analyzer is not registered")?;
    if criterion_id.is_empty()
        || evidence.obligation_id != criterion_id
        || evidence.source_hash != binding.source_hash
        || evidence.diff_hash != binding.diff_hash
        || evidence.scope_completeness != ScopeCompletenessV1::Complete
        || evidence.effective_state(Some(capability)) != StaticEvidenceStateV1::Satisfied
    {
        return Err("static evidence is not a complete satisfied proof for this criterion".into());
    }
    let record = EvidenceRecordV1 {
        schema_version: crate::workflow::EVIDENCE_SCHEMA_VERSION,
        record_id: evidence_record_id("static", criterion_id, &evidence.evidence_id),
        binding: binding.clone(),
        criterion_id: criterion_id.into(),
        result: EvidenceResultV1::Passed,
        provenance: EvidenceProvenanceV1 {
            source_kind: EvidenceSourceKindV1::StaticAnalysis,
            source_ref: format!("static:{}", evidence.evidence_id),
            observed_at_unix_secs,
        },
    };
    record.validate()?;
    Ok(record)
}

/// Convert a complete common-case report to exactly one criterion record.
/// Convergence accepts one record per criterion, so individual case records
/// would create a false conflict. Command output is never retained.
pub(crate) fn common_case_evidence_records(
    binding: &EvidenceBindingV1,
    criterion_id: &str,
    report: &RunReport,
    observed_at_unix_secs: u64,
) -> Result<Vec<EvidenceRecordV1>, String> {
    binding.validate()?;
    if criterion_id.is_empty() {
        return Err("criterion ID is required for common-case evidence".into());
    }
    let (result, source_ref) = common_case_summary(report);
    let record = EvidenceRecordV1 {
        schema_version: crate::workflow::EVIDENCE_SCHEMA_VERSION,
        record_id: evidence_record_id("common-case", criterion_id, &source_ref),
        binding: binding.clone(),
        criterion_id: criterion_id.into(),
        result,
        provenance: EvidenceProvenanceV1 {
            source_kind: EvidenceSourceKindV1::TestExecution,
            source_ref,
            observed_at_unix_secs,
        },
    };
    record.validate()?;
    Ok(vec![record])
}

/// Seal a host-observed exact-test outcome into one convergence-ready record.
/// The executor is outside this module; it may provide only the typed result
/// bound to the reconstructed execution-plan hash.  Neither argv nor raw
/// output enters workflow evidence.
pub(crate) fn existing_test_evidence_record(
    binding: &EvidenceBindingV1,
    criterion_id: &str,
    plan: &RustTestExecutionPlanV1,
    outcome: &RustTestExecutionOutcomeV1,
    observed_at_unix_secs: u64,
) -> Result<EvidenceRecordV1, String> {
    binding.validate()?;
    if criterion_id.is_empty() {
        return Err("criterion ID is required for existing-test evidence".into());
    }
    outcome
        .validate_for(plan)
        .map_err(|_| "existing-test outcome does not match its execution plan")?;
    let result = match outcome.passed {
        Some(true) if outcome.execution_available && outcome.cleanup_complete => {
            EvidenceResultV1::Passed
        }
        Some(false) if outcome.execution_available && outcome.cleanup_complete => {
            EvidenceResultV1::Failed
        }
        _ => EvidenceResultV1::Unavailable,
    };
    let source_ref = format!("rust-test-plan:{}", plan.plan_hash());
    let record = EvidenceRecordV1 {
        schema_version: crate::workflow::EVIDENCE_SCHEMA_VERSION,
        record_id: evidence_record_id("existing-test", criterion_id, &source_ref),
        binding: binding.clone(),
        criterion_id: criterion_id.into(),
        result,
        provenance: EvidenceProvenanceV1 {
            source_kind: EvidenceSourceKindV1::TestExecution,
            source_ref,
            observed_at_unix_secs,
        },
    };
    record.validate()?;
    Ok(record)
}

fn common_case_summary(report: &RunReport) -> (EvidenceResultV1, String) {
    let (tag, cases, fallback_required, identity, result) = match report {
        RunReport::Passed {
            identity,
            cases,
            fallback_required,
        } if !*fallback_required && !cases.is_empty() && cases.iter().all(|case| case.passed) => (
            "passed",
            cases.as_slice(),
            *fallback_required,
            Some(identity),
            EvidenceResultV1::Passed,
        ),
        RunReport::Failed {
            identity: Some(identity),
            cases,
            fallback_required,
        } if !*fallback_required => (
            "failed",
            cases.as_slice(),
            *fallback_required,
            Some(identity),
            EvidenceResultV1::Failed,
        ),
        RunReport::Passed {
            identity,
            cases,
            fallback_required,
        }
        | RunReport::Failed {
            identity: Some(identity),
            cases,
            fallback_required,
        } => (
            "incomplete",
            cases.as_slice(),
            *fallback_required,
            Some(identity),
            EvidenceResultV1::Unavailable,
        ),
        RunReport::Failed {
            identity: None,
            cases,
            fallback_required,
        } => (
            "unidentified",
            cases.as_slice(),
            *fallback_required,
            None,
            EvidenceResultV1::Unavailable,
        ),
        RunReport::Unavailable {
            identity,
            fallback_required,
            ..
        } => (
            "unavailable",
            &[][..],
            *fallback_required,
            identity.as_ref(),
            EvidenceResultV1::Unavailable,
        ),
    };
    let cases = cases.iter().map(case_summary).collect::<Vec<_>>();
    let bytes = serde_json::to_vec(&json!({
        "tag": tag,
        "cases": cases,
        "fallback_required": fallback_required,
        "identity": identity.map(|identity| json!({
            "plan_hash": identity.plan_hash,
            "program_hash": identity.program_hash,
        })),
    }))
    .expect("bounded common-case summary serializes");
    (
        result,
        format!("common-case-report:{:x}", Sha256::digest(bytes)),
    )
}

fn case_summary(case: &CaseReport) -> serde_json::Value {
    json!({
        "case": case.case,
        "passed": case.passed,
        "status": case.status,
        "output_truncated": case.output_truncated,
        "timed_out": case.timed_out,
    })
}

/// Combine independently observed sources before passing one record per
/// criterion to convergence. Mixed records resolve conservatively.
pub(crate) fn aggregate_criterion_evidence(
    binding: &EvidenceBindingV1,
    criterion_id: &str,
    records: &[EvidenceRecordV1],
    observed_at_unix_secs: u64,
) -> Result<EvidenceRecordV1, String> {
    binding.validate()?;
    if criterion_id.is_empty() || records.is_empty() {
        return Err("criterion evidence aggregation requires at least one exact record".into());
    }
    if records.iter().any(|record| {
        record.validate().is_err()
            || record.binding != *binding
            || record.criterion_id != criterion_id
    }) {
        return Err("criterion evidence aggregation received a mismatched record".into());
    }
    let result = if records
        .iter()
        .any(|record| record.result == EvidenceResultV1::Failed)
    {
        EvidenceResultV1::Failed
    } else if records
        .iter()
        .any(|record| record.result == EvidenceResultV1::Stale)
    {
        EvidenceResultV1::Stale
    } else if records
        .iter()
        .any(|record| record.result == EvidenceResultV1::Unavailable)
    {
        EvidenceResultV1::Unavailable
    } else {
        EvidenceResultV1::Passed
    };
    let mut ids = records
        .iter()
        .map(|record| record.record_id.clone())
        .collect::<Vec<_>>();
    ids.sort();
    let source_ref = format!("aggregate:{}", stable_id_hash(&ids));
    let record = EvidenceRecordV1 {
        schema_version: crate::workflow::EVIDENCE_SCHEMA_VERSION,
        record_id: evidence_record_id("aggregate", criterion_id, &source_ref),
        binding: binding.clone(),
        criterion_id: criterion_id.into(),
        result,
        provenance: EvidenceProvenanceV1 {
            source_kind: EvidenceSourceKindV1::ExternalTool,
            source_ref,
            observed_at_unix_secs,
        },
    };
    record.validate()?;
    Ok(record)
}

fn evidence_record_id(kind: &str, criterion_id: &str, source: &str) -> String {
    format!(
        "evidence-{}",
        &stable_id_hash(&[kind.into(), criterion_id.into(), source.into()])[..24]
    )
}

#[cfg(test)]
mod tests {
    mod provenance;
    use super::*;
    use crate::{
        common_tests::{RunIdentityV1, RunReport},
        verification::{StaticCapabilityV1, select_tests_offline},
        workflow::{
            ArtifactStatusV1, FeatureSpecV1, ImplementationPlanV1, RequirementV1,
            RoutineEligibilityV1, TaskDefinitionV1, TaskGraphV1, TaskRuntimeV1, TaskStateV1,
            VerificationCriterionV1,
        },
    };

    fn hash(byte: char) -> String {
        std::iter::repeat_n(byte, 64).collect()
    }
    fn binding() -> EvidenceBindingV1 {
        EvidenceBindingV1 {
            task_id: "task-1".into(),
            task_definition_hash: hash('a'),
            subject_hash: hash('b'),
            source_hash: hash('c'),
            diff_hash: hash('d'),
            artifact_graph_basis_hash: hash('e'),
        }
    }
    fn entry(id: &str) -> TestEntryV1 {
        TestEntryV1 {
            id: id.into(),
            framework: "rust".into(),
            package: "core".into(),
            target: "lib".into(),
            exact_name: format!("suite::{id}"),
            source_location: Some(super::super::SourceLocationV1 {
                path: "src/lib.rs".into(),
                line: 1,
            }),
            dynamic_or_unknown: false,
            cost: super::super::TestCostClassV1::Fast,
            symbols: BTreeSet::new(),
            common_case_ids: BTreeSet::new(),
        }
    }
    fn contract() -> ChangeContractV1 {
        ChangeContractV1::draft(
            "contract-1",
            RoutineEligibilityV1::local_component("core"),
            "symptom",
            "cause",
            "outcome",
            vec!["criterion text".into()],
        )
        .unwrap()
        .revised(ArtifactStatusV1::Approved)
        .unwrap()
    }

    #[test]
    fn needs_preserve_exact_contract_ids() {
        let contract = contract();
        let needs = derive_verification_needs(ActiveVerificationArtifactsV1::RoutineContract {
            active_contract: &contract.reference(),
            contract: &contract,
        })
        .unwrap();
        assert_eq!(needs.len(), 1);
        assert_eq!(needs[0].task_id, contract.header.id);
        assert_eq!(needs[0].criterion_id, contract.verification[0].id);
        assert_eq!(
            needs[0].observable_failure,
            contract.verification[0].description
        );
    }

    #[test]
    fn planned_needs_preserve_exact_task_requirement_and_criterion_ids() {
        let requirement = RequirementV1::stable("requirement").unwrap();
        let criterion = VerificationCriterionV1::typed(
            "criterion description",
            "observable failure",
            true,
            [CriterionEvidenceKindV1::ExistingTest]
                .into_iter()
                .collect(),
        )
        .unwrap();
        let feature = FeatureSpecV1::draft(
            "feature-1",
            "title",
            "summary",
            vec![requirement.clone()],
            vec![criterion.clone()],
        )
        .unwrap();
        let plan = ImplementationPlanV1::draft(
            "plan-1",
            feature.reference(),
            "approach",
            vec!["scope".into()],
        )
        .unwrap();
        let task_id = TaskDefinitionV1::stable_id("one").unwrap();
        let graph = TaskGraphV1::draft(
            "graph-1",
            feature.reference(),
            plan.reference(),
            vec![TaskDefinitionV1 {
                id: task_id.clone(),
                title: "task".into(),
                scope: vec!["scope".into()],
                requirement_ids: vec![requirement.id.clone()],
                criterion_ids: vec![criterion.id.clone()],
                dependencies: Vec::new(),
            }],
            vec![TaskRuntimeV1 {
                task_id: task_id.clone(),
                state: TaskStateV1::Ready,
                blocker: None,
            }],
        )
        .unwrap();
        let active = ActiveTaskRefV1 {
            task_graph: graph.reference(),
            task_id: task_id.clone(),
            definition_hash: graph.definition_hash.clone(),
        };
        let needs = derive_verification_needs(ActiveVerificationArtifactsV1::PlannedTask {
            active_feature: &feature.reference(),
            active_task: &active,
            feature: &feature,
            task_graph: &graph,
        })
        .unwrap();
        assert_eq!(needs.len(), 1);
        assert_eq!(needs[0].task_id, task_id);
        assert_eq!(needs[0].requirement_id, requirement.id);
        assert_eq!(needs[0].criterion_id, criterion.id);
        assert_eq!(needs[0].observable_failure, "observable failure");
    }

    #[test]
    fn complete_inventory_is_deterministic_and_partial_falls_back_to_all() {
        let first = build_complete_rust_inventory(vec![entry("z"), entry("a")]).unwrap();
        let second = build_complete_rust_inventory(vec![entry("a"), entry("z")]).unwrap();
        assert_eq!(first.inventory_hash, second.inventory_hash);
        let mut partial = first.clone();
        partial.complete = false;
        let candidates = CandidateSetV1 {
            ids: vec!["a".into()],
            mandatory_ids: BTreeSet::new(),
            inventory_complete: false,
        };
        assert!(matches!(
            select_tests_offline(
                None,
                &partial,
                &candidates,
                "q",
                "d",
                SCORE_CUTOFF,
                AllTestsScopeV1::Package
            ),
            TestSelectionV1::All { .. }
        ));
    }

    #[test]
    fn invalid_judge_input_never_creates_an_exact_selection() {
        let inventory = build_complete_rust_inventory(vec![entry("a")]).unwrap();
        let candidates = CandidateSetV1 {
            ids: vec!["a".into()],
            mandatory_ids: BTreeSet::new(),
            inventory_complete: true,
        };
        let contract = contract();
        let need = derive_verification_needs(ActiveVerificationArtifactsV1::RoutineContract {
            active_contract: &contract.reference(),
            contract: &contract,
        })
        .unwrap()
        .remove(0);
        assert!(build_jev_batch(&need, &inventory, &candidates).is_some());
        let selection = select_tests_offline(
            None,
            &inventory,
            &candidates,
            "q",
            "d",
            SCORE_CUTOFF,
            AllTestsScopeV1::Package,
        );
        assert!(matches!(selection, TestSelectionV1::All { .. }));
    }

    #[test]
    fn locationless_complete_entry_can_be_scored_and_selected_exactly() {
        let mut locationless = entry("locationless");
        locationless.source_location = None;
        let inventory = build_complete_rust_inventory(vec![locationless]).unwrap();
        let candidates = CandidateSetV1 {
            ids: vec!["locationless".into()],
            mandatory_ids: BTreeSet::new(),
            inventory_complete: true,
        };
        let contract = contract();
        let need = derive_verification_needs(ActiveVerificationArtifactsV1::RoutineContract {
            active_contract: &contract.reference(),
            contract: &contract,
        })
        .unwrap()
        .remove(0);
        let (_, _, question_hash) = build_jev_batch(&need, &inventory, &candidates).unwrap();
        let record = TestDecisionRecordV1 {
            schema_version: SCHEMA_VERSION,
            record_id: "decision".into(),
            provider: "jev".into(),
            model: "typed-decider".into(),
            question_hash: question_hash.clone(),
            diff_hash: "d".into(),
            inventory_hash: inventory.inventory_hash.clone(),
            candidate_hash: stable_id_hash(&candidates.ids),
            answers: vec![TestScoreAnswerV1 {
                candidate_id: "locationless".into(),
                score: 3.0,
                probabilities: [
                    ("0".into(), 0.0),
                    ("1".into(), 0.0),
                    ("2".into(), 0.0),
                    ("3".into(), 1.0),
                ]
                .into_iter()
                .collect(),
                confidence: Some(1.0),
            }],
            usage: DecisionUsageV1::default(),
        };
        assert_eq!(
            select_tests_offline(
                Some(&record),
                &inventory,
                &candidates,
                &question_hash,
                "d",
                SCORE_CUTOFF,
                AllTestsScopeV1::Package,
            ),
            TestSelectionV1::Exact {
                ids: vec!["locationless".into()],
            }
        );
    }

    #[test]
    fn static_only_evidence_preserves_binding() {
        let capability = StaticCapabilityV1 {
            schema_version: SCHEMA_VERSION,
            analyzer_id: "lint".into(),
            executable_hash: hash('f'),
            version: "1".into(),
            output_schema_hash: hash('0'),
            rules: vec![super::super::StaticRuleCapabilityV1 {
                rule_id: "rule".into(),
                obligation_kinds: ["criterion".into()].into_iter().collect(),
                maximum_scope: super::super::AnalysisScopeV1::Workspace,
                clean_proves_satisfied: true,
                requires_project_graph: false,
            }],
        };
        let mut catalog = StaticCapabilityCatalogV1::default();
        catalog.register(capability).unwrap();
        let evidence = StaticEvidenceV1 {
            schema_version: SCHEMA_VERSION,
            evidence_id: "static-1".into(),
            obligation_id: "criterion-1".into(),
            obligation_kind: "criterion".into(),
            analyzer_id: "lint".into(),
            rule_id: "rule".into(),
            tool_hash: hash('f'),
            config_hash: hash('9'),
            source_hash: hash('c'),
            diff_hash: hash('d'),
            scope: super::super::AnalysisScopeV1::Workspace,
            scope_completeness: ScopeCompletenessV1::Complete,
            project_graph_basis: None,
            state: StaticEvidenceStateV1::Satisfied,
            findings: Vec::new(),
            error_kind: None,
            duration_ms: None,
            cache_hit: None,
        };
        let record =
            static_satisfied_evidence_record(&binding(), "criterion-1", &evidence, &catalog, 1)
                .unwrap();
        assert_eq!(record.binding, binding());
        assert_eq!(record.result, EvidenceResultV1::Passed);
    }

    #[test]
    fn common_case_failures_and_unavailability_are_preserved() {
        let cases = vec![CaseReport {
            case: "says",
            passed: false,
            detail: "failure".into(),
            status: Some(1),
            stdout: "secret output omitted".into(),
            stderr: String::new(),
            output_truncated: false,
            timed_out: false,
            observed_paths: Vec::new(),
        }];
        let failed = RunReport::Failed {
            identity: Some(RunIdentityV1 {
                plan_hash: hash('1'),
                program_hash: hash('2'),
            }),
            cases: cases.clone(),
            fallback_required: false,
        };
        let failed_record = common_case_evidence_records(&binding(), "criterion-1", &failed, 1)
            .unwrap()
            .remove(0);
        assert_eq!(failed_record.result, EvidenceResultV1::Failed);
        assert_eq!(failed_record.binding, binding());
        let unavailable = RunReport::Unavailable {
            identity: None,
            reason: "unavailable".into(),
            fallback_required: true,
        };
        let unavailable_record =
            common_case_evidence_records(&binding(), "criterion-1", &unavailable, 1)
                .unwrap()
                .remove(0);
        assert_eq!(unavailable_record.result, EvidenceResultV1::Unavailable);
    }

    #[test]
    fn exact_test_outcome_is_bound_to_the_reconstructed_plan() {
        let inventory = TestInventoryV1::complete_rust(vec![TestEntryV1 {
            id: "test-1".into(),
            framework: "rust".into(),
            package: "core".into(),
            target: "lib".into(),
            exact_name: "suite::one".into(),
            source_location: None,
            dynamic_or_unknown: false,
            cost: super::super::TestCostClassV1::Fast,
            symbols: BTreeSet::new(),
            common_case_ids: BTreeSet::new(),
        }])
        .unwrap();
        let plan = RustTestExecutionPlanV1::build(
            &TestSelectionV1::Exact {
                ids: vec!["test-1".into()],
            },
            &inventory,
            None,
        )
        .unwrap();
        let outcome = RustTestExecutionOutcomeV1 {
            plan_hash: plan.plan_hash(),
            execution_available: true,
            cleanup_complete: true,
            passed: Some(true),
        };
        let record =
            existing_test_evidence_record(&binding(), "criterion-1", &plan, &outcome, 1).unwrap();
        assert_eq!(record.result, EvidenceResultV1::Passed);
        assert!(record.provenance.source_ref.ends_with(&plan.plan_hash()));
    }
}
