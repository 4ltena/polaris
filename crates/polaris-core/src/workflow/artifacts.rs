//! Versioned workflow artifacts. Bodies are immutable; stores publish only
//! validated metadata references into a session marker.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

pub const WORKFLOW_ARTIFACT_SCHEMA_VERSION: u32 = 1;
pub const MAX_WORKFLOW_ARTIFACT_BODY_BYTES: usize = 256 * 1024;
pub const MAX_WORKFLOW_ARTIFACTS: usize = 256;
const MAX_ARTIFACT_ID_BYTES: usize = 128;
const MAX_CONTRACT_FIELD_BYTES: usize = 16 * 1024;
const MAX_VERIFICATION_ITEMS: usize = 32;
const MAX_COMPONENTS: usize = 8;
const MAX_REQUIREMENTS: usize = 128;
const MAX_TASKS: usize = 256;
const MAX_TASK_DEPENDENCIES: usize = 32;
const MAX_TASK_SCOPE_ITEMS: usize = 64;
const MAX_PROJECT_PRINCIPLES: usize = 32;
const MAX_PRINCIPLE_SELECTORS: usize = 64;
const MAX_DECISION_EVIDENCE: usize = 32;
const MAX_DECISION_ALTERNATIVES: usize = 16;
const MAX_DECISION_TRADEOFFS: usize = 32;
const MAX_UNRESOLVED_DECISIONS: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ArtifactStatusV1 {
    Draft,
    Ready,
    Approved,
    Stale,
    Rejected,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowArtifactKindV1 {
    ChangeContract,
    ProjectPrinciples,
    IdeaDecision,
    FeatureSpec,
    ImplementationPlan,
    TaskGraph,
}

/// A project-scoped invariant.  Empty selectors mean that the invariant
/// applies to the entire project; otherwise it is selected by exact feature
/// and/or task IDs without copying host policy into the artifact.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPrincipleV1 {
    pub id: String,
    pub statement: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub feature_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub task_ids: Vec<String>,
}

impl ProjectPrincipleV1 {
    fn validate(&self) -> Result<(), String> {
        validate_artifact_id(&self.id)?;
        if !valid_text(&self.statement, 4096)
            || self.feature_ids.len() > MAX_PRINCIPLE_SELECTORS
            || self.task_ids.len() > MAX_PRINCIPLE_SELECTORS
        {
            return Err("project principleの内容または適用範囲が不正です".into());
        }
        for id in self.feature_ids.iter().chain(&self.task_ids) {
            validate_artifact_id(id)?;
        }
        reject_duplicate_ids(
            self.feature_ids.iter().map(String::as_str),
            "principle feature",
        )?;
        reject_duplicate_ids(self.task_ids.iter().map(String::as_str), "principle task")
    }

    pub fn applies_to(&self, feature_id: Option<&str>, task_id: Option<&str>) -> bool {
        (self.feature_ids.is_empty()
            || feature_id
                .is_some_and(|id| self.feature_ids.iter().any(|candidate| candidate == id)))
            && (self.task_ids.is_empty()
                || task_id.is_some_and(|id| self.task_ids.iter().any(|candidate| candidate == id)))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectPrinciplesV1 {
    pub schema_version: u32,
    pub header: ArtifactHeaderV1,
    pub principles: Vec<ProjectPrincipleV1>,
}

impl ProjectPrinciplesV1 {
    pub fn draft(
        id: impl Into<String>,
        principles: Vec<ProjectPrincipleV1>,
    ) -> Result<Self, String> {
        let mut artifact = Self {
            schema_version: WORKFLOW_ARTIFACT_SCHEMA_VERSION,
            header: new_header(id.into()),
            principles,
        };
        artifact.rehash()?;
        artifact.validate()?;
        Ok(artifact)
    }

    pub fn reference(&self) -> ArtifactRefV1 {
        self.header.reference()
    }

    pub fn revised(&self, status: ArtifactStatusV1) -> Result<Self, String> {
        self.validate()?;
        let mut next = self.clone();
        revise_header(&mut next.header, status)?;
        next.rehash()?;
        next.validate()?;
        Ok(next)
    }

    pub fn applicable_to(
        &self,
        feature_id: Option<&str>,
        task_id: Option<&str>,
    ) -> Vec<&ProjectPrincipleV1> {
        self.principles
            .iter()
            .filter(|item| item.applies_to(feature_id, task_id))
            .collect()
    }

    fn canonical_hash(&self) -> Result<String, String> {
        canonical_artifact_hash("polaris.workflow.project_principles.v1", self)
    }

    fn rehash(&mut self) -> Result<(), String> {
        self.header.content_hash.clear();
        self.header.content_hash = self.canonical_hash()?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_common_artifact(self.schema_version, &self.header)?;
        if self.principles.is_empty() || self.principles.len() > MAX_PROJECT_PRINCIPLES {
            return Err("project principlesの件数が不正です".into());
        }
        for principle in &self.principles {
            principle.validate()?;
        }
        reject_duplicate_ids(
            self.principles.iter().map(|item| item.id.as_str()),
            "project principle",
        )?;
        if self.header.content_hash != self.canonical_hash()? {
            return Err("project principlesのcontent hashが一致しません".into());
        }
        validate_body_size(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum IdeaDecisionOutcomeV1 {
    Go,
    Clarify,
    Stop,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionAlternativeV1 {
    pub id: String,
    pub description: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnresolvedDecisionV1 {
    pub id: String,
    pub question: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdeaDecisionV1 {
    pub schema_version: u32,
    pub header: ArtifactHeaderV1,
    pub problem: String,
    pub evidence_for: Vec<String>,
    pub evidence_against: Vec<String>,
    pub alternatives: Vec<DecisionAlternativeV1>,
    pub tradeoffs: Vec<String>,
    pub decision: IdeaDecisionOutcomeV1,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub unresolved: Vec<UnresolvedDecisionV1>,
}

impl IdeaDecisionV1 {
    #[allow(clippy::too_many_arguments)]
    pub fn draft(
        id: impl Into<String>,
        problem: impl Into<String>,
        evidence_for: Vec<String>,
        evidence_against: Vec<String>,
        alternatives: Vec<DecisionAlternativeV1>,
        tradeoffs: Vec<String>,
        decision: IdeaDecisionOutcomeV1,
        unresolved: Vec<UnresolvedDecisionV1>,
    ) -> Result<Self, String> {
        let mut artifact = Self {
            schema_version: WORKFLOW_ARTIFACT_SCHEMA_VERSION,
            header: new_header(id.into()),
            problem: problem.into(),
            evidence_for,
            evidence_against,
            alternatives,
            tradeoffs,
            decision,
            unresolved,
        };
        artifact.rehash()?;
        artifact.validate()?;
        Ok(artifact)
    }

    pub fn reference(&self) -> ArtifactRefV1 {
        self.header.reference()
    }
    pub fn revised(&self, status: ArtifactStatusV1) -> Result<Self, String> {
        self.validate()?;
        let mut next = self.clone();
        revise_header(&mut next.header, status)?;
        next.rehash()?;
        next.validate()?;
        Ok(next)
    }
    pub fn is_unresolved(&self) -> bool {
        self.decision == IdeaDecisionOutcomeV1::Clarify || !self.unresolved.is_empty()
    }
    fn canonical_hash(&self) -> Result<String, String> {
        canonical_artifact_hash("polaris.workflow.idea_decision.v1", self)
    }
    fn rehash(&mut self) -> Result<(), String> {
        self.header.content_hash.clear();
        self.header.content_hash = self.canonical_hash()?;
        Ok(())
    }
    pub fn validate(&self) -> Result<(), String> {
        validate_common_artifact(self.schema_version, &self.header)?;
        if !valid_text(&self.problem, MAX_CONTRACT_FIELD_BYTES)
            || self.evidence_for.len() > MAX_DECISION_EVIDENCE
            || self.evidence_against.len() > MAX_DECISION_EVIDENCE
            || self.alternatives.len() > MAX_DECISION_ALTERNATIVES
            || self.tradeoffs.len() > MAX_DECISION_TRADEOFFS
            || self.unresolved.len() > MAX_UNRESOLVED_DECISIONS
            || self
                .evidence_for
                .iter()
                .chain(&self.evidence_against)
                .chain(&self.tradeoffs)
                .any(|text| !valid_text(text, 4096))
            || self.alternatives.iter().any(|item| {
                validate_artifact_id(&item.id).is_err() || !valid_text(&item.description, 4096)
            })
            || self.unresolved.iter().any(|item| {
                validate_artifact_id(&item.id).is_err() || !valid_text(&item.question, 4096)
            })
            || (self.decision == IdeaDecisionOutcomeV1::Clarify && self.unresolved.is_empty())
        {
            return Err("idea decisionの内容、未解決事項または上限が不正です".into());
        }
        reject_duplicate_ids(
            self.alternatives.iter().map(|item| item.id.as_str()),
            "decision alternative",
        )?;
        reject_duplicate_ids(
            self.unresolved.iter().map(|item| item.id.as_str()),
            "unresolved decision",
        )?;
        if self.header.content_hash != self.canonical_hash()? {
            return Err("idea decisionのcontent hashが一致しません".into());
        }
        validate_body_size(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequirementV1 {
    pub id: String,
    pub description: String,
}

impl RequirementV1 {
    pub fn stable(description: impl Into<String>) -> Result<Self, String> {
        let description = description.into();
        let id = stable_record_id("req", &description)?;
        Ok(Self { id, description })
    }

    fn validate(&self) -> Result<(), String> {
        validate_artifact_id(&self.id)?;
        if !valid_text(&self.description, 4096) {
            return Err("requirementの説明が不正です".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationCriterionV1 {
    pub id: String,
    pub description: String,
    pub required: bool,
    /// Typed criteria carry the concrete failed observation to verify.  It is
    /// optional only so pre-v1 typed bodies retain their exact canonical bytes
    /// and content hash; callers must treat the absent form conservatively.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observable_failure: Option<String>,
    /// Evidence explicitly permitted for this criterion.  An empty set is
    /// the legacy compatibility form, not permission for static proof.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub evidence_kinds: BTreeSet<CriterionEvidenceKindV1>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CriterionEvidenceKindV1 {
    Static,
    ExistingTest,
    CommonCase,
    NewRegression,
    ManualReview,
}

impl VerificationCriterionV1 {
    pub fn stable(description: impl Into<String>, required: bool) -> Result<Self, String> {
        let description = description.into();
        let id = stable_record_id("criterion", &description)?;
        Ok(Self {
            id,
            description,
            required,
            // Keep the established constructor wire-compatible.  New
            // producers that need static proof use `typed`, which makes the
            // verification contract explicit and content-addressed.
            observable_failure: None,
            evidence_kinds: BTreeSet::new(),
        })
    }

    /// Create a typed criterion.  The observation and evidence set become
    /// part of the immutable artifact body and may therefore be projected
    /// into verification needs without model-authored restatement.
    pub fn typed(
        description: impl Into<String>,
        observable_failure: impl Into<String>,
        required: bool,
        evidence_kinds: BTreeSet<CriterionEvidenceKindV1>,
    ) -> Result<Self, String> {
        let description = description.into();
        let observable_failure = observable_failure.into();
        let id = stable_record_id("criterion", &description)?;
        let criterion = Self {
            id,
            description,
            required,
            observable_failure: Some(observable_failure),
            evidence_kinds,
        };
        criterion.validate()?;
        Ok(criterion)
    }

    /// Legacy criteria omit typed verification metadata.  They may only use
    /// test/common-case evidence until an explicit typed revision is made.
    pub fn effective_observable_failure(&self) -> &str {
        self.observable_failure
            .as_deref()
            .unwrap_or(&self.description)
    }

    pub fn effective_evidence_kinds(&self) -> BTreeSet<CriterionEvidenceKindV1> {
        if self.evidence_kinds.is_empty() {
            [
                CriterionEvidenceKindV1::ExistingTest,
                CriterionEvidenceKindV1::CommonCase,
            ]
            .into_iter()
            .collect()
        } else {
            self.evidence_kinds.clone()
        }
    }

    pub fn has_explicit_evidence_kinds(&self) -> bool {
        !self.evidence_kinds.is_empty()
    }

    pub(crate) fn validate(&self) -> Result<(), String> {
        validate_artifact_id(&self.id)?;
        if !valid_text(&self.description, 4096) {
            return Err("verification criterionの説明が不正です".into());
        }
        match (&self.observable_failure, self.evidence_kinds.is_empty()) {
            (None, true) => {}
            (Some(observable_failure), false) if valid_text(observable_failure, 4096) => {}
            (Some(_), true) => {
                return Err("typed verification criterionにはevidence kindが必要です".into());
            }
            (None, false) => {
                return Err("typed verification criterionにはobservable failureが必要です".into());
            }
            (Some(_), false) => {
                return Err("verification criterionのobservable failureが不正です".into());
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRefV1 {
    pub id: String,
    pub revision: u64,
    pub content_hash: String,
}

impl ArtifactRefV1 {
    pub fn validate(&self) -> Result<(), String> {
        validate_artifact_id(&self.id)?;
        if self.revision == 0 {
            return Err("artifact revisionは1以上である必要があります".into());
        }
        validate_hash(&self.content_hash)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactHeaderV1 {
    pub id: String,
    pub revision: u64,
    pub content_hash: String,
    pub status: ArtifactStatusV1,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub created_from: Vec<ArtifactRefV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<ArtifactRefV1>,
}

impl ArtifactHeaderV1 {
    pub fn reference(&self) -> ArtifactRefV1 {
        ArtifactRefV1 {
            id: self.id.clone(),
            revision: self.revision,
            content_hash: self.content_hash.clone(),
        }
    }

    fn validate(&self) -> Result<(), String> {
        self.reference().validate()?;
        if self.created_from.len() > 32 {
            return Err("artifactの出典参照が上限を超えました".into());
        }
        let mut seen = BTreeSet::new();
        for source in &self.created_from {
            source.validate()?;
            if !seen.insert(source) {
                return Err("artifactの出典参照が重複しています".into());
            }
        }
        if let Some(previous) = &self.supersedes {
            previous.validate()?;
            if previous.id != self.id || previous.revision >= self.revision {
                return Err("artifactの置換元revisionが不正です".into());
            }
        }
        Ok(())
    }
}

/// Deterministic routine-path eligibility. An unknown or prohibited impact
/// must be escalated to the full specification path by the trusted owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoutineEligibilityV1 {
    pub affected_components: Vec<String>,
    #[serde(default)]
    pub security: bool,
    #[serde(default)]
    pub migration: bool,
    #[serde(default)]
    pub public_api: bool,
    #[serde(default)]
    pub dependency: bool,
    #[serde(default)]
    pub distribution: bool,
    #[serde(default)]
    pub major_user_contract: bool,
    #[serde(default)]
    pub unknown_impact: bool,
}

impl RoutineEligibilityV1 {
    pub fn local_component(component: impl Into<String>) -> Self {
        Self {
            affected_components: vec![component.into()],
            security: false,
            migration: false,
            public_api: false,
            dependency: false,
            distribution: false,
            major_user_contract: false,
            unknown_impact: false,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.affected_components.is_empty()
            || self.affected_components.len() > MAX_COMPONENTS
            || self
                .affected_components
                .iter()
                .any(|component| !valid_text(component, 256))
        {
            return Err("routine変更のcomponent範囲が不正です".into());
        }
        let unique = self.affected_components.iter().collect::<BTreeSet<_>>();
        if unique.len() != self.affected_components.len() {
            return Err("routine変更のcomponentが重複しています".into());
        }
        Ok(())
    }

    pub fn blocker(&self) -> Option<&'static str> {
        if self.unknown_impact {
            Some("unknown_impact")
        } else if self.security {
            Some("security")
        } else if self.migration {
            Some("migration")
        } else if self.public_api {
            Some("public_api")
        } else if self.dependency {
            Some("dependency")
        } else if self.distribution {
            Some("distribution")
        } else if self.major_user_contract {
            Some("major_user_contract")
        } else if self.affected_components.len() != 1 {
            Some("multiple_components")
        } else {
            None
        }
    }

    pub fn is_routine(&self) -> bool {
        self.blocker().is_none()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum VerificationWireV1 {
    #[default]
    Typed,
    LegacyStrings,
}

/// Lightweight contract body. Approval is a separate immutable revision
/// published by a trusted store owner; model output alone cannot approve it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ChangeContractV1 {
    pub schema_version: u32,
    pub header: ArtifactHeaderV1,
    pub eligibility: RoutineEligibilityV1,
    pub symptom_or_request: String,
    pub cause: String,
    pub expected_outcome: String,
    pub verification: Vec<VerificationCriterionV1>,
    // This is deliberately not persisted. It records whether the body was
    // decoded from the former `Vec<String>` wire so that its legacy hash is
    // accepted for that exact legacy representation only. A typed body can
    // never borrow the old content address.
    #[serde(skip)]
    verification_wire: VerificationWireV1,
}

impl ChangeContractV1 {
    pub fn draft(
        id: impl Into<String>,
        eligibility: RoutineEligibilityV1,
        symptom_or_request: impl Into<String>,
        cause: impl Into<String>,
        expected_outcome: impl Into<String>,
        verification: Vec<String>,
    ) -> Result<Self, String> {
        let verification = verification
            .into_iter()
            .map(|item| VerificationCriterionV1::stable(item, true))
            .collect::<Result<Vec<_>, _>>()?;
        let mut contract = Self {
            schema_version: WORKFLOW_ARTIFACT_SCHEMA_VERSION,
            header: ArtifactHeaderV1 {
                id: id.into(),
                revision: 1,
                content_hash: "0".repeat(64),
                status: ArtifactStatusV1::Draft,
                created_from: Vec::new(),
                supersedes: None,
            },
            eligibility,
            symptom_or_request: symptom_or_request.into(),
            cause: cause.into(),
            expected_outcome: expected_outcome.into(),
            verification,
            verification_wire: VerificationWireV1::Typed,
        };
        contract.header.content_hash = contract.canonical_hash()?;
        contract.validate()?;
        Ok(contract)
    }

    /// Create a lightweight contract from criteria fixed by the trusted host.
    ///
    /// The ordinary model proposal path uses this constructor so observable
    /// failures and admissible evidence kinds cannot be weakened by prose in
    /// a tool call. Approval remains a separate durable owner operation.
    pub fn draft_typed(
        id: impl Into<String>,
        eligibility: RoutineEligibilityV1,
        symptom_or_request: impl Into<String>,
        cause: impl Into<String>,
        expected_outcome: impl Into<String>,
        verification: Vec<VerificationCriterionV1>,
    ) -> Result<Self, String> {
        let mut contract = Self {
            schema_version: WORKFLOW_ARTIFACT_SCHEMA_VERSION,
            header: ArtifactHeaderV1 {
                id: id.into(),
                revision: 1,
                content_hash: "0".repeat(64),
                status: ArtifactStatusV1::Draft,
                created_from: Vec::new(),
                supersedes: None,
            },
            eligibility,
            symptom_or_request: symptom_or_request.into(),
            cause: cause.into(),
            expected_outcome: expected_outcome.into(),
            verification,
            verification_wire: VerificationWireV1::Typed,
        };
        contract.header.content_hash = contract.canonical_hash()?;
        contract.validate()?;
        Ok(contract)
    }

    /// Creates a new immutable revision. Stores expose approval separately
    /// from body creation so untrusted output cannot self-approve a gate.
    pub fn revised(&self, status: ArtifactStatusV1) -> Result<Self, String> {
        self.validate()?;
        let mut next = self.clone();
        next.header.revision = next
            .header
            .revision
            .checked_add(1)
            .ok_or("artifact revisionが上限に達しました")?;
        next.header.supersedes = Some(self.header.reference());
        next.header.status = status;
        next.verification_wire = VerificationWireV1::Typed;
        next.header.content_hash = "0".repeat(64);
        next.header.content_hash = next.canonical_hash()?;
        next.validate()?;
        Ok(next)
    }

    pub fn reference(&self) -> ArtifactRefV1 {
        self.header.reference()
    }

    pub fn canonical_bytes(&self) -> Result<Vec<u8>, String> {
        let mut canonical = self.clone();
        canonical.header.content_hash.clear();
        let mut bytes = b"polaris.workflow.change_contract.v1\0".to_vec();
        bytes.extend(
            serde_json::to_vec(&canonical).map_err(|_| "change contractを正規化できません")?,
        );
        Ok(bytes)
    }

    pub fn canonical_hash(&self) -> Result<String, String> {
        Ok(format!("{:x}", Sha256::digest(self.canonical_bytes()?)))
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != WORKFLOW_ARTIFACT_SCHEMA_VERSION {
            return Err("未対応のchange contract保存形式です".into());
        }
        self.header.validate()?;
        self.eligibility.validate()?;
        if !valid_text(&self.symptom_or_request, MAX_CONTRACT_FIELD_BYTES)
            || !valid_text(&self.cause, MAX_CONTRACT_FIELD_BYTES)
            || !valid_text(&self.expected_outcome, MAX_CONTRACT_FIELD_BYTES)
            || self.verification.is_empty()
            || self.verification.len() > MAX_VERIFICATION_ITEMS
            || self
                .verification
                .iter()
                .any(|criterion| criterion.validate().is_err())
        {
            return Err("change contractの必須内容または上限が不正です".into());
        }
        reject_duplicate_ids(
            self.verification
                .iter()
                .map(|criterion| criterion.id.as_str()),
            "change contractのcriterion",
        )?;
        let canonical_hash = self.canonical_hash()?;
        let uses_current_hash = self.header.content_hash == canonical_hash;
        let uses_legacy_hash = self.verification_wire == VerificationWireV1::LegacyStrings
            && self.legacy_criteria_are_exact()?
            && self.header.content_hash == self.legacy_canonical_hash()?;
        if !uses_current_hash && !uses_legacy_hash {
            return Err("change contractのcontent hashが一致しません".into());
        }
        let bytes = serde_json::to_vec(self).map_err(|_| "change contractを保存できません")?;
        if bytes.len() > MAX_WORKFLOW_ARTIFACT_BODY_BYTES {
            return Err("change contract本文が上限を超えました".into());
        }
        Ok(())
    }

    fn legacy_canonical_hash(&self) -> Result<String, String> {
        #[derive(Serialize)]
        struct LegacyCanonical<'a> {
            schema_version: u32,
            header: ArtifactHeaderV1,
            eligibility: &'a RoutineEligibilityV1,
            symptom_or_request: &'a str,
            cause: &'a str,
            expected_outcome: &'a str,
            verification: Vec<&'a str>,
        }
        let mut header = self.header.clone();
        header.content_hash.clear();
        let legacy = LegacyCanonical {
            schema_version: self.schema_version,
            header,
            eligibility: &self.eligibility,
            symptom_or_request: &self.symptom_or_request,
            cause: &self.cause,
            expected_outcome: &self.expected_outcome,
            verification: self
                .verification
                .iter()
                .map(|item| item.description.as_str())
                .collect(),
        };
        let mut bytes = b"polaris.workflow.change_contract.v1\0".to_vec();
        bytes.extend(
            serde_json::to_vec(&legacy).map_err(|_| "legacy change contractを正規化できません")?,
        );
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    fn legacy_criteria_are_exact(&self) -> Result<bool, String> {
        self.verification.iter().try_fold(true, |valid, criterion| {
            let expected = VerificationCriterionV1::stable(&criterion.description, true)?;
            Ok(valid && *criterion == expected)
        })
    }
}

impl<'de> Deserialize<'de> for ChangeContractV1 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum VerificationWire {
            Typed(Vec<VerificationCriterionV1>),
            Legacy(Vec<String>),
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Wire {
            schema_version: u32,
            header: ArtifactHeaderV1,
            eligibility: RoutineEligibilityV1,
            symptom_or_request: String,
            cause: String,
            expected_outcome: String,
            verification: VerificationWire,
        }
        let wire = Wire::deserialize(deserializer)?;
        let (verification, verification_wire) = match wire.verification {
            VerificationWire::Typed(items) => (items, VerificationWireV1::Typed),
            VerificationWire::Legacy(items) => (
                items
                    .into_iter()
                    .map(|item| {
                        VerificationCriterionV1::stable(item, true)
                            .map_err(serde::de::Error::custom)
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                VerificationWireV1::LegacyStrings,
            ),
        };
        Ok(Self {
            schema_version: wire.schema_version,
            header: wire.header,
            eligibility: wire.eligibility,
            symptom_or_request: wire.symptom_or_request,
            cause: wire.cause,
            expected_outcome: wire.expected_outcome,
            verification,
            verification_wire,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FeatureSpecV1 {
    pub schema_version: u32,
    pub header: ArtifactHeaderV1,
    pub title: String,
    pub summary: String,
    pub requirements: Vec<RequirementV1>,
    pub acceptance_criteria: Vec<VerificationCriterionV1>,
    /// Questions that must be resolved before implementation can start.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub required_clarifications: Vec<UnresolvedDecisionV1>,
    /// Deterministic specification consistency failures. Empty is the only
    /// implementable state; prose warnings cannot be silently ignored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub consistency_errors: Vec<String>,
}

impl FeatureSpecV1 {
    pub fn draft(
        id: impl Into<String>,
        title: impl Into<String>,
        summary: impl Into<String>,
        requirements: Vec<RequirementV1>,
        acceptance_criteria: Vec<VerificationCriterionV1>,
    ) -> Result<Self, String> {
        Self::draft_with_context(
            id,
            title,
            summary,
            requirements,
            acceptance_criteria,
            Vec::new(),
            Vec::new(),
            Vec::new(),
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn draft_with_context(
        id: impl Into<String>,
        title: impl Into<String>,
        summary: impl Into<String>,
        requirements: Vec<RequirementV1>,
        acceptance_criteria: Vec<VerificationCriterionV1>,
        created_from: Vec<ArtifactRefV1>,
        required_clarifications: Vec<UnresolvedDecisionV1>,
        consistency_errors: Vec<String>,
    ) -> Result<Self, String> {
        let mut artifact = Self {
            schema_version: WORKFLOW_ARTIFACT_SCHEMA_VERSION,
            header: new_header(id.into()),
            title: title.into(),
            summary: summary.into(),
            requirements,
            acceptance_criteria,
            required_clarifications,
            consistency_errors,
        };
        artifact.header.created_from = created_from;
        artifact.rehash()?;
        artifact.validate()?;
        Ok(artifact)
    }

    pub fn reference(&self) -> ArtifactRefV1 {
        self.header.reference()
    }

    pub fn revised(&self, status: ArtifactStatusV1) -> Result<Self, String> {
        self.validate()?;
        let mut next = self.clone();
        revise_header(&mut next.header, status)?;
        next.rehash()?;
        next.validate()?;
        Ok(next)
    }

    fn canonical_hash(&self) -> Result<String, String> {
        canonical_artifact_hash("polaris.workflow.feature_spec.v1", self)
    }

    fn rehash(&mut self) -> Result<(), String> {
        self.header.content_hash.clear();
        self.header.content_hash = self.canonical_hash()?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_common_artifact(self.schema_version, &self.header)?;
        if !valid_text(&self.title, 512)
            || !valid_text(&self.summary, MAX_CONTRACT_FIELD_BYTES)
            || self.requirements.is_empty()
            || self.requirements.len() > MAX_REQUIREMENTS
            || self.acceptance_criteria.is_empty()
            || self.acceptance_criteria.len() > MAX_VERIFICATION_ITEMS
            || self.required_clarifications.len() > MAX_UNRESOLVED_DECISIONS
            || self.required_clarifications.iter().any(|item| {
                validate_artifact_id(&item.id).is_err() || !valid_text(&item.question, 4096)
            })
            || self.consistency_errors.len() > MAX_UNRESOLVED_DECISIONS
            || self
                .consistency_errors
                .iter()
                .any(|item| !valid_text(item, 4096))
        {
            return Err("feature specの必須内容または上限が不正です".into());
        }
        for requirement in &self.requirements {
            requirement.validate()?;
        }
        for criterion in &self.acceptance_criteria {
            criterion.validate()?;
        }
        reject_duplicate_ids(
            self.requirements.iter().map(|item| item.id.as_str()),
            "feature specのrequirement",
        )?;
        reject_duplicate_ids(
            self.acceptance_criteria.iter().map(|item| item.id.as_str()),
            "feature specのcriterion",
        )?;
        reject_duplicate_ids(
            self.required_clarifications
                .iter()
                .map(|item| item.id.as_str()),
            "feature specのrequired clarification",
        )?;
        if self.header.content_hash != self.canonical_hash()? {
            return Err("feature specのcontent hashが一致しません".into());
        }
        validate_body_size(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ImplementationPlanV1 {
    pub schema_version: u32,
    pub header: ArtifactHeaderV1,
    pub feature_spec: ArtifactRefV1,
    pub approach: String,
    pub scope: Vec<String>,
}

impl ImplementationPlanV1 {
    pub fn draft(
        id: impl Into<String>,
        feature_spec: ArtifactRefV1,
        approach: impl Into<String>,
        scope: Vec<String>,
    ) -> Result<Self, String> {
        let mut artifact = Self {
            schema_version: WORKFLOW_ARTIFACT_SCHEMA_VERSION,
            header: new_header(id.into()),
            feature_spec,
            approach: approach.into(),
            scope,
        };
        artifact
            .header
            .created_from
            .push(artifact.feature_spec.clone());
        artifact.rehash()?;
        artifact.validate()?;
        Ok(artifact)
    }

    pub fn reference(&self) -> ArtifactRefV1 {
        self.header.reference()
    }

    pub fn revised(&self, status: ArtifactStatusV1) -> Result<Self, String> {
        self.validate()?;
        let mut next = self.clone();
        revise_header(&mut next.header, status)?;
        next.rehash()?;
        next.validate()?;
        Ok(next)
    }

    fn canonical_hash(&self) -> Result<String, String> {
        canonical_artifact_hash("polaris.workflow.implementation_plan.v1", self)
    }

    fn rehash(&mut self) -> Result<(), String> {
        self.header.content_hash.clear();
        self.header.content_hash = self.canonical_hash()?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_common_artifact(self.schema_version, &self.header)?;
        self.feature_spec.validate()?;
        if !valid_text(&self.approach, MAX_CONTRACT_FIELD_BYTES)
            || self.scope.is_empty()
            || self.scope.len() > MAX_TASK_SCOPE_ITEMS
            || self.scope.iter().any(|scope| !valid_text(scope, 2048))
        {
            return Err("implementation planの必須内容または上限が不正です".into());
        }
        if !self.header.created_from.contains(&self.feature_spec) {
            return Err("implementation planのfeature spec出典がありません".into());
        }
        if self.header.content_hash != self.canonical_hash()? {
            return Err("implementation planのcontent hashが一致しません".into());
        }
        validate_body_size(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStateV1 {
    Planned,
    Ready,
    InProgress,
    Blocked,
    Completed,
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskDefinitionV1 {
    pub id: String,
    pub title: String,
    pub scope: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requirement_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub criterion_ids: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dependencies: Vec<String>,
}

impl TaskDefinitionV1 {
    pub fn stable_id(seed: &str) -> Result<String, String> {
        stable_record_id("task", seed)
    }

    fn validate(&self) -> Result<(), String> {
        validate_artifact_id(&self.id)?;
        if !valid_text(&self.title, 512)
            || self.scope.is_empty()
            || self.scope.len() > MAX_TASK_SCOPE_ITEMS
            || self.scope.iter().any(|item| !valid_text(item, 2048))
            || self.dependencies.len() > MAX_TASK_DEPENDENCIES
        {
            return Err("task definitionの必須内容または上限が不正です".into());
        }
        for id in self
            .requirement_ids
            .iter()
            .chain(&self.criterion_ids)
            .chain(&self.dependencies)
        {
            validate_artifact_id(id)?;
        }
        reject_duplicate_ids(
            self.requirement_ids.iter().map(String::as_str),
            "taskのrequirement参照",
        )?;
        reject_duplicate_ids(
            self.criterion_ids.iter().map(String::as_str),
            "taskのcriterion参照",
        )?;
        reject_duplicate_ids(
            self.dependencies.iter().map(String::as_str),
            "taskのdependency",
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskRuntimeV1 {
    pub task_id: String,
    pub state: TaskStateV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocker: Option<String>,
}

impl TaskRuntimeV1 {
    fn validate(&self) -> Result<(), String> {
        validate_artifact_id(&self.task_id)?;
        match (self.state, self.blocker.as_deref()) {
            (TaskStateV1::Blocked, Some(blocker)) if valid_text(blocker, 4096) => Ok(()),
            (TaskStateV1::Blocked, _) => Err("blocked taskにはblockerが必要です".into()),
            (_, None) => Ok(()),
            (_, Some(_)) => Err("blocked以外のtaskにblockerは設定できません".into()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskGraphV1 {
    pub schema_version: u32,
    pub header: ArtifactHeaderV1,
    pub feature_spec: ArtifactRefV1,
    pub implementation_plan: ArtifactRefV1,
    pub definition_hash: String,
    pub runtime_state_hash: String,
    pub tasks: Vec<TaskDefinitionV1>,
    pub runtime: Vec<TaskRuntimeV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveTaskRefV1 {
    pub task_graph: ArtifactRefV1,
    pub task_id: String,
    pub definition_hash: String,
}

impl ActiveTaskRefV1 {
    pub fn validate(&self) -> Result<(), String> {
        self.task_graph.validate()?;
        validate_artifact_id(&self.task_id)?;
        validate_hash(&self.definition_hash)
    }

    pub fn validate_against(&self, graph: &TaskGraphV1) -> Result<(), String> {
        self.validate()?;
        graph.validate()?;
        if self.task_graph != graph.reference()
            || self.definition_hash != graph.definition_hash
            || graph.task(&self.task_id).is_none()
        {
            return Err("active task参照がtask graphのexact revisionと一致しません".into());
        }
        Ok(())
    }
}

impl TaskGraphV1 {
    pub fn draft(
        id: impl Into<String>,
        feature_spec: ArtifactRefV1,
        implementation_plan: ArtifactRefV1,
        tasks: Vec<TaskDefinitionV1>,
        runtime: Vec<TaskRuntimeV1>,
    ) -> Result<Self, String> {
        let mut artifact = Self {
            schema_version: WORKFLOW_ARTIFACT_SCHEMA_VERSION,
            header: new_header(id.into()),
            feature_spec,
            implementation_plan,
            definition_hash: String::new(),
            runtime_state_hash: String::new(),
            tasks,
            runtime,
        };
        artifact.header.created_from = vec![
            artifact.feature_spec.clone(),
            artifact.implementation_plan.clone(),
        ];
        artifact.rehash()?;
        artifact.validate()?;
        Ok(artifact)
    }

    pub fn reference(&self) -> ArtifactRefV1 {
        self.header.reference()
    }

    pub fn with_runtime(&self, runtime: Vec<TaskRuntimeV1>) -> Result<Self, String> {
        self.validate()?;
        let mut next = self.clone();
        revise_header(&mut next.header, self.header.status)?;
        next.runtime = runtime;
        next.rehash()?;
        next.validate()?;
        Ok(next)
    }

    pub fn revised(&self, status: ArtifactStatusV1) -> Result<Self, String> {
        self.validate()?;
        let mut next = self.clone();
        revise_header(&mut next.header, status)?;
        next.rehash()?;
        next.validate()?;
        Ok(next)
    }

    pub fn task(&self, id: &str) -> Option<(&TaskDefinitionV1, &TaskRuntimeV1)> {
        let definition = self.tasks.iter().find(|task| task.id == id)?;
        let runtime = self.runtime.iter().find(|runtime| runtime.task_id == id)?;
        Some((definition, runtime))
    }

    pub fn validate_with_feature(
        &self,
        feature: &FeatureSpecV1,
        plan: &ImplementationPlanV1,
    ) -> Result<(), String> {
        self.validate()?;
        feature.validate()?;
        plan.validate()?;
        if self.feature_spec != feature.reference()
            || self.implementation_plan != plan.reference()
            || plan.feature_spec != feature.reference()
        {
            return Err("task graphのfeature/plan参照が一致しません".into());
        }
        let requirements = feature
            .requirements
            .iter()
            .map(|item| item.id.as_str())
            .collect::<BTreeSet<_>>();
        let criteria = feature
            .acceptance_criteria
            .iter()
            .map(|item| item.id.as_str())
            .collect::<BTreeSet<_>>();
        for task in &self.tasks {
            if task
                .requirement_ids
                .iter()
                .any(|id| !requirements.contains(id.as_str()))
                || task
                    .criterion_ids
                    .iter()
                    .any(|id| !criteria.contains(id.as_str()))
            {
                return Err("task graphにdangling requirement/criterion参照があります".into());
            }
        }
        Ok(())
    }

    fn definition_hash_value(&self) -> Result<String, String> {
        hash_serialized(&(
            "polaris.workflow.task_definitions.v1",
            &self.feature_spec,
            &self.implementation_plan,
            &self.tasks,
        ))
    }

    fn runtime_hash_value(&self) -> Result<String, String> {
        hash_serialized(&("polaris.workflow.task_runtime.v1", &self.runtime))
    }

    fn canonical_hash(&self) -> Result<String, String> {
        canonical_artifact_hash("polaris.workflow.task_graph.v1", self)
    }

    fn rehash(&mut self) -> Result<(), String> {
        self.definition_hash = self.definition_hash_value()?;
        self.runtime_state_hash = self.runtime_hash_value()?;
        self.header.content_hash.clear();
        self.header.content_hash = self.canonical_hash()?;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), String> {
        validate_common_artifact(self.schema_version, &self.header)?;
        self.feature_spec.validate()?;
        self.implementation_plan.validate()?;
        validate_hash(&self.definition_hash)?;
        validate_hash(&self.runtime_state_hash)?;
        if self.tasks.is_empty()
            || self.tasks.len() > MAX_TASKS
            || self.tasks.len() != self.runtime.len()
        {
            return Err("task graphのtask数が不正です".into());
        }
        for task in &self.tasks {
            task.validate()?;
        }
        for runtime in &self.runtime {
            runtime.validate()?;
        }
        reject_duplicate_ids(self.tasks.iter().map(|task| task.id.as_str()), "task")?;
        reject_duplicate_ids(
            self.runtime.iter().map(|runtime| runtime.task_id.as_str()),
            "task runtime",
        )?;
        let tasks = self
            .tasks
            .iter()
            .map(|task| (task.id.as_str(), task))
            .collect::<BTreeMap<_, _>>();
        if self
            .runtime
            .iter()
            .any(|runtime| !tasks.contains_key(runtime.task_id.as_str()))
        {
            return Err("task runtimeにdangling task参照があります".into());
        }
        for task in &self.tasks {
            if task.dependencies.contains(&task.id)
                || task
                    .dependencies
                    .iter()
                    .any(|dependency| !tasks.contains_key(dependency.as_str()))
            {
                return Err("task dependencyが不正です".into());
            }
        }
        ensure_task_graph_acyclic(&tasks)?;
        if !self.header.created_from.contains(&self.feature_spec)
            || !self.header.created_from.contains(&self.implementation_plan)
        {
            return Err("task graphの必須出典がありません".into());
        }
        if self.definition_hash != self.definition_hash_value()?
            || self.runtime_state_hash != self.runtime_hash_value()?
        {
            return Err("task graphのdefinition/runtime hashが一致しません".into());
        }
        if self.header.content_hash != self.canonical_hash()? {
            return Err("task graphのcontent hashが一致しません".into());
        }
        validate_body_size(self)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowArtifactMetadataV1 {
    pub kind: WorkflowArtifactKindV1,
    pub header: ArtifactHeaderV1,
    /// Retained as a required field for ChangeContractV1 metadata and wire
    /// compatibility with the M27 store. Other artifact kinds use a neutral
    /// single-component value and never consult it for routine eligibility.
    pub eligibility: RoutineEligibilityV1,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub definition_hash: Option<String>,
}

impl WorkflowArtifactMetadataV1 {
    pub fn reference(&self) -> ArtifactRefV1 {
        self.header.reference()
    }

    fn from_change_contract(contract: &ChangeContractV1) -> Self {
        Self {
            kind: WorkflowArtifactKindV1::ChangeContract,
            header: contract.header.clone(),
            eligibility: contract.eligibility.clone(),
            definition_hash: None,
        }
    }

    fn from_project_principles(artifact: &ProjectPrinciplesV1) -> Self {
        Self::from_header(WorkflowArtifactKindV1::ProjectPrinciples, &artifact.header)
    }

    fn from_idea_decision(artifact: &IdeaDecisionV1) -> Self {
        Self::from_header(WorkflowArtifactKindV1::IdeaDecision, &artifact.header)
    }

    fn from_header(kind: WorkflowArtifactKindV1, header: &ArtifactHeaderV1) -> Self {
        Self {
            kind,
            header: header.clone(),
            eligibility: RoutineEligibilityV1::local_component("workflow-artifact"),
            definition_hash: None,
        }
    }

    fn from_task_graph(artifact: &TaskGraphV1) -> Self {
        Self {
            kind: WorkflowArtifactKindV1::TaskGraph,
            header: artifact.header.clone(),
            eligibility: RoutineEligibilityV1::local_component("workflow-artifact"),
            definition_hash: Some(artifact.definition_hash.clone()),
        }
    }

    fn validate(&self) -> Result<(), String> {
        self.header.validate()?;
        self.eligibility.validate()?;
        match (self.kind, self.definition_hash.as_deref()) {
            (WorkflowArtifactKindV1::TaskGraph, Some(hash)) => validate_hash(hash)?,
            (WorkflowArtifactKindV1::TaskGraph, None) => {
                return Err("task graph metadataにdefinition hashがありません".into());
            }
            (_, Some(_)) => return Err("task graph以外にdefinition hashは設定できません".into()),
            (_, None) => {}
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowArtifactGraphV1 {
    pub schema_version: u32,
    pub revision: u64,
    pub artifacts: Vec<WorkflowArtifactMetadataV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RevisionStaleImpactV1 {
    pub replacement: ArtifactRefV1,
    pub superseded: ArtifactRefV1,
    pub stale_dependents: Vec<ArtifactRefV1>,
    pub evidence_stale: bool,
    pub gate_proof_stale: bool,
}

impl Default for WorkflowArtifactGraphV1 {
    fn default() -> Self {
        Self {
            schema_version: WORKFLOW_ARTIFACT_SCHEMA_VERSION,
            revision: 0,
            artifacts: Vec::new(),
        }
    }
}

impl WorkflowArtifactGraphV1 {
    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != WORKFLOW_ARTIFACT_SCHEMA_VERSION {
            return Err("未対応のworkflow artifact graphです".into());
        }
        if self.artifacts.len() > MAX_WORKFLOW_ARTIFACTS {
            return Err("workflow artifact数が上限を超えました".into());
        }
        let mut keys = BTreeSet::new();
        for artifact in &self.artifacts {
            artifact.validate()?;
            if !keys.insert((artifact.header.id.as_str(), artifact.header.revision)) {
                return Err("workflow artifactのIDとrevisionが重複しています".into());
            }
            if let Some(previous) = &artifact.header.supersedes
                && !self.contains(previous)
            {
                return Err("workflow artifactの置換元がgraphにありません".into());
            }
            for source in &artifact.header.created_from {
                if source == &artifact.reference() || !self.contains(source) {
                    return Err("workflow artifactの出典がdanglingまたは自己参照です".into());
                }
            }
        }
        self.ensure_acyclic()?;
        if self.revision < self.artifacts.len() as u64 {
            return Err("workflow artifact graph revisionが不正です".into());
        }
        Ok(())
    }

    pub fn basis_hash(&self) -> Result<String, String> {
        self.validate()?;
        let bytes = serde_json::to_vec(self).map_err(|_| "artifact graphを正規化できません")?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    pub fn contains(&self, reference: &ArtifactRefV1) -> bool {
        self.artifacts
            .iter()
            .any(|artifact| artifact.reference() == *reference)
    }

    pub fn metadata(&self, reference: &ArtifactRefV1) -> Option<&WorkflowArtifactMetadataV1> {
        self.artifacts
            .iter()
            .find(|artifact| artifact.reference() == *reference)
    }

    pub fn latest(&self, id: &str) -> Option<&WorkflowArtifactMetadataV1> {
        self.artifacts
            .iter()
            .filter(|artifact| artifact.header.id == id)
            .max_by_key(|artifact| artifact.header.revision)
    }

    pub fn validate_active_feature(&self, reference: &ArtifactRefV1) -> Result<(), String> {
        reference.validate()?;
        if self.metadata(reference).is_none_or(|item| {
            item.kind != WorkflowArtifactKindV1::FeatureSpec
                || self.latest(&reference.id).map(|item| item.reference())
                    != Some(reference.clone())
        }) {
            return Err("active featureがexact feature spec revisionを参照していません".into());
        }
        Ok(())
    }

    pub fn validate_active_task(&self, reference: &ActiveTaskRefV1) -> Result<(), String> {
        reference.validate()?;
        let metadata = self
            .metadata(&reference.task_graph)
            .ok_or("active taskのtask graphがartifact graphにありません")?;
        if metadata.kind != WorkflowArtifactKindV1::TaskGraph
            || metadata.definition_hash.as_deref() != Some(reference.definition_hash.as_str())
            || self
                .latest(&reference.task_graph.id)
                .map(|item| item.reference())
                != Some(reference.task_graph.clone())
        {
            return Err(
                "active taskのdefinition hashがexact task graph revisionと一致しません".into(),
            );
        }
        Ok(())
    }

    pub fn insert_change_contract(&mut self, contract: &ChangeContractV1) -> Result<bool, String> {
        contract.validate()?;
        self.insert_metadata(WorkflowArtifactMetadataV1::from_change_contract(contract))
    }

    pub fn insert_feature_spec(&mut self, artifact: &FeatureSpecV1) -> Result<bool, String> {
        artifact.validate()?;
        self.insert_metadata(WorkflowArtifactMetadataV1::from_header(
            WorkflowArtifactKindV1::FeatureSpec,
            &artifact.header,
        ))
    }

    pub fn insert_project_principles(
        &mut self,
        artifact: &ProjectPrinciplesV1,
    ) -> Result<bool, String> {
        artifact.validate()?;
        self.insert_metadata(WorkflowArtifactMetadataV1::from_project_principles(
            artifact,
        ))
    }

    pub fn insert_idea_decision(&mut self, artifact: &IdeaDecisionV1) -> Result<bool, String> {
        artifact.validate()?;
        self.insert_metadata(WorkflowArtifactMetadataV1::from_idea_decision(artifact))
    }

    /// Returns the deterministic downstream closure which must be published as
    /// stale revisions after a principles update.  It does not mutate existing
    /// metadata: graph revisions and artifact bodies are immutable, and the
    /// writer owns the CAS publication of every stale replacement.
    pub fn stale_dependents_of(
        &self,
        upstream: &ArtifactRefV1,
    ) -> Result<Vec<ArtifactRefV1>, String> {
        self.validate()?;
        let upstream_metadata = self
            .metadata(upstream)
            .ok_or("stale元artifactがgraphにありません")?;
        let _ = upstream_metadata;
        let mut stale = BTreeSet::new();
        let mut frontier = BTreeSet::from([upstream.clone()]);
        while !frontier.is_empty() {
            let next = self
                .artifacts
                .iter()
                .filter_map(|artifact| {
                    let reference = artifact.reference();
                    (!stale.contains(&reference)
                        && artifact
                            .header
                            .created_from
                            .iter()
                            .any(|source| frontier.contains(source)))
                    .then_some(reference)
                })
                .collect::<BTreeSet<_>>();
            stale.extend(next.iter().cloned());
            frontier = next;
        }
        Ok(stale.into_iter().collect())
    }

    /// Starts propagation from the replaced immutable revision. Dependents
    /// refer to that old exact body, never directly to its replacement.
    pub fn stale_dependents_after_principles_revision(
        &self,
        replacement: &ProjectPrinciplesV1,
    ) -> Result<Vec<ArtifactRefV1>, String> {
        replacement.validate()?;
        let previous = replacement
            .header
            .supersedes
            .as_ref()
            .ok_or("初版project principlesにはstale元がありません")?;
        self.stale_dependents_of(previous)
    }

    /// Computes the complete immutable invalidation set for any inserted
    /// replacement revision. Evidence and phase-gate proofs bind the graph
    /// basis, so a successful revision always invalidates both.
    pub fn stale_impact_after_revision(
        &self,
        replacement: &ArtifactRefV1,
    ) -> Result<RevisionStaleImpactV1, String> {
        self.validate()?;
        let metadata = self
            .metadata(replacement)
            .ok_or("replacement artifactがgraphにありません")?;
        if self.latest(&replacement.id).map(|item| item.reference()) != Some(replacement.clone()) {
            return Err("replacement artifactは現行revisionではありません".into());
        }
        let superseded = metadata
            .header
            .supersedes
            .clone()
            .ok_or("初版artifactにはstale伝播がありません")?;
        Ok(RevisionStaleImpactV1 {
            replacement: replacement.clone(),
            superseded: superseded.clone(),
            stale_dependents: self.stale_dependents_of(&superseded)?,
            evidence_stale: true,
            gate_proof_stale: true,
        })
    }

    pub fn insert_implementation_plan(
        &mut self,
        artifact: &ImplementationPlanV1,
    ) -> Result<bool, String> {
        artifact.validate()?;
        let source = self
            .metadata(&artifact.feature_spec)
            .ok_or("implementation planのfeature specがgraphにありません")?;
        if source.kind != WorkflowArtifactKindV1::FeatureSpec {
            return Err("implementation planの参照先はfeature specではありません".into());
        }
        self.insert_metadata(WorkflowArtifactMetadataV1::from_header(
            WorkflowArtifactKindV1::ImplementationPlan,
            &artifact.header,
        ))
    }

    pub fn insert_task_graph(&mut self, artifact: &TaskGraphV1) -> Result<bool, String> {
        artifact.validate()?;
        if self
            .metadata(&artifact.feature_spec)
            .is_none_or(|item| item.kind != WorkflowArtifactKindV1::FeatureSpec)
            || self
                .metadata(&artifact.implementation_plan)
                .is_none_or(|item| item.kind != WorkflowArtifactKindV1::ImplementationPlan)
        {
            return Err("task graphのfeature specまたはplanがgraphにありません".into());
        }
        self.insert_metadata(WorkflowArtifactMetadataV1::from_task_graph(artifact))
    }

    fn insert_metadata(&mut self, metadata: WorkflowArtifactMetadataV1) -> Result<bool, String> {
        metadata.validate()?;
        let reference = metadata.reference();
        if let Some(existing) = self.metadata(&reference) {
            return if existing == &metadata {
                Ok(false)
            } else {
                Err("同じartifact参照のmetadataが異なります".into())
            };
        }
        match self.latest(&reference.id) {
            None if reference.revision != 1 || metadata.header.supersedes.is_some() => {
                return Err("最初のartifact revisionが不正です".into());
            }
            Some(previous)
                if reference.revision != previous.header.revision.saturating_add(1)
                    || metadata.header.supersedes.as_ref() != Some(&previous.reference()) =>
            {
                return Err("artifact revisionまたは置換元が連続していません".into());
            }
            _ => {}
        }
        for source in &metadata.header.created_from {
            if !self.contains(source) {
                return Err("workflow artifactの出典がgraphにありません".into());
            }
        }
        self.artifacts.push(metadata);
        self.revision = self
            .revision
            .checked_add(1)
            .ok_or("artifact graph revisionが上限に達しました")?;
        self.validate()?;
        Ok(true)
    }

    fn ensure_acyclic(&self) -> Result<(), String> {
        let entries = self
            .artifacts
            .iter()
            .map(|item| (item.reference(), item))
            .collect::<BTreeMap<_, _>>();
        fn visit<'a>(
            reference: &'a ArtifactRefV1,
            entries: &BTreeMap<ArtifactRefV1, &'a WorkflowArtifactMetadataV1>,
            visiting: &mut BTreeSet<ArtifactRefV1>,
            visited: &mut BTreeSet<ArtifactRefV1>,
        ) -> Result<(), String> {
            if visited.contains(reference) {
                return Ok(());
            }
            if !visiting.insert(reference.clone()) {
                return Err("workflow artifact graphにcycleがあります".into());
            }
            let item = entries
                .get(reference)
                .ok_or("workflow artifact graphにdangling参照があります")?;
            for source in item
                .header
                .created_from
                .iter()
                .chain(item.header.supersedes.iter())
            {
                visit(source, entries, visiting, visited)?;
            }
            visiting.remove(reference);
            visited.insert(reference.clone());
            Ok(())
        }
        let mut visiting = BTreeSet::new();
        let mut visited = BTreeSet::new();
        for reference in entries.keys() {
            visit(reference, &entries, &mut visiting, &mut visited)?;
        }
        Ok(())
    }

    pub fn approved_routine_contract(
        &self,
        reference: &ArtifactRefV1,
    ) -> Result<&WorkflowArtifactMetadataV1, String> {
        reference.validate()?;
        let artifact = self
            .metadata(reference)
            .ok_or("active change contractがartifact graphにありません")?;
        if self.latest(&reference.id).map(|item| item.reference()) != Some(reference.clone()) {
            return Err("active change contractは現行revisionではありません".into());
        }
        if artifact.kind != WorkflowArtifactKindV1::ChangeContract
            || artifact.header.status != ArtifactStatusV1::Approved
        {
            return Err("active change contractは承認済みではありません".into());
        }
        if let Some(reason) = artifact.eligibility.blocker() {
            return Err(format!("change contractは完全仕様経路が必要です: {reason}"));
        }
        Ok(artifact)
    }
}

fn new_header(id: String) -> ArtifactHeaderV1 {
    ArtifactHeaderV1 {
        id,
        revision: 1,
        content_hash: String::new(),
        status: ArtifactStatusV1::Draft,
        created_from: Vec::new(),
        supersedes: None,
    }
}

fn revise_header(header: &mut ArtifactHeaderV1, status: ArtifactStatusV1) -> Result<(), String> {
    let previous = header.reference();
    header.revision = header
        .revision
        .checked_add(1)
        .ok_or("artifact revisionが上限に達しました")?;
    header.supersedes = Some(previous);
    header.status = status;
    header.content_hash.clear();
    Ok(())
}

fn validate_common_artifact(version: u32, header: &ArtifactHeaderV1) -> Result<(), String> {
    if version != WORKFLOW_ARTIFACT_SCHEMA_VERSION {
        return Err("未対応のworkflow artifact保存形式です".into());
    }
    header.validate()
}

fn canonical_artifact_hash<T: Serialize>(domain: &str, value: &T) -> Result<String, String> {
    let mut value = serde_json::to_value(value).map_err(|_| "artifactを正規化できません")?;
    value
        .get_mut("header")
        .and_then(serde_json::Value::as_object_mut)
        .ok_or("artifact headerを正規化できません")?
        .insert(
            "content_hash".into(),
            serde_json::Value::String(String::new()),
        );
    hash_serialized(&(domain, value))
}

fn hash_serialized<T: Serialize>(value: &T) -> Result<String, String> {
    let bytes = serde_json::to_vec(value).map_err(|_| "artifactを正規化できません")?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn validate_body_size<T: Serialize>(value: &T) -> Result<(), String> {
    let bytes = serde_json::to_vec(value).map_err(|_| "workflow artifactを保存できません")?;
    if bytes.len() > MAX_WORKFLOW_ARTIFACT_BODY_BYTES {
        Err("workflow artifact本文が上限を超えました".into())
    } else {
        Ok(())
    }
}

fn stable_record_id(prefix: &str, seed: &str) -> Result<String, String> {
    if !valid_text(seed, MAX_CONTRACT_FIELD_BYTES) {
        return Err("stable IDのseedが不正です".into());
    }
    let normalized = seed.split_whitespace().collect::<Vec<_>>().join(" ");
    let digest = format!("{:x}", Sha256::digest(normalized.as_bytes()));
    Ok(format!("{prefix}-{}", &digest[..20]))
}

fn reject_duplicate_ids<'a>(ids: impl Iterator<Item = &'a str>, label: &str) -> Result<(), String> {
    let mut seen = BTreeSet::new();
    for id in ids {
        if !seen.insert(id) {
            return Err(format!("{label} IDが重複しています"));
        }
    }
    Ok(())
}

fn ensure_task_graph_acyclic(tasks: &BTreeMap<&str, &TaskDefinitionV1>) -> Result<(), String> {
    fn visit<'a>(
        id: &'a str,
        tasks: &BTreeMap<&'a str, &'a TaskDefinitionV1>,
        visiting: &mut BTreeSet<&'a str>,
        visited: &mut BTreeSet<&'a str>,
    ) -> Result<(), String> {
        if visited.contains(id) {
            return Ok(());
        }
        if !visiting.insert(id) {
            return Err("task graphにdependency cycleがあります".into());
        }
        for dependency in &tasks.get(id).expect("validated task").dependencies {
            visit(dependency, tasks, visiting, visited)?;
        }
        visiting.remove(id);
        visited.insert(id);
        Ok(())
    }
    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    for id in tasks.keys().copied() {
        visit(id, tasks, &mut visiting, &mut visited)?;
    }
    Ok(())
}

pub fn validate_hash(hash: &str) -> Result<(), String> {
    if hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        Ok(())
    } else {
        Err("artifact hashが不正です".into())
    }
}

fn validate_artifact_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > MAX_ARTIFACT_ID_BYTES
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err("artifact IDが不正です".into());
    }
    Ok(())
}

fn valid_text(value: &str, max: usize) -> bool {
    !value.trim().is_empty() && value.len() <= max && !value.contains('\0')
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract() -> ChangeContractV1 {
        ChangeContractV1::draft(
            "change-1",
            RoutineEligibilityV1::local_component("polaris-core"),
            "phase gate is display-only",
            "transitions do not evaluate artifacts",
            "implement requires a current approved contract",
            vec!["gate matrix passes".into()],
        )
        .unwrap()
    }

    fn principles() -> ProjectPrinciplesV1 {
        ProjectPrinciplesV1::draft(
            "project-principles",
            vec![ProjectPrincipleV1 {
                id: "privacy".into(),
                statement: "Do not expose private artifact bodies in workflow metadata.".into(),
                feature_ids: vec!["feature-principles".into()],
                task_ids: vec!["task-principles".into()],
            }],
        )
        .unwrap()
    }

    #[test]
    fn principles_are_hashed_bounded_and_selectable() {
        let artifact = principles();
        assert!(artifact.validate().is_ok());
        assert_eq!(
            artifact
                .applicable_to(Some("feature-principles"), Some("task-principles"))
                .len(),
            1
        );
        assert!(
            artifact
                .applicable_to(Some("another-feature"), Some("task-principles"))
                .is_empty()
        );

        let mut tampered = artifact.clone();
        tampered.principles[0].statement.push_str(" changed");
        assert!(tampered.validate().is_err());
        let over_limit = ProjectPrinciplesV1::draft(
            "too-many-principles",
            (0..MAX_PROJECT_PRINCIPLES + 1)
                .map(|index| ProjectPrincipleV1 {
                    id: format!("principle-{index}"),
                    statement: "bounded invariant".into(),
                    feature_ids: Vec::new(),
                    task_ids: Vec::new(),
                })
                .collect(),
        );
        assert!(over_limit.is_err());
    }

    #[test]
    fn decision_validates_unresolved_state_and_hash() {
        let decision = IdeaDecisionV1::draft(
            "idea-decision",
            "Whether to persist a decision artifact.",
            vec!["Restart recovery needs a durable record.".into()],
            vec!["It adds workflow state.".into()],
            vec![DecisionAlternativeV1 {
                id: "free-text".into(),
                description: "Keep only prose in chat history.".into(),
            }],
            vec!["Durability adds schema surface.".into()],
            IdeaDecisionOutcomeV1::Clarify,
            vec![UnresolvedDecisionV1 {
                id: "retention".into(),
                question: "How long should old revisions remain?".into(),
            }],
        )
        .unwrap();
        assert!(decision.is_unresolved());
        let mut tampered = decision.clone();
        tampered.problem.push_str(" changed");
        assert!(tampered.validate().is_err());
        assert!(
            IdeaDecisionV1::draft(
                "bad-clarify",
                "A question",
                Vec::new(),
                Vec::new(),
                Vec::new(),
                Vec::new(),
                IdeaDecisionOutcomeV1::Clarify,
                Vec::new(),
            )
            .is_err()
        );
    }

    #[test]
    fn principles_supersede_hook_returns_transitive_dependents() {
        let principles = principles();
        let mut feature = FeatureSpecV1::draft(
            "feature-principles",
            "Principles consumer",
            "References project principles.",
            vec![RequirementV1::stable("Feature observes the principle").unwrap()],
            vec![
                VerificationCriterionV1::stable("Stale propagation is deterministic", true)
                    .unwrap(),
            ],
        )
        .unwrap();
        feature.header.created_from.push(principles.reference());
        feature.rehash().unwrap();
        feature.validate().unwrap();
        let plan = ImplementationPlanV1::draft(
            "plan-principles",
            feature.reference(),
            "Use the current feature basis.",
            vec!["workflow".into()],
        )
        .unwrap();
        let mut graph = WorkflowArtifactGraphV1::default();
        graph.insert_project_principles(&principles).unwrap();
        graph.insert_feature_spec(&feature).unwrap();
        graph.insert_implementation_plan(&plan).unwrap();
        assert_eq!(
            graph.stale_dependents_of(&principles.reference()).unwrap(),
            vec![feature.reference(), plan.reference()]
        );
        let replacement = principles.revised(ArtifactStatusV1::Ready).unwrap();
        graph.insert_project_principles(&replacement).unwrap();
        assert_eq!(
            graph
                .stale_dependents_after_principles_revision(&replacement)
                .unwrap(),
            vec![feature.reference(), plan.reference()]
        );
        let impact = graph
            .stale_impact_after_revision(&replacement.reference())
            .unwrap();
        assert_eq!(impact.superseded, principles.reference());
        assert_eq!(
            impact.stale_dependents,
            vec![feature.reference(), plan.reference()]
        );
        assert!(impact.evidence_stale && impact.gate_proof_stale);
    }

    #[test]
    fn immutable_status_changes_create_a_hashed_revision_chain() {
        let draft = contract();
        let approved = draft.revised(ArtifactStatusV1::Approved).unwrap();
        assert_eq!(approved.header.revision, 2);
        assert_eq!(approved.header.supersedes, Some(draft.reference()));
        assert_ne!(approved.header.content_hash, draft.header.content_hash);
        assert!(approved.validate().is_ok());
    }

    #[test]
    fn superseded_approved_contract_is_never_current() {
        let draft = contract();
        let approved = draft.revised(ArtifactStatusV1::Approved).unwrap();
        let replacement = approved.revised(ArtifactStatusV1::Approved).unwrap();
        let mut graph = WorkflowArtifactGraphV1::default();
        graph.insert_change_contract(&draft).unwrap();
        graph.insert_change_contract(&approved).unwrap();
        graph.insert_change_contract(&replacement).unwrap();
        assert!(
            graph
                .approved_routine_contract(&approved.reference())
                .is_err()
        );
        assert!(
            graph
                .approved_routine_contract(&replacement.reference())
                .is_ok()
        );
        let impact = graph
            .stale_impact_after_revision(&replacement.reference())
            .unwrap();
        assert_eq!(impact.superseded, approved.reference());
        assert!(impact.evidence_stale && impact.gate_proof_stale);
    }

    #[test]
    fn graph_is_idempotent_but_rejects_revision_gaps() {
        let draft = contract();
        let approved = draft.revised(ArtifactStatusV1::Approved).unwrap();
        let mut graph = WorkflowArtifactGraphV1::default();
        assert!(graph.insert_change_contract(&draft).unwrap());
        assert!(!graph.insert_change_contract(&draft).unwrap());
        assert!(graph.insert_change_contract(&approved).unwrap());
        assert!(
            graph
                .approved_routine_contract(&approved.reference())
                .is_ok()
        );

        let mut skipped = approved.revised(ArtifactStatusV1::Stale).unwrap();
        skipped.header.revision += 1;
        skipped.header.content_hash = "0".repeat(64);
        skipped.header.content_hash = skipped.canonical_hash().unwrap();
        assert!(graph.insert_change_contract(&skipped).is_err());
    }

    #[test]
    fn prohibited_or_unknown_impacts_cannot_use_the_routine_gate() {
        let mut draft = contract();
        draft.eligibility.security = true;
        draft.header.content_hash = "0".repeat(64);
        draft.header.content_hash = draft.canonical_hash().unwrap();
        let approved = draft.revised(ArtifactStatusV1::Approved).unwrap();
        let mut graph = WorkflowArtifactGraphV1::default();
        graph.insert_change_contract(&draft).unwrap();
        graph.insert_change_contract(&approved).unwrap();
        assert!(
            graph
                .approved_routine_contract(&approved.reference())
                .unwrap_err()
                .contains("security")
        );
    }

    #[test]
    fn body_tampering_and_unsafe_ids_are_rejected() {
        let mut changed = contract();
        changed.expected_outcome.push_str(" altered");
        assert!(changed.validate().is_err());
        assert!(
            ChangeContractV1::draft(
                "../escape",
                RoutineEligibilityV1::local_component("core"),
                "request",
                "cause",
                "outcome",
                vec!["verify".into()],
            )
            .is_err()
        );
    }

    #[test]
    fn verification_ids_are_stable_across_reordering() {
        let first = ChangeContractV1::draft(
            "stable-criteria",
            RoutineEligibilityV1::local_component("core"),
            "request",
            "cause",
            "outcome",
            vec!["alpha check".into(), "beta check".into()],
        )
        .unwrap();
        let second = ChangeContractV1::draft(
            "stable-criteria-2",
            RoutineEligibilityV1::local_component("core"),
            "request",
            "cause",
            "outcome",
            vec!["beta check".into(), "alpha check".into()],
        )
        .unwrap();
        let first_ids = first
            .verification
            .iter()
            .map(|item| item.id.clone())
            .collect::<BTreeSet<_>>();
        let second_ids = second
            .verification
            .iter()
            .map(|item| item.id.clone())
            .collect::<BTreeSet<_>>();
        assert_eq!(first_ids, second_ids);
        assert!(first.verification.iter().all(|item| item.required));
    }

    #[test]
    fn typed_criteria_require_explicit_observation_and_evidence_kinds() {
        let typed = VerificationCriterionV1::typed(
            "criterion",
            "the exact observable failure",
            true,
            [CriterionEvidenceKindV1::Static].into_iter().collect(),
        )
        .unwrap();
        assert_eq!(
            typed.effective_observable_failure(),
            "the exact observable failure"
        );
        assert!(typed.has_explicit_evidence_kinds());
        let mut invalid = typed.clone();
        invalid.evidence_kinds.clear();
        assert!(invalid.validate().is_err());
        let legacy = VerificationCriterionV1::stable("legacy", true).unwrap();
        assert!(!legacy.has_explicit_evidence_kinds());
        assert!(
            !legacy
                .effective_evidence_kinds()
                .contains(&CriterionEvidenceKindV1::Static)
        );
    }

    #[test]
    fn legacy_string_verification_bodies_remain_readable() {
        let contract = contract();
        let legacy_hash = contract.legacy_canonical_hash().unwrap();
        let mut value = serde_json::to_value(&contract).unwrap();
        value["header"]["content_hash"] = serde_json::Value::String(legacy_hash);
        value["verification"] = serde_json::Value::Array(
            contract
                .verification
                .iter()
                .map(|item| serde_json::Value::String(item.description.clone()))
                .collect(),
        );
        let restored: ChangeContractV1 = serde_json::from_value(value).unwrap();
        assert!(restored.validate().is_ok());
        assert_eq!(restored.verification[0].description, "gate matrix passes");
    }

    #[test]
    fn typed_contract_cannot_claim_a_legacy_content_address() {
        let contract = contract();
        let legacy_hash = contract.legacy_canonical_hash().unwrap();
        let mut typed = serde_json::to_value(&contract).unwrap();
        typed["header"]["content_hash"] = serde_json::Value::String(legacy_hash);
        let restored: ChangeContractV1 = serde_json::from_value(typed).unwrap();
        assert!(restored.validate().is_err());
    }

    #[test]
    fn legacy_contract_cannot_change_criterion_identity_or_requiredness() {
        let contract = contract();
        let legacy_hash = contract.legacy_canonical_hash().unwrap();
        let mut value = serde_json::to_value(&contract).unwrap();
        value["header"]["content_hash"] = serde_json::Value::String(legacy_hash);
        value["verification"] = serde_json::Value::Array(
            contract
                .verification
                .iter()
                .map(|item| serde_json::Value::String(item.description.clone()))
                .collect(),
        );
        let mut restored: ChangeContractV1 = serde_json::from_value(value).unwrap();
        restored.verification[0].required = false;
        assert!(restored.validate().is_err());
        restored.verification[0].required = true;
        restored.verification[0].id = "criterion-forged".into();
        assert!(restored.validate().is_err());
    }

    fn feature_and_plan() -> (FeatureSpecV1, ImplementationPlanV1) {
        let feature = FeatureSpecV1::draft(
            "feature-1",
            "Typed workflow",
            "Make task state explicit",
            vec![RequirementV1::stable("Active task is exact").unwrap()],
            vec![VerificationCriterionV1::stable("Task ref validates", true).unwrap()],
        )
        .unwrap();
        let plan = ImplementationPlanV1::draft(
            "plan-1",
            feature.reference(),
            "Build the immutable graph",
            vec!["workflow core".into()],
        )
        .unwrap();
        (feature, plan)
    }

    fn task_graph(feature: &FeatureSpecV1, plan: &ImplementationPlanV1) -> TaskGraphV1 {
        let task_id = TaskDefinitionV1::stable_id("task graph core").unwrap();
        TaskGraphV1::draft(
            "tasks-1",
            feature.reference(),
            plan.reference(),
            vec![TaskDefinitionV1 {
                id: task_id.clone(),
                title: "Build graph".into(),
                scope: vec!["workflow/artifacts.rs".into()],
                requirement_ids: vec![feature.requirements[0].id.clone()],
                criterion_ids: vec![feature.acceptance_criteria[0].id.clone()],
                dependencies: Vec::new(),
            }],
            vec![TaskRuntimeV1 {
                task_id,
                state: TaskStateV1::Ready,
                blocker: None,
            }],
        )
        .unwrap()
    }

    #[test]
    fn task_definition_and_runtime_hashes_are_independent() {
        let (feature, plan) = feature_and_plan();
        let graph = task_graph(&feature, &plan);
        let next = graph
            .with_runtime(vec![TaskRuntimeV1 {
                task_id: graph.tasks[0].id.clone(),
                state: TaskStateV1::Blocked,
                blocker: Some("waiting for evidence".into()),
            }])
            .unwrap();
        assert_eq!(graph.definition_hash, next.definition_hash);
        assert_ne!(graph.runtime_state_hash, next.runtime_state_hash);
        assert_ne!(graph.header.content_hash, next.header.content_hash);
    }

    #[test]
    fn task_graph_rejects_dangling_dependencies_and_cycles() {
        let (feature, plan) = feature_and_plan();
        let mut dangling = task_graph(&feature, &plan);
        dangling.tasks[0].dependencies.push("missing-task".into());
        dangling.rehash().unwrap();
        assert!(dangling.validate().is_err());

        let first = TaskDefinitionV1::stable_id("first").unwrap();
        let second = TaskDefinitionV1::stable_id("second").unwrap();
        let cycle = TaskGraphV1::draft(
            "tasks-cycle",
            feature.reference(),
            plan.reference(),
            vec![
                TaskDefinitionV1 {
                    id: first.clone(),
                    title: "First".into(),
                    scope: vec!["first".into()],
                    requirement_ids: Vec::new(),
                    criterion_ids: Vec::new(),
                    dependencies: vec![second.clone()],
                },
                TaskDefinitionV1 {
                    id: second.clone(),
                    title: "Second".into(),
                    scope: vec!["second".into()],
                    requirement_ids: Vec::new(),
                    criterion_ids: Vec::new(),
                    dependencies: vec![first.clone()],
                },
            ],
            vec![
                TaskRuntimeV1 {
                    task_id: first,
                    state: TaskStateV1::Planned,
                    blocker: None,
                },
                TaskRuntimeV1 {
                    task_id: second,
                    state: TaskStateV1::Planned,
                    blocker: None,
                },
            ],
        );
        assert!(cycle.unwrap_err().contains("cycle"));
    }
}
