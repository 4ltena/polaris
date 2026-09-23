//! Pure, durable verification inputs and decisions.
//!
//! This module deliberately does not execute analyzers or tests.  Callers
//! register adapters elsewhere, normalize their bounded output here, and keep
//! the resulting records beside the workflow artifacts that requested them.
//! Keeping the boundary data-only prevents an analyzer response from becoming
//! authority to widen a sandbox or to claim a clean result by itself.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

pub mod adapters;
pub mod checkpoint;
pub mod owner;
pub mod pipeline;
pub mod store;

pub use checkpoint::*;

pub const SCHEMA_VERSION: u32 = 1;
pub const MAX_FINDINGS: usize = 256;
pub const MAX_CANDIDATES_PER_NEED: usize = 8;
pub const SCORE_CUTOFF: u8 = 2;
pub const LOW_CONFIDENCE: f64 = 0.70;
const MAX_STATIC_RULES: usize = 256;

/// Stable references are strings so this layer does not couple its durable
/// representation to a particular workflow artifact implementation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationNeedV1 {
    pub schema_version: u32,
    pub requirement_id: String,
    pub task_id: String,
    pub criterion_id: String,
    /// A concise, observable failure.  It is not source or diff content.
    pub observable_failure: String,
    #[serde(default)]
    pub symbols: Vec<String>,
    pub evidence_kinds: BTreeSet<EvidenceKindV1>,
}

impl VerificationNeedV1 {
    pub fn validate(&self) -> Result<(), VerificationError> {
        if self.schema_version != SCHEMA_VERSION
            || self.requirement_id.is_empty()
            || self.task_id.is_empty()
            || self.criterion_id.is_empty()
            || self.observable_failure.is_empty()
            || self.evidence_kinds.is_empty()
        {
            return Err(VerificationError::InvalidNeed);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKindV1 {
    Static,
    ExistingTest,
    CommonCase,
    NewRegression,
    ManualReview,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnalysisScopeV1 {
    File,
    Package,
    Workspace,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScopeCompletenessV1 {
    Complete,
    Partial,
    Unknown,
}

impl ScopeCompletenessV1 {
    pub fn is_complete(self) -> bool {
        matches!(self, Self::Complete)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StaticEvidenceStateV1 {
    Satisfied,
    Violated,
    Unknown,
    Unavailable,
    NotApplicable,
}

/// The registered authority an adapter had before it was run.  Runtime tool
/// output never extends this capability.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticCapabilityV1 {
    pub schema_version: u32,
    pub analyzer_id: String,
    pub executable_hash: String,
    pub version: String,
    pub output_schema_hash: String,
    pub rules: Vec<StaticRuleCapabilityV1>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticRuleCapabilityV1 {
    pub rule_id: String,
    pub obligation_kinds: BTreeSet<String>,
    pub maximum_scope: AnalysisScopeV1,
    /// Whether a clean, complete result is a proof for this rule.
    pub clean_proves_satisfied: bool,
    pub requires_project_graph: bool,
}

/// A complete, content-addressed project graph used by a static adapter.
///
/// Rules which reason about cross-file relationships must carry this proof;
/// an adapter cannot turn a partial file walk into a project-wide conclusion.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectGraphBasisV1 {
    pub graph_hash: String,
    pub complete: bool,
}

impl ProjectGraphBasisV1 {
    fn valid_complete(&self) -> bool {
        self.complete && !self.graph_hash.is_empty()
    }
}

impl StaticCapabilityV1 {
    pub fn validate(&self) -> Result<(), VerificationError> {
        if self.schema_version != SCHEMA_VERSION
            || self.analyzer_id.is_empty()
            || self.version.is_empty()
            || !valid_hash(&self.executable_hash)
            || !valid_hash(&self.output_schema_hash)
            || self.rules.is_empty()
            || self.rules.len() > MAX_STATIC_RULES
        {
            return Err(VerificationError::InvalidStaticCapability);
        }
        let mut ids = BTreeSet::new();
        for rule in &self.rules {
            if rule.rule_id.is_empty()
                || rule.obligation_kinds.is_empty()
                || rule.obligation_kinds.iter().any(|kind| kind.is_empty())
                || !ids.insert(&rule.rule_id)
            {
                return Err(VerificationError::InvalidStaticCapability);
            }
        }
        Ok(())
    }

    pub fn rule(&self, rule_id: &str) -> Option<&StaticRuleCapabilityV1> {
        self.validate()
            .is_ok()
            .then(|| self.rules.iter().find(|rule| rule.rule_id == rule_id))
            .flatten()
    }
}

/// Host-registered static analyzer catalog.  Adapter output names an analyzer
/// but cannot provide its own capability contract.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StaticCapabilityCatalogV1 {
    capabilities: BTreeMap<String, StaticCapabilityV1>,
}

impl StaticCapabilityCatalogV1 {
    pub fn register(&mut self, capability: StaticCapabilityV1) -> Result<(), VerificationError> {
        capability.validate()?;
        if self.capabilities.contains_key(&capability.analyzer_id) {
            return Err(VerificationError::InvalidStaticCapability);
        }
        self.capabilities
            .insert(capability.analyzer_id.clone(), capability);
        Ok(())
    }

    pub fn resolve(&self, analyzer_id: &str) -> Option<&StaticCapabilityV1> {
        self.capabilities.get(analyzer_id)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FindingV1 {
    pub fingerprint: String,
    pub rule_id: String,
    /// Relative project path only; adapters reject paths outside the project.
    pub path: Option<String>,
    pub line: Option<u32>,
    pub symbol: Option<String>,
    pub introduced_by_current_diff: bool,
    pub baseline: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StaticEvidenceV1 {
    pub schema_version: u32,
    pub evidence_id: String,
    pub obligation_id: String,
    pub obligation_kind: String,
    pub analyzer_id: String,
    pub rule_id: String,
    pub tool_hash: String,
    pub config_hash: String,
    pub source_hash: String,
    pub diff_hash: String,
    pub scope: AnalysisScopeV1,
    pub scope_completeness: ScopeCompletenessV1,
    /// Present when the rule depended on a project graph. It is deliberately
    /// content-addressed so a cached result cannot prove a different graph.
    #[serde(default)]
    pub project_graph_basis: Option<ProjectGraphBasisV1>,
    pub state: StaticEvidenceStateV1,
    #[serde(default)]
    pub findings: Vec<FindingV1>,
    pub error_kind: Option<String>,
    pub duration_ms: Option<u64>,
    pub cache_hit: Option<bool>,
}

impl StaticEvidenceV1 {
    /// Checks the bounded, non-secret representation accepted from an adapter.
    /// The adapter itself owns parsing and path-root validation; this catches
    /// malformed persisted data before it can be reused.
    pub fn validate(&self) -> Result<(), VerificationError> {
        if self.schema_version != SCHEMA_VERSION
            || self.evidence_id.is_empty()
            || self.obligation_id.is_empty()
            || self.obligation_kind.is_empty()
            || self.analyzer_id.is_empty()
            || self.rule_id.is_empty()
            || self.tool_hash.is_empty()
            || self.config_hash.is_empty()
            || self.source_hash.is_empty()
            || self.diff_hash.is_empty()
            || self.findings.len() > MAX_FINDINGS
            || self.findings.iter().any(|finding| {
                finding.fingerprint.is_empty()
                    || finding.rule_id.is_empty()
                    || (finding.introduced_by_current_diff && finding.baseline)
                    || finding.path.as_ref().is_some_and(|path| {
                        path.starts_with('/') || path.split('/').any(|part| part == "..")
                    })
            })
            || self
                .project_graph_basis
                .as_ref()
                .is_some_and(|basis| basis.graph_hash.is_empty())
        {
            return Err(VerificationError::InvalidStaticEvidence);
        }
        let mut fingerprints = BTreeSet::new();
        if self
            .findings
            .iter()
            .any(|finding| !fingerprints.insert(&finding.fingerprint))
        {
            return Err(VerificationError::InvalidStaticEvidence);
        }
        Ok(())
    }

    /// A raw clean becomes Unknown unless the pre-registered rule proves this
    /// obligation at a complete scope.  This is the only place that grants a
    /// static `Satisfied` conclusion.
    pub fn effective_state(
        &self,
        capability: Option<&StaticCapabilityV1>,
    ) -> StaticEvidenceStateV1 {
        if self.validate().is_err() {
            return StaticEvidenceStateV1::Unknown;
        }
        if self.state != StaticEvidenceStateV1::Satisfied {
            return self.state;
        }
        let Some(rule) = capability.and_then(|capability| capability.rule(&self.rule_id)) else {
            return StaticEvidenceStateV1::Unknown;
        };
        let capability_matches = capability.is_some_and(|registered| {
            registered.schema_version == SCHEMA_VERSION
                && registered.analyzer_id == self.analyzer_id
                && registered.executable_hash == self.tool_hash
        });
        let scope_within_registered_capability =
            scope_rank(self.scope) <= scope_rank(rule.maximum_scope);
        let graph_proven = !rule.requires_project_graph
            || self
                .project_graph_basis
                .as_ref()
                .is_some_and(ProjectGraphBasisV1::valid_complete);
        if capability_matches
            && rule.clean_proves_satisfied
            && rule.obligation_kinds.contains(&self.obligation_kind)
            && self.error_kind.is_none()
            && self.scope_completeness.is_complete()
            && scope_within_registered_capability
            && graph_proven
            && self.findings.is_empty()
        {
            StaticEvidenceStateV1::Satisfied
        } else {
            StaticEvidenceStateV1::Unknown
        }
    }

    pub fn reusable_for(&self, source_hash: &str, diff_hash: &str, config_hash: &str) -> bool {
        self.source_hash == source_hash
            && self.diff_hash == diff_hash
            && self.config_hash == config_hash
    }
}

/// Exact request/scope identity supplied by the host when it invokes an
/// analyzer. It intentionally includes the reported scope and graph basis so
/// a file-level response cannot be reused as a workspace proof.
pub fn static_analysis_request_hash(evidence: &StaticEvidenceV1) -> String {
    #[derive(Serialize)]
    struct Canonical<'a> {
        obligation_id: &'a str,
        obligation_kind: &'a str,
        analyzer_id: &'a str,
        rule_id: &'a str,
        config_hash: &'a str,
        source_hash: &'a str,
        diff_hash: &'a str,
        scope: AnalysisScopeV1,
        project_graph_basis: &'a Option<ProjectGraphBasisV1>,
    }
    let canonical = Canonical {
        obligation_id: &evidence.obligation_id,
        obligation_kind: &evidence.obligation_kind,
        analyzer_id: &evidence.analyzer_id,
        rule_id: &evidence.rule_id,
        config_hash: &evidence.config_hash,
        source_hash: &evidence.source_hash,
        diff_hash: &evidence.diff_hash,
        scope: evidence.scope,
        project_graph_basis: &evidence.project_graph_basis,
    };
    let bytes = serde_json::to_vec(&canonical).expect("static request identity serializes");
    format!("{:x}", Sha256::digest(bytes))
}

fn valid_hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn scope_rank(scope: AnalysisScopeV1) -> u8 {
    match scope {
        AnalysisScopeV1::File => 0,
        AnalysisScopeV1::Package => 1,
        AnalysisScopeV1::Workspace => 2,
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MergedStaticEvidenceV1 {
    pub fingerprint: String,
    pub state: StaticEvidenceStateV1,
    pub sources: Vec<StaticEvidenceV1>,
}

/// Merges exact finding fingerprints deterministically. Disagreeing evidence
/// is retained verbatim but presented as Unknown; an adapter may not silently
/// override another adapter's conclusion.
pub fn merge_static_evidence(
    evidence: impl IntoIterator<Item = StaticEvidenceV1>,
    capabilities: &BTreeMap<String, StaticCapabilityV1>,
) -> Vec<MergedStaticEvidenceV1> {
    let mut obligations: BTreeMap<(String, String), Vec<StaticEvidenceV1>> = BTreeMap::new();
    for item in evidence {
        obligations
            .entry((item.obligation_id.clone(), item.rule_id.clone()))
            .or_default()
            .push(item);
    }
    let mut grouped: BTreeMap<String, Vec<StaticEvidenceV1>> = BTreeMap::new();
    for ((obligation_id, rule_id), sources) in obligations {
        let fingerprints = sources
            .iter()
            .flat_map(|source| {
                source
                    .findings
                    .iter()
                    .map(|finding| finding.fingerprint.clone())
            })
            .collect::<BTreeSet<_>>();
        if fingerprints.is_empty() {
            grouped.insert(format!("obligation:{obligation_id}:{rule_id}"), sources);
            continue;
        }
        for fingerprint in fingerprints {
            let members = sources
                .iter()
                .filter(|source| {
                    source.findings.is_empty()
                        || source
                            .findings
                            .iter()
                            .any(|finding| finding.fingerprint == fingerprint)
                })
                .cloned()
                .collect();
            grouped.insert(fingerprint, members);
        }
    }
    grouped
        .into_iter()
        .map(|(fingerprint, mut sources)| {
            sources.sort_by(|left, right| left.evidence_id.cmp(&right.evidence_id));
            let states: BTreeSet<_> = sources
                .iter()
                .map(|source| source.effective_state(capabilities.get(&source.analyzer_id)))
                .collect();
            let state = if states.len() == 1 {
                *states.iter().next().expect("one state")
            } else {
                StaticEvidenceStateV1::Unknown
            };
            MergedStaticEvidenceV1 {
                fingerprint,
                state,
                sources,
            }
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestCostClassV1 {
    Fast,
    Normal,
    Slow,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceLocationV1 {
    pub path: String,
    pub line: u32,
}

/// A `cargo test -- --list` complete name plus its package and target is
/// authoritative. Source locations are optional ranking hints, never a
/// replacement identity or a condition for exact selection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestEntryV1 {
    pub id: String,
    pub framework: String,
    pub package: String,
    pub target: String,
    pub exact_name: String,
    pub source_location: Option<SourceLocationV1>,
    pub dynamic_or_unknown: bool,
    pub cost: TestCostClassV1,
    #[serde(default)]
    pub symbols: BTreeSet<String>,
    #[serde(default)]
    pub common_case_ids: BTreeSet<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestInventoryV1 {
    pub schema_version: u32,
    pub inventory_hash: String,
    pub framework: String,
    /// A partial listing makes narrowing unsafe.
    pub complete: bool,
    pub entries: Vec<TestEntryV1>,
}

impl TestInventoryV1 {
    /// Build the only complete Rust inventory representation.  Its hash is
    /// over every sorted entry, so an unchanged claimed hash cannot be reused
    /// after a caller mutates the listing.
    pub fn complete_rust(mut entries: Vec<TestEntryV1>) -> Result<Self, VerificationError> {
        entries.sort_by(|left, right| left.id.cmp(&right.id));
        let inventory_hash = Self::canonical_entries_hash(&entries)?;
        let inventory = Self {
            schema_version: SCHEMA_VERSION,
            inventory_hash,
            framework: "rust".into(),
            complete: true,
            entries,
        };
        inventory.validate()?;
        Ok(inventory)
    }

    pub fn canonical_entries_hash(entries: &[TestEntryV1]) -> Result<String, VerificationError> {
        let mut canonical = entries.to_vec();
        canonical.sort_by(|left, right| left.id.cmp(&right.id));
        let bytes =
            serde_json::to_vec(&canonical).map_err(|_| VerificationError::InvalidInventory)?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }

    pub fn validate(&self) -> Result<(), VerificationError> {
        if self.schema_version != SCHEMA_VERSION
            || self.inventory_hash.is_empty()
            || self.framework.is_empty()
        {
            return Err(VerificationError::InvalidInventory);
        }
        let mut ids = BTreeSet::new();
        for entry in &self.entries {
            if entry.id.is_empty()
                || entry.exact_name.is_empty()
                || entry.package.is_empty()
                || entry.target.is_empty()
                || entry.framework != self.framework
                || entry.source_location.as_ref().is_some_and(|location| {
                    location.path.is_empty()
                        || location.path.starts_with('/')
                        || location.path.split('/').any(|part| part == "..")
                })
                || !ids.insert(&entry.id)
            {
                return Err(VerificationError::InvalidInventory);
            }
        }
        if self.inventory_hash != Self::canonical_entries_hash(&self.entries)? {
            return Err(VerificationError::InvalidInventory);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CandidateHintsV1 {
    pub changed_test_ids: BTreeSet<String>,
    pub packages: BTreeSet<String>,
    pub paths: BTreeSet<String>,
    pub symbols: BTreeSet<String>,
    pub fault_ids: BTreeSet<String>,
    pub common_case_ids: BTreeSet<String>,
    /// Ranking is advisory and may only add order. It may not erase a
    /// deterministically eligible candidate.
    pub bm25_ranked_ids: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CandidateSetV1 {
    pub ids: Vec<String>,
    /// Deterministically required tests (currently exact changed-test IDs).
    /// Advisory scoring may add tests but can never remove these IDs.
    #[serde(default)]
    pub mandatory_ids: BTreeSet<String>,
    pub inventory_complete: bool,
}

impl CandidateSetV1 {
    pub(crate) fn validate_against(
        &self,
        inventory: &TestInventoryV1,
    ) -> Result<(), VerificationError> {
        if self.inventory_complete != inventory.complete || self.ids.iter().any(String::is_empty) {
            return Err(VerificationError::InvalidCandidates);
        }
        let ids: BTreeSet<_> = self.ids.iter().collect();
        if ids.len() != self.ids.len() {
            return Err(VerificationError::InvalidCandidates);
        }
        let known: BTreeSet<_> = inventory
            .entries
            .iter()
            .map(|entry| entry.id.as_str())
            .collect();
        if self.ids.iter().any(|id| !known.contains(id.as_str())) {
            return Err(VerificationError::InvalidCandidates);
        }
        if self.mandatory_ids.iter().any(|id| !ids.contains(id)) {
            return Err(VerificationError::InvalidCandidates);
        }
        Ok(())
    }
}

/// Deterministic union: changed tests and dynamic/unknown entries survive
/// every heuristic. A missing source-location is deliberately not dynamic:
/// per-binary Cargo listings still provide an exact package/target/name
/// identity. BM25 only reorders candidates already in the union.
pub fn candidate_union(inventory: &TestInventoryV1, hints: &CandidateHintsV1) -> CandidateSetV1 {
    let mut kept = BTreeSet::new();
    for entry in &inventory.entries {
        let related = hints.changed_test_ids.contains(&entry.id)
            || entry.dynamic_or_unknown
            || hints.packages.contains(&entry.package)
            || entry
                .source_location
                .as_ref()
                .is_some_and(|location| hints.paths.contains(&location.path))
            || !entry.symbols.is_disjoint(&hints.symbols)
            || !entry.common_case_ids.is_disjoint(&hints.common_case_ids)
            || hints.fault_ids.contains(&entry.id);
        if related {
            kept.insert(entry.id.clone());
        }
    }
    let rank: BTreeMap<_, _> = hints
        .bm25_ranked_ids
        .iter()
        .enumerate()
        .map(|(i, id)| (id, i))
        .collect();
    let mut ids: Vec<_> = kept.into_iter().collect();
    ids.sort_by(|left, right| rank.get(left).cmp(&rank.get(right)).then(left.cmp(right)));
    CandidateSetV1 {
        ids,
        mandatory_ids: hints
            .changed_test_ids
            .iter()
            .filter(|id| inventory.entries.iter().any(|entry| &entry.id == *id))
            .cloned()
            .collect(),
        inventory_complete: inventory.complete,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllTestsScopeV1 {
    Package,
    Workspace,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TestSelectionV1 {
    Exact {
        ids: Vec<String>,
    },
    All {
        scope: AllTestsScopeV1,
        reason: AllReasonV1,
    },
}

/// The verification route is selected separately from existing-test impact.
/// Choice is appropriate here because one execution plan is chosen, while
/// test impact remains a non-exclusive Score decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationRouteV1 {
    ExistingTest,
    CommonCase,
    NewRegression,
    ManualReview,
}

impl VerificationRouteV1 {
    pub const SAFE_ORDER: [Self; 4] = [
        Self::ExistingTest,
        Self::CommonCase,
        Self::NewRegression,
        Self::ManualReview,
    ];
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationRouteCandidateV1 {
    pub id: String,
    pub route: VerificationRouteV1,
    /// Exact artifact/test identifier or a host-owned plan reference.
    pub source_ref: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PlanDecisionV1 {
    pub schema_version: u32,
    pub record_id: String,
    pub provider: String,
    pub model: String,
    pub question_hash: String,
    pub diff_hash: String,
    pub candidate_hash: String,
    pub selected_candidate_id: String,
    pub usage: DecisionUsageV1,
}

impl PlanDecisionV1 {
    pub(crate) fn validate_against(
        &self,
        candidates: &[VerificationRouteCandidateV1],
    ) -> Result<(), VerificationError> {
        if self.schema_version != SCHEMA_VERSION
            || self.record_id.is_empty()
            || self.provider.is_empty()
            || self.model.is_empty()
            || self.question_hash.is_empty()
            || self.diff_hash.is_empty()
            || self.candidate_hash
                != stable_id_hash(
                    &candidates
                        .iter()
                        .map(|item| item.id.clone())
                        .collect::<Vec<_>>(),
                )
            || !candidates
                .iter()
                .any(|item| item.id == self.selected_candidate_id)
        {
            return Err(VerificationError::InvalidPlanDecision);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AllReasonV1 {
    InvalidJudge,
    InvalidInventory,
    InvalidCandidates,
    MissingJudge,
    LowConfidence,
    IncompleteInventory,
    DynamicOrUnknown,
    TooManyCandidates,
    HashMismatch,
    NoSelectedTests,
}

impl TestSelectionV1 {
    pub fn validate(&self) -> Result<(), VerificationError> {
        match self {
            Self::Exact { ids } if ids.is_empty() || ids.iter().any(|id| id.is_empty()) => {
                Err(VerificationError::InvalidSelection)
            }
            Self::Exact { ids } if ids.windows(2).any(|pair| pair[0] >= pair[1]) => {
                Err(VerificationError::InvalidSelection)
            }
            _ => Ok(()),
        }
    }
}

/// A Cargo test-harness selector recovered from a host-owned, per-binary
/// listing.  This deliberately does not retain arbitrary Cargo arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RustTargetSelectorV1 {
    Lib,
    Bin(String),
    Test(String),
    Bench(String),
    Example(String),
}

impl RustTargetSelectorV1 {
    pub fn parse(value: &str) -> Result<Self, VerificationError> {
        if value == "lib" {
            return Ok(Self::Lib);
        }
        let Some((kind, name)) = value.split_once(':') else {
            return Err(VerificationError::InvalidSelection);
        };
        if !valid_cargo_selector_value(name) {
            return Err(VerificationError::InvalidSelection);
        }
        match kind {
            "bin" => Ok(Self::Bin(name.into())),
            "test" => Ok(Self::Test(name.into())),
            "bench" => Ok(Self::Bench(name.into())),
            "example" => Ok(Self::Example(name.into())),
            _ => Err(VerificationError::InvalidSelection),
        }
    }

    fn cargo_args(&self) -> Vec<String> {
        match self {
            Self::Lib => vec!["--lib".into()],
            Self::Bin(name) => vec!["--bin".into(), name.clone()],
            Self::Test(name) => vec!["--test".into(), name.clone()],
            Self::Bench(name) => vec!["--bench".into(), name.clone()],
            Self::Example(name) => vec!["--example".into(), name.clone()],
        }
    }
}

/// A host-executable Rust test plan.  It has no cwd, environment, shell text,
/// or model-provided arguments; the host may only execute `argv()` as fixed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustTestInvocationV1 {
    argv: Vec<String>,
}

impl RustTestInvocationV1 {
    pub fn argv(&self) -> &[String] {
        &self.argv
    }
}

/// A sealed, host-executable projection of one persisted test selection.
///
/// The plan contains no cwd, environment, shell text, timeout, or model
/// arguments.  A process owner must validate it against the immutable
/// inventory again immediately before execution, then return a separately
/// typed outcome.  Keeping that ownership outside this module prevents a
/// stale Jev choice from silently becoming a process request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustTestExecutionPlanV1 {
    pub inventory_hash: String,
    pub selection: TestSelectionV1,
    pub invocations: Vec<RustTestInvocationV1>,
}

impl RustTestExecutionPlanV1 {
    pub fn build(
        selection: &TestSelectionV1,
        inventory: &TestInventoryV1,
        package_for_all: Option<&str>,
    ) -> Result<Self, VerificationError> {
        let invocations = project_rust_test_selection(selection, inventory, package_for_all)?;
        let value = Self {
            inventory_hash: inventory.inventory_hash.clone(),
            selection: selection.clone(),
            invocations,
        };
        value.validate_against(inventory, package_for_all)?;
        Ok(value)
    }

    pub fn validate_against(
        &self,
        inventory: &TestInventoryV1,
        package_for_all: Option<&str>,
    ) -> Result<(), VerificationError> {
        if self.inventory_hash != inventory.inventory_hash {
            return Err(VerificationError::InvalidSelection);
        }
        let expected = project_rust_test_selection(&self.selection, inventory, package_for_all)?;
        if self.invocations.is_empty() || self.invocations != expected {
            return Err(VerificationError::InvalidSelection);
        }
        Ok(())
    }

    pub fn plan_hash(&self) -> String {
        let bytes = serde_json::to_vec(self).expect("bounded test execution plan serializes");
        format!("{:x}", Sha256::digest(bytes))
    }
}

/// The only result shape an external Rust-test executor may return to the
/// verification owner. Raw output is deliberately excluded; evidence records
/// retain only the sealed plan identity and terminal result.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RustTestExecutionOutcomeV1 {
    pub plan_hash: String,
    pub execution_available: bool,
    pub cleanup_complete: bool,
    pub passed: Option<bool>,
}

impl RustTestExecutionOutcomeV1 {
    pub fn validate_for(&self, plan: &RustTestExecutionPlanV1) -> Result<(), VerificationError> {
        if self.plan_hash != plan.plan_hash()
            || (!self.execution_available && self.passed.is_some())
            || (self.execution_available && self.passed.is_none())
        {
            return Err(VerificationError::InvalidSelection);
        }
        Ok(())
    }
}

/// Turn a hash-bound selection into fixed Cargo argv vectors.  Exact IDs must
/// resolve to the supplied inventory.  `All::Package` requires a known
/// package, while `All::Workspace` never smuggles a package selector in.
pub fn project_rust_test_selection(
    selection: &TestSelectionV1,
    inventory: &TestInventoryV1,
    package_for_all: Option<&str>,
) -> Result<Vec<RustTestInvocationV1>, VerificationError> {
    selection.validate()?;
    inventory.validate()?;
    if inventory.framework != "rust" {
        return Err(VerificationError::InvalidInventory);
    }
    match selection {
        TestSelectionV1::Exact { ids } => ids
            .iter()
            .map(|id| {
                let entry = inventory
                    .entries
                    .iter()
                    .find(|entry| entry.id == *id)
                    .ok_or(VerificationError::InvalidSelection)?;
                if entry.dynamic_or_unknown
                    || !valid_cargo_selector_value(&entry.package)
                    || !valid_cargo_selector_value(&entry.exact_name)
                {
                    return Err(VerificationError::InvalidSelection);
                }
                let target = RustTargetSelectorV1::parse(&entry.target)?;
                let mut argv = vec![
                    "cargo".into(),
                    "test".into(),
                    "--package".into(),
                    entry.package.clone(),
                ];
                argv.extend(target.cargo_args());
                argv.extend(["--".into(), entry.exact_name.clone(), "--exact".into()]);
                Ok(RustTestInvocationV1 { argv })
            })
            .collect(),
        TestSelectionV1::All {
            scope: AllTestsScopeV1::Package,
            ..
        } => {
            let package = package_for_all.ok_or(VerificationError::InvalidSelection)?;
            if !valid_cargo_selector_value(package)
                || !inventory
                    .entries
                    .iter()
                    .any(|entry| entry.package == package)
            {
                return Err(VerificationError::InvalidSelection);
            }
            Ok(vec![RustTestInvocationV1 {
                argv: vec![
                    "cargo".into(),
                    "test".into(),
                    "--package".into(),
                    package.into(),
                ],
            }])
        }
        TestSelectionV1::All {
            scope: AllTestsScopeV1::Workspace,
            ..
        } => Ok(vec![RustTestInvocationV1 {
            argv: vec!["cargo".into(), "test".into(), "--workspace".into()],
        }]),
    }
}

fn valid_cargo_selector_value(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('-')
        && !value.contains('\0')
        && !value.contains(['/', '\\'])
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestScoreAnswerV1 {
    pub candidate_id: String,
    /// The probability-weighted score returned by the typed decider.  It is
    /// deliberately not rounded before persistence.
    pub score: f64,
    /// Exact probability distribution over the four score levels, keyed by
    /// `"0"` through `"3"`.
    pub probabilities: BTreeMap<String, f64>,
    pub confidence: Option<f64>,
}

impl TestScoreAnswerV1 {
    fn valid(&self) -> bool {
        self.score.is_finite()
            && (0.0..=3.0).contains(&self.score)
            && self.probabilities.len() == 4
            && (0..4).all(|level| self.probabilities.contains_key(&level.to_string()))
            && self
                .probabilities
                .values()
                .all(|value| (0.0..=1.0).contains(value))
            && ((self.probabilities.values().sum::<f64>() - 1.0).abs() < 0.01)
            && ((0..4)
                .map(|level| level as f64 * self.probabilities[&level.to_string()])
                .sum::<f64>()
                - self.score)
                .abs()
                < 0.01
            && self
                .confidence
                .is_none_or(|value| (0.0..=1.0).contains(&value))
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionUsageV1 {
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
    pub micro_usd: Option<u64>,
    pub latency_ms: Option<u64>,
    pub missing_usage: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TestDecisionRecordV1 {
    pub schema_version: u32,
    pub record_id: String,
    pub provider: String,
    pub model: String,
    pub question_hash: String,
    pub diff_hash: String,
    pub inventory_hash: String,
    pub candidate_hash: String,
    pub answers: Vec<TestScoreAnswerV1>,
    pub usage: DecisionUsageV1,
}

impl TestDecisionRecordV1 {
    pub(crate) fn validate(&self) -> Result<(), VerificationError> {
        if self.schema_version != SCHEMA_VERSION
            || self.record_id.is_empty()
            || self.provider.is_empty()
            || self.model.is_empty()
            || self.question_hash.is_empty()
            || self.diff_hash.is_empty()
            || self.inventory_hash.is_empty()
            || self.candidate_hash.is_empty()
            || self.answers.len() > MAX_CANDIDATES_PER_NEED
        {
            return Err(VerificationError::InvalidSelection);
        }
        let mut ids = BTreeSet::new();
        if self.answers.iter().any(|answer| {
            answer.candidate_id.is_empty() || !answer.valid() || !ids.insert(&answer.candidate_id)
        }) {
            return Err(VerificationError::InvalidSelection);
        }
        Ok(())
    }

    pub fn reusable_for(
        &self,
        question_hash: &str,
        diff_hash: &str,
        inventory_hash: &str,
        candidate_hash: &str,
    ) -> bool {
        self.schema_version == SCHEMA_VERSION
            && self.question_hash == question_hash
            && self.diff_hash == diff_hash
            && self.inventory_hash == inventory_hash
            && self.candidate_hash == candidate_hash
    }
}

/// Applies a stored judge answer without any network access. Any missing,
/// malformed, stale, dynamic, incomplete, or low-confidence input expands to
/// an explicit All selection. `Exact([])` is never emitted.
pub fn select_tests_offline(
    record: Option<&TestDecisionRecordV1>,
    inventory: &TestInventoryV1,
    candidates: &CandidateSetV1,
    question_hash: &str,
    diff_hash: &str,
    cutoff: u8,
    all_scope: AllTestsScopeV1,
) -> TestSelectionV1 {
    let all = |reason| TestSelectionV1::All {
        scope: all_scope,
        reason,
    };
    if inventory.validate().is_err() {
        return all(AllReasonV1::InvalidInventory);
    }
    if candidates.validate_against(inventory).is_err() {
        return all(AllReasonV1::InvalidCandidates);
    }
    if !inventory.complete || !candidates.inventory_complete {
        return all(AllReasonV1::IncompleteInventory);
    }
    if candidates.ids.len() > MAX_CANDIDATES_PER_NEED {
        return all(AllReasonV1::TooManyCandidates);
    }
    let candidate_hash = stable_id_hash(&candidates.ids);
    let Some(record) = record else {
        return all(AllReasonV1::MissingJudge);
    };
    if !record.reusable_for(
        question_hash,
        diff_hash,
        &inventory.inventory_hash,
        &candidate_hash,
    ) {
        return all(AllReasonV1::HashMismatch);
    }
    let expected: BTreeSet<_> = candidates.ids.iter().cloned().collect();
    let answered: BTreeSet<_> = record
        .answers
        .iter()
        .map(|answer| answer.candidate_id.clone())
        .collect();
    if expected != answered
        || record.answers.len() != expected.len()
        || record.answers.iter().any(|answer| !answer.valid())
    {
        return all(AllReasonV1::InvalidJudge);
    }
    let by_id: BTreeMap<_, _> = inventory
        .entries
        .iter()
        .map(|entry| (&entry.id, entry))
        .collect();
    if candidates
        .ids
        .iter()
        .any(|id| by_id.get(id).is_none_or(|entry| entry.dynamic_or_unknown))
    {
        return all(AllReasonV1::DynamicOrUnknown);
    }
    if record
        .answers
        .iter()
        .any(|answer| answer.confidence.is_none_or(|value| value < LOW_CONFIDENCE))
    {
        return all(AllReasonV1::LowConfidence);
    }
    let mut selected: Vec<_> = record
        .answers
        .iter()
        .filter_map(|answer| (answer.score >= cutoff as f64).then_some(answer.candidate_id.clone()))
        .collect();
    selected.extend(candidates.mandatory_ids.iter().cloned());
    selected.sort();
    selected.dedup();
    selected.dedup();
    if selected.is_empty() {
        all(AllReasonV1::NoSelectedTests)
    } else {
        TestSelectionV1::Exact { ids: selected }
    }
}

pub fn stable_id_hash(ids: &[String]) -> String {
    let mut canonical = ids.to_vec();
    canonical.sort();
    canonical.dedup();
    format!("{:x}", Sha256::digest(canonical.join("\n").as_bytes()))
}

#[derive(Debug, thiserror::Error, Clone, PartialEq, Eq)]
pub enum VerificationError {
    #[error("invalid verification need")]
    InvalidNeed,
    #[error("invalid test inventory")]
    InvalidInventory,
    #[error("invalid test candidates")]
    InvalidCandidates,
    #[error("invalid static evidence")]
    InvalidStaticEvidence,
    #[error("invalid static capability")]
    InvalidStaticCapability,
    #[error("invalid test selection")]
    InvalidSelection,
    #[error("invalid verification plan decision")]
    InvalidPlanDecision,
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod invocation_tests {
    use super::*;

    fn entry(id: &str, package: &str, target: &str) -> TestEntryV1 {
        TestEntryV1 {
            id: id.into(),
            framework: "rust".into(),
            package: package.into(),
            target: target.into(),
            exact_name: "suite::same".into(),
            source_location: None,
            dynamic_or_unknown: false,
            cost: TestCostClassV1::Fast,
            symbols: BTreeSet::new(),
            common_case_ids: BTreeSet::new(),
        }
    }

    #[test]
    fn exact_projection_is_target_aware_and_rejects_unknown_ids() {
        let inventory =
            TestInventoryV1::complete_rust(vec![entry("one", "core", "test:api")]).unwrap();
        let invocations = project_rust_test_selection(
            &TestSelectionV1::Exact {
                ids: vec!["one".into()],
            },
            &inventory,
            None,
        )
        .unwrap();
        assert_eq!(
            invocations[0].argv(),
            [
                "cargo",
                "test",
                "--package",
                "core",
                "--test",
                "api",
                "--",
                "suite::same",
                "--exact",
            ]
        );
        assert!(
            project_rust_test_selection(
                &TestSelectionV1::Exact {
                    ids: vec!["stale".into()],
                },
                &inventory,
                None,
            )
            .is_err()
        );
    }

    #[test]
    fn all_projection_preserves_package_and_workspace_scope() {
        let inventory = TestInventoryV1::complete_rust(vec![entry("one", "core", "lib")]).unwrap();
        let package = project_rust_test_selection(
            &TestSelectionV1::All {
                scope: AllTestsScopeV1::Package,
                reason: AllReasonV1::MissingJudge,
            },
            &inventory,
            Some("core"),
        )
        .unwrap();
        assert_eq!(package[0].argv(), ["cargo", "test", "--package", "core"]);
        let workspace = project_rust_test_selection(
            &TestSelectionV1::All {
                scope: AllTestsScopeV1::Workspace,
                reason: AllReasonV1::MissingJudge,
            },
            &inventory,
            Some("core"),
        )
        .unwrap();
        assert_eq!(workspace[0].argv(), ["cargo", "test", "--workspace"]);
    }

    #[test]
    fn projection_rejects_dynamic_entries_and_invalid_target_selectors() {
        let mut dynamic = entry("dynamic", "core", "lib");
        dynamic.dynamic_or_unknown = true;
        let dynamic_inventory = TestInventoryV1::complete_rust(vec![dynamic]).unwrap();
        assert!(
            project_rust_test_selection(
                &TestSelectionV1::Exact {
                    ids: vec!["dynamic".into()],
                },
                &dynamic_inventory,
                None,
            )
            .is_err()
        );
        assert!(RustTargetSelectorV1::parse("--test=api").is_err());
        assert!(RustTargetSelectorV1::parse("test:../api").is_err());
        assert!(RustTargetSelectorV1::parse("test:api").is_ok());
    }

    #[test]
    fn execution_plan_keeps_same_named_tests_separate_by_package_and_target() {
        let inventory = TestInventoryV1::complete_rust(vec![
            entry("lib-same", "core", "lib"),
            entry("integration-same", "core", "test:integration"),
        ])
        .unwrap();
        let plan = RustTestExecutionPlanV1::build(
            &TestSelectionV1::Exact {
                ids: vec!["integration-same".into(), "lib-same".into()],
            },
            &inventory,
            None,
        )
        .unwrap();
        assert_eq!(plan.invocations.len(), 2);
        assert_ne!(plan.invocations[0].argv(), plan.invocations[1].argv());
        assert!(plan.validate_against(&inventory, None).is_ok());
        let outcome = RustTestExecutionOutcomeV1 {
            plan_hash: plan.plan_hash(),
            execution_available: true,
            cleanup_complete: true,
            passed: Some(true),
        };
        assert!(outcome.validate_for(&plan).is_ok());
    }

    #[test]
    fn execution_plan_rejects_tampered_argv_and_unavailable_success() {
        let inventory = TestInventoryV1::complete_rust(vec![entry("one", "core", "lib")]).unwrap();
        let mut plan = RustTestExecutionPlanV1::build(
            &TestSelectionV1::Exact {
                ids: vec!["one".into()],
            },
            &inventory,
            None,
        )
        .unwrap();
        plan.invocations[0].argv.push("--ignored".into());
        assert!(plan.validate_against(&inventory, None).is_err());
        let unavailable = RustTestExecutionOutcomeV1 {
            plan_hash: plan.plan_hash(),
            execution_available: false,
            cleanup_complete: true,
            passed: Some(true),
        };
        assert!(unavailable.validate_for(&plan).is_err());
    }

    #[test]
    fn source_location_absence_does_not_expand_complete_identity_candidates() {
        let inventory = TestInventoryV1::complete_rust(vec![
            entry("one", "core", "lib"),
            entry("two", "core", "test:api"),
        ])
        .unwrap();
        assert_eq!(
            candidate_union(&inventory, &CandidateHintsV1::default()).ids,
            Vec::<String>::new()
        );
        let dynamic = TestEntryV1 {
            dynamic_or_unknown: true,
            ..entry("dynamic", "core", "lib")
        };
        let dynamic_inventory = TestInventoryV1::complete_rust(vec![dynamic]).unwrap();
        assert_eq!(
            candidate_union(&dynamic_inventory, &CandidateHintsV1::default()).ids,
            vec!["dynamic"]
        );
    }
}
