//! Pure contracts for final-answer observation and, once separately activated,
//! publication gating.
//!
//! This module owns no provider, store, history, or publication handle.  Its
//! values can describe an observation or a legal host transition, but cannot
//! publish text.  Product wiring remains a separate milestone.

mod protected;
pub mod shadow;

pub use protected::{
    ProtectedDocumentV1, ProtectedSpanKindV1, ProtectedSpanV1, ProtectionError,
    protect_translation_input, restore_translation_output, scan_protected_spans,
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use thiserror::Error;

pub const FINALIZATION_SCHEMA_VERSION: u32 = 1;

fn sha256(bytes: impl AsRef<[u8]>) -> String {
    format!("{:x}", Sha256::digest(bytes.as_ref()))
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn present(value: &str) -> bool {
    !value.trim().is_empty()
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalizationMode {
    #[default]
    Off,
    Shadow,
    Enforce,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalizationSubjectV1 {
    pub schema_version: u32,
    pub project_id: String,
    pub session_id: String,
    pub session_epoch: u64,
    pub lane_id: String,
    pub root_run_id: String,
    pub attempt_id: String,
    pub turn_sequence: u64,
    pub input_receipt_ids: Vec<String>,
    pub attachment_hashes: Vec<String>,
    pub workflow_revision: String,
    pub exact_prompt_hash: String,
}

impl FinalizationSubjectV1 {
    pub fn validate(&self) -> Result<(), FinalizationError> {
        if self.schema_version != FINALIZATION_SCHEMA_VERSION
            || self.session_epoch == 0
            || self.turn_sequence == 0
            || [
                &self.project_id,
                &self.session_id,
                &self.lane_id,
                &self.root_run_id,
                &self.attempt_id,
                &self.workflow_revision,
            ]
            .into_iter()
            .any(|value| !present(value))
            || !valid_hash(&self.exact_prompt_hash)
            || self.input_receipt_ids.is_empty()
            || self.attachment_hashes.iter().any(|hash| !valid_hash(hash))
            || has_empty_or_duplicate(&self.input_receipt_ids)
            || has_empty_or_duplicate(&self.attachment_hashes)
        {
            return Err(FinalizationError::InvalidSubject);
        }
        Ok(())
    }

    pub fn canonical_id(&self) -> Result<String, FinalizationError> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| FinalizationError::InvalidSubject)?;
        Ok(sha256(bytes))
    }
}

fn has_empty_or_duplicate(values: &[String]) -> bool {
    let mut seen = BTreeSet::new();
    values
        .iter()
        .any(|value| !present(value) || !seen.insert(value.as_str()))
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StructuredCitationV1 {
    pub citation_id: String,
    pub destination: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinalizationCandidateV1 {
    pub schema_version: u32,
    pub subject_id: String,
    pub format_version: String,
    pub body: String,
    pub citations: Vec<StructuredCitationV1>,
    pub candidate_hash: String,
}

#[derive(Serialize)]
struct CandidateHashInput<'a> {
    schema_version: u32,
    subject_id: &'a str,
    format_version: &'a str,
    body: &'a str,
    citations: &'a [StructuredCitationV1],
}

impl FinalizationCandidateV1 {
    pub fn new(
        subject: &FinalizationSubjectV1,
        format_version: impl Into<String>,
        body: impl Into<String>,
        citations: Vec<StructuredCitationV1>,
    ) -> Result<Self, FinalizationError> {
        let mut candidate = Self {
            schema_version: FINALIZATION_SCHEMA_VERSION,
            subject_id: subject.canonical_id()?,
            format_version: format_version.into(),
            body: body.into(),
            citations,
            candidate_hash: String::new(),
        };
        candidate.candidate_hash = candidate.derived_hash()?;
        candidate.validate()?;
        Ok(candidate)
    }

    pub fn validate(&self) -> Result<(), FinalizationError> {
        if self.schema_version != FINALIZATION_SCHEMA_VERSION
            || !valid_hash(&self.subject_id)
            || !present(&self.format_version)
            || self.body.is_empty()
            || has_duplicate_citations(&self.citations)
            || self.citations.iter().any(|citation| {
                !present(&citation.citation_id)
                    || !present(&citation.destination)
                    || !valid_hash(&citation.content_hash)
            })
            || self.candidate_hash != self.derived_hash()?
        {
            return Err(FinalizationError::InvalidCandidate);
        }
        Ok(())
    }

    fn derived_hash(&self) -> Result<String, FinalizationError> {
        let bytes = serde_json::to_vec(&CandidateHashInput {
            schema_version: self.schema_version,
            subject_id: &self.subject_id,
            format_version: &self.format_version,
            body: &self.body,
            citations: &self.citations,
        })
        .map_err(|_| FinalizationError::InvalidCandidate)?;
        Ok(sha256(bytes))
    }
}

fn has_duplicate_citations(citations: &[StructuredCitationV1]) -> bool {
    let mut seen = BTreeSet::new();
    citations
        .iter()
        .any(|citation| !seen.insert(citation.citation_id.as_str()))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranslationDirectionV1 {
    EnToJa,
    JaToEn,
}

impl TranslationDirectionV1 {
    pub const fn source(self) -> LanguageV1 {
        match self {
            Self::EnToJa => LanguageV1::En,
            Self::JaToEn => LanguageV1::Ja,
        }
    }

    pub const fn target(self) -> LanguageV1 {
        match self {
            Self::EnToJa => LanguageV1::Ja,
            Self::JaToEn => LanguageV1::En,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LanguageV1 {
    En,
    Ja,
    Other,
    Mixed,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JudgeExpectationV1 {
    pub stage: JudgeStageV1,
    pub request_hash: String,
    pub profile_hash: String,
    pub candidate_hash: String,
    pub prompt_hash: String,
    pub closed_evidence_hash: String,
    pub required_criterion_ids: Vec<String>,
    pub output_language_criterion_id: Option<String>,
    pub presentation_language: LanguageV1,
    pub translation: Option<TranslationDirectionV1>,
}

impl JudgeExpectationV1 {
    fn validate(&self) -> Result<(), VerdictError> {
        if [
            &self.request_hash,
            &self.profile_hash,
            &self.candidate_hash,
            &self.prompt_hash,
            &self.closed_evidence_hash,
        ]
        .into_iter()
        .any(|hash| !valid_hash(hash))
            || has_empty_or_duplicate(&self.required_criterion_ids)
            || self.required_criterion_ids.is_empty()
            || self
                .output_language_criterion_id
                .as_ref()
                .is_some_and(|id| !self.required_criterion_ids.contains(id))
            || self.translation.is_some() != self.output_language_criterion_id.is_some()
        {
            return Err(VerdictError::InvalidExpectation);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JudgeStageV1 {
    Source,
    Delivered,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelVerdictV1 {
    Pass,
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionStatusV1 {
    Met,
    Unmet,
    NotObservable,
    DeferredToDelivery,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CriterionResultV1 {
    pub criterion_id: String,
    pub status: CriterionStatusV1,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryRequirementV1 {
    DeferredToDelivery,
    NotRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FailureCodeV1 {
    CriterionUnmet,
    FormatMismatch,
    EvidenceUnsupported,
    WrongLanguage,
    IdentityMismatch,
    UsageMissing,
    InvalidResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeIdentityV1 {
    pub request_hash: String,
    pub profile_hash: String,
    pub candidate_hash: String,
    pub prompt_hash: String,
    pub closed_evidence_hash: String,
    pub provider: String,
    pub model: String,
    pub question_version: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct JudgeUsageV1 {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub latency_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceJudgeResultV1 {
    pub verdict: ModelVerdictV1,
    pub criterion_results: Vec<CriterionResultV1>,
    pub source_language: LanguageV1,
    pub delivery_requirement: DeliveryRequirementV1,
    pub format_match: bool,
    pub evidence_support: bool,
    pub failure_codes: Vec<FailureCodeV1>,
    pub identity: JudgeIdentityV1,
    pub usage: Option<JudgeUsageV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveredJudgeResultV1 {
    pub verdict: ModelVerdictV1,
    pub criterion_results: Vec<CriterionResultV1>,
    pub delivery_language: LanguageV1,
    pub format_match: bool,
    pub evidence_support: bool,
    pub failure_codes: Vec<FailureCodeV1>,
    pub identity: JudgeIdentityV1,
    pub usage: Option<JudgeUsageV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "stage", rename_all = "snake_case", deny_unknown_fields)]
pub enum FulfillmentJudgeResultV1 {
    Source(SourceJudgeResultV1),
    Delivered(DeliveredJudgeResultV1),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostVerdictV1 {
    Pass,
    Fail,
}

impl FulfillmentJudgeResultV1 {
    /// Recomputes the verdict from the host-owned criterion set and bindings.
    /// `not_observable`, malformed sets, missing usage, and identity drift are
    /// invalid inputs rather than model failures.
    pub fn host_verdict(
        &self,
        expected: &JudgeExpectationV1,
    ) -> Result<HostVerdictV1, VerdictError> {
        expected.validate()?;
        match (expected.stage, self) {
            (JudgeStageV1::Source, Self::Source(result)) => {
                validate_identity(&result.identity, expected)?;
                if result.usage.is_none() {
                    return Err(VerdictError::UsageMissing);
                }
                validate_criteria(&result.criterion_results, expected)?;
                if result
                    .criterion_results
                    .iter()
                    .any(|item| item.status == CriterionStatusV1::NotObservable)
                {
                    return Err(VerdictError::NotObservable);
                }
                let language_ok = match expected.translation {
                    Some(direction) => {
                        let output = expected
                            .output_language_criterion_id
                            .as_ref()
                            .ok_or(VerdictError::InvalidExpectation)?;
                        result.source_language == direction.source()
                            && result.delivery_requirement
                                == DeliveryRequirementV1::DeferredToDelivery
                            && result.criterion_results.iter().all(|item| {
                                if &item.criterion_id == output {
                                    item.status == CriterionStatusV1::DeferredToDelivery
                                } else {
                                    item.status == CriterionStatusV1::Met
                                }
                            })
                    }
                    None => {
                        result.source_language == expected.presentation_language
                            && result.delivery_requirement == DeliveryRequirementV1::NotRequired
                            && result
                                .criterion_results
                                .iter()
                                .all(|item| item.status == CriterionStatusV1::Met)
                    }
                };
                reconcile_model_verdict(
                    result.verdict,
                    language_ok && result.format_match && result.evidence_support,
                )
            }
            (JudgeStageV1::Delivered, Self::Delivered(result)) => {
                validate_identity(&result.identity, expected)?;
                if result.usage.is_none() {
                    return Err(VerdictError::UsageMissing);
                }
                validate_criteria(&result.criterion_results, expected)?;
                if result.criterion_results.iter().any(|item| {
                    matches!(
                        item.status,
                        CriterionStatusV1::DeferredToDelivery | CriterionStatusV1::NotObservable
                    )
                }) {
                    return Err(VerdictError::IllegalDeliveredCriterion);
                }
                let language = expected
                    .translation
                    .map(TranslationDirectionV1::target)
                    .unwrap_or(expected.presentation_language);
                reconcile_model_verdict(
                    result.verdict,
                    result.delivery_language == language
                        && result
                            .criterion_results
                            .iter()
                            .all(|item| item.status == CriterionStatusV1::Met)
                        && result.format_match
                        && result.evidence_support,
                )
            }
            _ => Err(VerdictError::StageMismatch),
        }
    }
}

fn validate_identity(
    actual: &JudgeIdentityV1,
    expected: &JudgeExpectationV1,
) -> Result<(), VerdictError> {
    if actual.request_hash != expected.request_hash
        || actual.profile_hash != expected.profile_hash
        || actual.candidate_hash != expected.candidate_hash
        || actual.prompt_hash != expected.prompt_hash
        || actual.closed_evidence_hash != expected.closed_evidence_hash
        || !present(&actual.provider)
        || !present(&actual.model)
        || !present(&actual.question_version)
    {
        return Err(VerdictError::IdentityMismatch);
    }
    Ok(())
}

fn validate_criteria(
    actual: &[CriterionResultV1],
    expected: &JudgeExpectationV1,
) -> Result<(), VerdictError> {
    let expected_set = expected
        .required_criterion_ids
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>();
    let actual_set = actual
        .iter()
        .map(|result| result.criterion_id.as_str())
        .collect::<BTreeSet<_>>();
    if actual_set.len() != actual.len() {
        return Err(VerdictError::DuplicateCriterion);
    }
    if actual_set != expected_set {
        return Err(VerdictError::CriterionSetMismatch);
    }
    Ok(())
}

fn reconcile_model_verdict(
    model: ModelVerdictV1,
    derived_pass: bool,
) -> Result<HostVerdictV1, VerdictError> {
    match (model, derived_pass) {
        (ModelVerdictV1::Pass, true) => Ok(HostVerdictV1::Pass),
        (ModelVerdictV1::Fail, false) => Ok(HostVerdictV1::Fail),
        _ => Err(VerdictError::ContradictoryModelVerdict),
    }
}

/// Deterministically derives the delivered result only when no translation or
/// deferred criterion exists and all canonical bindings remain byte-identical.
pub fn derive_untranslated_delivered_result(
    source: &SourceJudgeResultV1,
    source_expected: &JudgeExpectationV1,
    delivered_expected: &JudgeExpectationV1,
) -> Result<DeliveredJudgeResultV1, VerdictError> {
    if source_expected.stage != JudgeStageV1::Source
        || delivered_expected.stage != JudgeStageV1::Delivered
        || source_expected.translation.is_some()
        || delivered_expected.translation.is_some()
        || source_expected.request_hash != delivered_expected.request_hash
        || source_expected.profile_hash != delivered_expected.profile_hash
        || source_expected.candidate_hash != delivered_expected.candidate_hash
        || source_expected.prompt_hash != delivered_expected.prompt_hash
        || source_expected.closed_evidence_hash != delivered_expected.closed_evidence_hash
        || source
            .criterion_results
            .iter()
            .any(|item| item.status == CriterionStatusV1::DeferredToDelivery)
    {
        return Err(VerdictError::SeparateDeliveredJudgeRequired);
    }
    let result = FulfillmentJudgeResultV1::Source(source.clone());
    result.host_verdict(source_expected)?;
    let delivered = DeliveredJudgeResultV1 {
        verdict: source.verdict,
        criterion_results: source.criterion_results.clone(),
        delivery_language: source.source_language,
        format_match: source.format_match,
        evidence_support: source.evidence_support,
        failure_codes: source.failure_codes.clone(),
        identity: source.identity.clone(),
        usage: source.usage.clone(),
    };
    FulfillmentJudgeResultV1::Delivered(delivered.clone()).host_verdict(delivered_expected)?;
    Ok(delivered)
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum VerdictError {
    #[error("judge expectation is invalid")]
    InvalidExpectation,
    #[error("judge stage does not match the request")]
    StageMismatch,
    #[error("judge identity does not match the request")]
    IdentityMismatch,
    #[error("judge usage is missing")]
    UsageMissing,
    #[error("criterion set is missing or contains unknown IDs")]
    CriterionSetMismatch,
    #[error("criterion ID occurs more than once")]
    DuplicateCriterion,
    #[error("criterion was not observable")]
    NotObservable,
    #[error("delivered result contains an illegal criterion status")]
    IllegalDeliveredCriterion,
    #[error("model verdict contradicts the host-derived result")]
    ContradictoryModelVerdict,
    #[error("a physically separate delivered judge is required")]
    SeparateDeliveredJudgeRequired,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FinalizationStateV1 {
    Captured,
    SourceJudging,
    SourcePassed,
    Protecting,
    Translating,
    Restoring,
    IntegrityChecking,
    IntegrityPassed,
    DeliveredJudging,
    ReadyToPublish,
    Published,
    Blocked,
    Cancelled,
}

impl FinalizationStateV1 {
    pub const fn terminal(self) -> bool {
        matches!(self, Self::Published | Self::Blocked | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerFinalizerV1 {
    subject_id: String,
    source_candidate_hash: String,
    closed_evidence_hash: String,
    finalization_plan_hash: String,
    state: FinalizationStateV1,
    visits: BTreeMap<FinalizationStateV1, u8>,
}

impl AnswerFinalizerV1 {
    pub fn capture(
        subject: &FinalizationSubjectV1,
        candidate: &FinalizationCandidateV1,
        closed_evidence_hash: &str,
        finalization_plan_hash: &str,
    ) -> Result<Self, FinalizationError> {
        let subject_id = subject.canonical_id()?;
        candidate.validate()?;
        if candidate.subject_id != subject_id {
            return Err(FinalizationError::StaleBinding);
        }
        if !valid_hash(closed_evidence_hash) || !valid_hash(finalization_plan_hash) {
            return Err(FinalizationError::InvalidCaptureBinding);
        }
        let mut visits = BTreeMap::new();
        visits.insert(FinalizationStateV1::Captured, 1);
        Ok(Self {
            subject_id,
            source_candidate_hash: candidate.candidate_hash.clone(),
            closed_evidence_hash: closed_evidence_hash.into(),
            finalization_plan_hash: finalization_plan_hash.into(),
            state: FinalizationStateV1::Captured,
            visits,
        })
    }

    pub const fn state(&self) -> FinalizationStateV1 {
        self.state
    }

    pub fn subject_id(&self) -> &str {
        &self.subject_id
    }

    pub fn source_candidate_hash(&self) -> &str {
        &self.source_candidate_hash
    }

    pub fn closed_evidence_hash(&self) -> &str {
        &self.closed_evidence_hash
    }

    pub fn finalization_plan_hash(&self) -> &str {
        &self.finalization_plan_hash
    }

    /// Host-only transition validator. Stage visits are one-shot, which also
    /// prevents a caller from expressing an automatic retry as a loop.
    pub fn transition(&mut self, next: FinalizationStateV1) -> Result<(), FinalizationError> {
        if self.state.terminal() {
            return Err(FinalizationError::TerminalState);
        }
        let legal = matches!(
            (self.state, next),
            (
                FinalizationStateV1::Captured,
                FinalizationStateV1::SourceJudging
            ) | (FinalizationStateV1::Captured, FinalizationStateV1::Blocked)
                | (
                    FinalizationStateV1::SourceJudging,
                    FinalizationStateV1::SourcePassed
                )
                | (
                    FinalizationStateV1::SourceJudging,
                    FinalizationStateV1::Blocked
                )
                | (
                    FinalizationStateV1::SourcePassed,
                    FinalizationStateV1::Protecting
                )
                | (
                    FinalizationStateV1::SourcePassed,
                    FinalizationStateV1::IntegrityPassed
                )
                | (
                    FinalizationStateV1::SourcePassed,
                    FinalizationStateV1::Blocked
                )
                | (
                    FinalizationStateV1::Protecting,
                    FinalizationStateV1::Translating
                )
                | (
                    FinalizationStateV1::Protecting,
                    FinalizationStateV1::Blocked
                )
                | (
                    FinalizationStateV1::Translating,
                    FinalizationStateV1::Restoring
                )
                | (
                    FinalizationStateV1::Translating,
                    FinalizationStateV1::DeliveredJudging
                )
                | (
                    FinalizationStateV1::Restoring,
                    FinalizationStateV1::IntegrityChecking
                )
                | (
                    FinalizationStateV1::Restoring,
                    FinalizationStateV1::DeliveredJudging
                )
                | (
                    FinalizationStateV1::IntegrityChecking,
                    FinalizationStateV1::IntegrityPassed
                )
                | (
                    FinalizationStateV1::IntegrityChecking,
                    FinalizationStateV1::DeliveredJudging
                )
                | (
                    FinalizationStateV1::IntegrityPassed,
                    FinalizationStateV1::DeliveredJudging
                )
                | (
                    FinalizationStateV1::IntegrityPassed,
                    FinalizationStateV1::Blocked
                )
                | (
                    FinalizationStateV1::DeliveredJudging,
                    FinalizationStateV1::ReadyToPublish
                )
                | (
                    FinalizationStateV1::DeliveredJudging,
                    FinalizationStateV1::Blocked
                )
                | (
                    FinalizationStateV1::ReadyToPublish,
                    FinalizationStateV1::Published
                )
                | (
                    FinalizationStateV1::ReadyToPublish,
                    FinalizationStateV1::Blocked
                )
        ) || next == FinalizationStateV1::Cancelled;
        if !legal {
            return Err(FinalizationError::IllegalTransition {
                from: self.state,
                to: next,
            });
        }
        if self.visits.contains_key(&next) {
            return Err(FinalizationError::StageRetry(next));
        }
        self.visits.insert(next, 1);
        self.state = next;
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BenchmarkDecisionV1 {
    pub decision_id: String,
    pub profile_hash: String,
    pub passed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompositeActivationV1 {
    pub judge_decision_id: String,
    pub translation_decision_id: Option<String>,
    pub plan_hash: String,
}

pub fn composite_activation_plan_hash(
    judge_profile_hash: &str,
    translation_profile_hash: &str,
    judge_decision_id: &str,
    translation_decision_id: &str,
) -> String {
    sha256(format!(
        "answer-finalizer-composite-v1\0{judge_profile_hash}\0{translation_profile_hash}\0{judge_decision_id}\0{translation_decision_id}"
    ))
}

pub fn validate_mode_activation(
    mode: FinalizationMode,
    judge_profile_hash: &str,
    translation_profile_hash: Option<&str>,
    judge_decision: Option<&BenchmarkDecisionV1>,
    translation_decision: Option<&BenchmarkDecisionV1>,
    composite: Option<&CompositeActivationV1>,
) -> Result<(), ActivationError> {
    if mode != FinalizationMode::Enforce {
        return Ok(());
    }
    let judge = judge_decision.ok_or(ActivationError::EnforceLocked)?;
    if !judge.passed || judge.profile_hash != judge_profile_hash {
        return Err(ActivationError::StaleJudgeDecision);
    }
    match translation_profile_hash {
        None => Ok(()),
        Some(profile_hash) => {
            let translation = translation_decision.ok_or(ActivationError::TranslationLocked)?;
            let composite = composite.ok_or(ActivationError::CompositeLocked)?;
            if !translation.passed || translation.profile_hash != profile_hash {
                return Err(ActivationError::StaleTranslationDecision);
            }
            if composite.judge_decision_id != judge.decision_id
                || composite.translation_decision_id.as_deref()
                    != Some(translation.decision_id.as_str())
                || composite.plan_hash
                    != composite_activation_plan_hash(
                        judge_profile_hash,
                        profile_hash,
                        &judge.decision_id,
                        &translation.decision_id,
                    )
            {
                return Err(ActivationError::CompositeMismatch);
            }
            Ok(())
        }
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum ActivationError {
    #[error("enforce is locked without a pinned passing judge benchmark")]
    EnforceLocked,
    #[error("judge benchmark decision is stale or failed")]
    StaleJudgeDecision,
    #[error("translation is locked without a pinned passing benchmark")]
    TranslationLocked,
    #[error("translation benchmark decision is stale or failed")]
    StaleTranslationDecision,
    #[error("composite activation is absent")]
    CompositeLocked,
    #[error("composite activation does not bind both decisions")]
    CompositeMismatch,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShadowReceiptStageV1 {
    Scheduled,
    Running,
    Completed,
    ShadowUnavailable,
    Cancelled,
    QueueRejected,
    ShadowInterrupted,
}

impl ShadowReceiptStageV1 {
    pub const fn terminal(self) -> bool {
        !matches!(self, Self::Scheduled | Self::Running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowFinalizationReceiptV1 {
    pub subject_id: String,
    pub judge_profile_hash: String,
    pub translation_profile_hash: Option<String>,
    pub source_message_id: String,
    pub stage: ShadowReceiptStageV1,
    pub usage_missing: bool,
}

impl ShadowFinalizationReceiptV1 {
    pub fn transition(&mut self, next: ShadowReceiptStageV1) -> Result<(), FinalizationError> {
        if self.stage.terminal() {
            return Err(FinalizationError::TerminalShadowReceipt);
        }
        let legal = matches!(
            (self.stage, next),
            (
                ShadowReceiptStageV1::Scheduled,
                ShadowReceiptStageV1::Running
            ) | (
                ShadowReceiptStageV1::Scheduled,
                ShadowReceiptStageV1::Cancelled
            ) | (
                ShadowReceiptStageV1::Scheduled,
                ShadowReceiptStageV1::QueueRejected
            ) | (
                ShadowReceiptStageV1::Running,
                ShadowReceiptStageV1::Completed
            ) | (
                ShadowReceiptStageV1::Running,
                ShadowReceiptStageV1::ShadowUnavailable
            ) | (
                ShadowReceiptStageV1::Running,
                ShadowReceiptStageV1::Cancelled
            )
        );
        if !legal {
            return Err(FinalizationError::IllegalShadowTransition);
        }
        self.stage = next;
        Ok(())
    }

    /// Recovery is terminal and deliberately never recreates a job.
    pub fn close_after_restart(&mut self) -> bool {
        self.interrupt_in_flight_after_restart()
    }
}

// `matches!` cannot express an associated two-value pattern. Keep restart
// handling explicit and easy to audit.
impl ShadowFinalizationReceiptV1 {
    pub fn interrupt_in_flight_after_restart(&mut self) -> bool {
        match self.stage {
            ShadowReceiptStageV1::Scheduled | ShadowReceiptStageV1::Running => {
                self.stage = ShadowReceiptStageV1::ShadowInterrupted;
                true
            }
            _ => false,
        }
    }
}

/// Body-free observation. It intentionally has no candidate, publication ID,
/// history record, or mutation callback.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowObservationV1 {
    pub subject_id: String,
    pub source_candidate_hash: String,
    pub source_verdict: Option<HostVerdictV1Serde>,
    pub delivered_verdict: Option<HostVerdictV1Serde>,
    pub translation_attempted: bool,
    pub fallback_used: bool,
    pub integrity_passed: Option<bool>,
    pub usage_missing: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostVerdictV1Serde {
    Pass,
    Fail,
}

impl From<HostVerdictV1> for HostVerdictV1Serde {
    fn from(value: HostVerdictV1) -> Self {
        match value {
            HostVerdictV1::Pass => Self::Pass,
            HostVerdictV1::Fail => Self::Fail,
        }
    }
}

/// Off and shadow must return the already-published source bytes and IDs
/// unchanged. The observation is accepted only as an inert side value.
pub fn preserve_delivery_for_non_enforcing_mode<T: Clone + PartialEq>(
    mode: FinalizationMode,
    published: &T,
    _observation: Option<&ShadowObservationV1>,
) -> Result<T, FinalizationError> {
    match mode {
        FinalizationMode::Off | FinalizationMode::Shadow => Ok(published.clone()),
        FinalizationMode::Enforce => Err(FinalizationError::EnforceCannotUseParityBypass),
    }
}

#[derive(Debug, Error, Clone, PartialEq, Eq)]
pub enum FinalizationError {
    #[error("finalization subject is invalid")]
    InvalidSubject,
    #[error("finalization candidate is invalid")]
    InvalidCandidate,
    #[error("candidate is bound to another subject")]
    StaleBinding,
    #[error("closed evidence or finalization plan hash is invalid")]
    InvalidCaptureBinding,
    #[error("illegal finalization transition from {from:?} to {to:?}")]
    IllegalTransition {
        from: FinalizationStateV1,
        to: FinalizationStateV1,
    },
    #[error("finalization state is terminal")]
    TerminalState,
    #[error("finalization stage may not be retried: {0:?}")]
    StageRetry(FinalizationStateV1),
    #[error("illegal shadow receipt transition")]
    IllegalShadowTransition,
    #[error("shadow receipt is terminal")]
    TerminalShadowReceipt,
    #[error("enforce cannot use the non-enforcing parity bypass")]
    EnforceCannotUseParityBypass,
}

#[cfg(test)]
mod tests;
