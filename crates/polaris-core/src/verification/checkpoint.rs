//! Durable, host-owned verification checkpoint metadata.
//!
//! This is intentionally a reference-only state machine.  It cannot retain a
//! command, path, tool output, pass boolean, or native cleanup handle.  The
//! product owner resolves those from its fixed adapters and seals convergence
//! before publication.

use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};

use crate::workflow::{ArtifactRefV1, EvidenceBindingV1};

use super::{
    AllReasonV1, AllTestsScopeV1, AnalysisScopeV1, CandidateHintsV1, EvidenceKindV1, FindingV1,
    RustTestExecutionOutcomeV1, RustTestExecutionPlanV1, ScopeCompletenessV1,
    StaticCapabilityCatalogV1, StaticCapabilityV1, StaticEvidenceStateV1, StaticEvidenceV1,
    TestDecisionRecordV1, TestInventoryV1, TestSelectionV1,
    adapters::{StaticAdapterInvocationV1, StaticAdapterOutcomeV1, StaticAdapterParserV1},
    owner::VerifiedConvergenceBundleV1,
    store::VerificationRecordRefV1,
};

const SCHEMA_VERSION: u32 = 1;
const MAX_REFS: usize = 64;
const MAX_ID: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationCheckpointStageV1 {
    Prepared,
    Running,
    Terminal,
    Published,
    OutcomeUnknown,
}

impl VerificationCheckpointStageV1 {
    pub(crate) fn can_transition_to(self, next: Self) -> bool {
        matches!(
            (self, next),
            (Self::Prepared, Self::Running)
                | (Self::Prepared, Self::OutcomeUnknown)
                | (Self::Running, Self::Terminal)
                | (Self::Running, Self::OutcomeUnknown)
                | (Self::Terminal, Self::Published)
        )
    }
}

/// Hashes observed by the fixed common-case runner.  These are provenance,
/// never model-selected execution inputs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunnerProvenanceV1 {
    pub plan_hash: String,
    pub program_hash: String,
    pub report_hash: String,
    pub cleanup_complete: bool,
    pub execution_available: bool,
}

/// One host-selected execution route per required criterion.  The route is
/// fixed before `Running` is persisted; tool arguments cannot add or replace
/// a criterion, evidence kind, executable or plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CriterionRunV1 {
    pub criterion_id: String,
    pub evidence_kind: EvidenceKindV1,
    pub runner_id: String,
    pub provenance: Option<RunnerProvenanceV1>,
}

impl CriterionRunV1 {
    fn validate(&self) -> Result<(), String> {
        if !opaque(&self.criterion_id) || !opaque(&self.runner_id) {
            return Err("verification criterion run identity is invalid".into());
        }
        if let Some(provenance) = &self.provenance {
            provenance.validate()?;
        }
        Ok(())
    }
}

impl RunnerProvenanceV1 {
    fn validate(&self) -> Result<(), String> {
        for hash in [&self.plan_hash, &self.program_hash, &self.report_hash] {
            crate::workflow::validate_hash(hash)?;
        }
        Ok(())
    }

    pub fn can_pass(&self) -> bool {
        self.cleanup_complete && self.execution_available
    }
}

/// The durable part of a verification attempt.  All record references are
/// exact and content-addressed; records themselves remain in VerificationStore.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedVerificationCheckpointV1 {
    pub schema_version: u32,
    pub checkpoint_id: String,
    pub idempotency_id: String,
    pub workflow_revision: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub active_artifacts: Vec<ArtifactRefV1>,
    pub evidence_binding: EvidenceBindingV1,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub need_refs: Vec<VerificationRecordRefV1>,
    pub inventory_ref: Option<VerificationRecordRefV1>,
    pub candidate_ref: Option<VerificationRecordRefV1>,
    pub jev_decision_ref: Option<VerificationRecordRefV1>,
    pub plan_decision_ref: Option<VerificationRecordRefV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub static_evidence_refs: Vec<VerificationRecordRefV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub review_refs: Vec<VerificationRecordRefV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_refs: Vec<VerificationRecordRefV1>,
    pub selection: Option<TestSelectionV1>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub all_tests_scope: Option<AllTestsScopeV1>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub selections: BTreeMap<String, TestSelectionV1>,
    /// Exact argv projections reconstructed from the immutable inventory and
    /// selection before a host-owned executor may run them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub test_execution_plans: BTreeMap<String, RustTestExecutionPlanV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub jev_decision_refs: Vec<VerificationRecordRefV1>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub criterion_runs: Vec<CriterionRunV1>,
    pub stage: VerificationCheckpointStageV1,
}

impl SavedVerificationCheckpointV1 {
    pub fn new(
        checkpoint_id: String,
        idempotency_id: String,
        workflow_revision: u64,
        active_artifacts: Vec<ArtifactRefV1>,
        evidence_binding: EvidenceBindingV1,
    ) -> Result<Self, String> {
        let value = Self {
            schema_version: SCHEMA_VERSION,
            checkpoint_id,
            idempotency_id,
            workflow_revision,
            active_artifacts,
            evidence_binding,
            need_refs: Vec::new(),
            inventory_ref: None,
            candidate_ref: None,
            jev_decision_ref: None,
            plan_decision_ref: None,
            static_evidence_refs: Vec::new(),
            review_refs: Vec::new(),
            evidence_refs: Vec::new(),
            selection: None,
            all_tests_scope: None,
            selections: BTreeMap::new(),
            test_execution_plans: BTreeMap::new(),
            jev_decision_refs: Vec::new(),
            criterion_runs: Vec::new(),
            stage: VerificationCheckpointStageV1::Prepared,
        };
        value.validate()?;
        Ok(value)
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_version != SCHEMA_VERSION
            || !opaque(&self.checkpoint_id)
            || !opaque(&self.idempotency_id)
            || self.active_artifacts.is_empty()
            || self.active_artifacts.len() > MAX_REFS
        {
            return Err("verification checkpoint is invalid".into());
        }
        self.evidence_binding.validate()?;
        for artifact in &self.active_artifacts {
            artifact.validate()?;
        }
        for refs in [
            &self.need_refs,
            &self.static_evidence_refs,
            &self.review_refs,
            &self.evidence_refs,
            &self.jev_decision_refs,
        ] {
            if refs.len() > MAX_REFS {
                return Err("verification checkpoint references exceed limit".into());
            }
            for reference in refs {
                reference.validate().map_err(|e| e.to_string())?;
            }
        }
        for reference in [
            &self.inventory_ref,
            &self.candidate_ref,
            &self.jev_decision_ref,
            &self.plan_decision_ref,
        ]
        .into_iter()
        .flatten()
        {
            reference.validate().map_err(|e| e.to_string())?;
        }
        if let Some(selection) = &self.selection {
            selection.validate().map_err(|e| e.to_string())?;
        }
        for (criterion_id, selection) in &self.selections {
            if !opaque(criterion_id) {
                return Err("verification selection criterion is invalid".into());
            }
            selection.validate().map_err(|e| e.to_string())?;
        }
        if self.test_execution_plans.keys().any(|criterion_id| {
            !self.selections.contains_key(criterion_id) || !opaque(criterion_id)
        }) {
            return Err("verification execution plan criterion is invalid".into());
        }
        let mut criteria = std::collections::BTreeSet::new();
        for run in &self.criterion_runs {
            run.validate()?;
            if !criteria.insert(&run.criterion_id) {
                return Err("verification criterion run is duplicated".into());
            }
        }
        if matches!(
            self.stage,
            VerificationCheckpointStageV1::Terminal | VerificationCheckpointStageV1::Published
        ) && (self.criterion_runs.is_empty()
            || self.criterion_runs.iter().any(|run| {
                run.provenance
                    .as_ref()
                    .is_none_or(|runner| !runner.can_pass())
            })
            || self.evidence_refs.len() != self.criterion_runs.len())
        {
            return Err(
                "pending or unavailable cleanup cannot become a passed verification checkpoint"
                    .into(),
            );
        }
        Ok(())
    }

    pub fn transition(&mut self, next: VerificationCheckpointStageV1) -> Result<(), String> {
        if !self.stage.can_transition_to(next) {
            return Err("verification checkpoint transition is invalid".into());
        }
        if matches!(
            next,
            VerificationCheckpointStageV1::Terminal | VerificationCheckpointStageV1::Published
        ) && (self.criterion_runs.is_empty()
            || self.criterion_runs.iter().any(|run| {
                run.provenance
                    .as_ref()
                    .is_none_or(|runner| !runner.can_pass())
            })
            || self.evidence_refs.len() != self.criterion_runs.len())
        {
            return Err("verification execution is unavailable or cleanup is pending".into());
        }
        self.stage = next;
        self.validate()
    }

    /// Recovery deliberately never resumes a running attempt automatically.
    pub fn mark_crash_recovered(&mut self) {
        if self.stage == VerificationCheckpointStageV1::Running {
            self.stage = VerificationCheckpointStageV1::OutcomeUnknown;
        }
    }

    pub(crate) fn same_preparation_basis(&self, other: &Self) -> bool {
        let run_basis = |runs: &[CriterionRunV1]| {
            runs.iter()
                .map(|run| {
                    (
                        run.criterion_id.clone(),
                        run.evidence_kind,
                        run.runner_id.clone(),
                    )
                })
                .collect::<Vec<_>>()
        };
        self.checkpoint_id == other.checkpoint_id
            && self.idempotency_id == other.idempotency_id
            && self.workflow_revision == other.workflow_revision
            && self.active_artifacts == other.active_artifacts
            && self.evidence_binding == other.evidence_binding
            && self.need_refs == other.need_refs
            && self.inventory_ref == other.inventory_ref
            && self.candidate_ref == other.candidate_ref
            && self.jev_decision_ref == other.jev_decision_ref
            && self.jev_decision_refs == other.jev_decision_refs
            && self.plan_decision_ref == other.plan_decision_ref
            && self.review_refs == other.review_refs
            && self.selection == other.selection
            && self.all_tests_scope == other.all_tests_scope
            && self.selections == other.selections
            && self.test_execution_plans == other.test_execution_plans
            && run_basis(&self.criterion_runs) == run_basis(&other.criterion_runs)
    }
}

/// Parsed tool operations contain only host-issued opaque handles.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationCheckpointActionV1 {
    Prepare,
    Run { preparation_id: String },
    Publish { checkpoint_id: String },
}

/// The owner request carries a sealed bundle only after the host has run the
/// fixed adapter.  Tool parsing can never construct this variant.
#[expect(
    clippy::large_enum_variant,
    reason = "public host boundary preserves an allocation-free sealed bundle request"
)]
pub enum VerificationCheckpointOwnerRequestV1 {
    Prepare,
    Run {
        preparation_id: String,
    },
    Publish {
        checkpoint_id: String,
        bundle: VerifiedConvergenceBundleV1,
    },
}

/// Host boundary used by the agent loop. Implementations resolve immutable
/// artifacts, registered analyzers, fixed runner plans and the durable
/// checkpoint store. They must never derive any of those from tool text.
pub trait VerificationToolOwnerV1: Send + Sync {
    fn prepare(&self) -> Result<SavedVerificationCheckpointV1, String>;
    fn begin_run(&self, preparation_id: &str) -> Result<SavedVerificationCheckpointV1, String>;
    fn poll_run(&self, preparation_id: &str) -> Result<SavedVerificationCheckpointV1, String>;
    fn publish(
        &self,
        checkpoint_id: &str,
    ) -> Result<(SavedVerificationCheckpointV1, VerifiedConvergenceBundleV1), String>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostObservedVerificationV1 {
    pub subject_hash: String,
    pub source_hash: String,
    pub diff_hash: String,
    pub project_graph_hash: String,
}
impl HostObservedVerificationV1 {
    pub fn validate(&self) -> Result<(), String> {
        for hash in [
            &self.subject_hash,
            &self.source_hash,
            &self.diff_hash,
            &self.project_graph_hash,
        ] {
            crate::workflow::validate_hash(hash)?;
        }
        Ok(())
    }
}
pub trait VerificationObserverV1: Send + Sync {
    fn observe(&self) -> Result<HostObservedVerificationV1, String>;
}

/// Immutable identity of a host-owned exact-test executor.  It binds the
/// program and fixed execution configuration into the verification store;
/// model or wire input cannot select either value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RustTestExecutorIdentityV1 {
    pub runner_id: String,
    pub program_hash: String,
    pub config_hash: String,
}

impl RustTestExecutorIdentityV1 {
    fn validate(&self) -> Result<(), String> {
        if !opaque(&self.runner_id) {
            return Err("existing-test executor identity is invalid".into());
        }
        crate::workflow::validate_hash(&self.program_hash)?;
        crate::workflow::validate_hash(&self.config_hash)
    }
}

/// Host injection point for exact Rust tests.  Implementations own their
/// program allowlist, sandbox, cwd, environment and timeout.  They may run
/// only the already-validated plan and must report unavailable/timeout rather
/// than retrying or widening the boundary.
pub trait RustTestExecutorV1: Send + Sync {
    fn identity(&self) -> Result<RustTestExecutorIdentityV1, String>;
    fn execute(&self, plan: &RustTestExecutionPlanV1) -> RustTestExecutionOutcomeV1;
}

/// A concrete host-owned executor for the normal Cargo path.  The caller
/// injects every process boundary at construction; the persisted plan merely
/// supplies fixed Cargo arguments reconstructed from the inventory.
#[derive(Clone)]
pub struct FixedRustTestExecutorV1 {
    pub runner_id: String,
    pub cargo_program: PathBuf,
    pub policy: polaris_sandbox::SandboxPolicy,
    pub cwd: PathBuf,
    pub write_root: PathBuf,
    pub timeout: Duration,
}

impl FixedRustTestExecutorV1 {
    fn unavailable(&self, plan: &RustTestExecutionPlanV1) -> RustTestExecutionOutcomeV1 {
        RustTestExecutionOutcomeV1 {
            plan_hash: plan.plan_hash(),
            execution_available: false,
            cleanup_complete: false,
            passed: None,
        }
    }
}

impl RustTestExecutorV1 for FixedRustTestExecutorV1 {
    fn identity(&self) -> Result<RustTestExecutorIdentityV1, String> {
        if !opaque(&self.runner_id) || !self.cargo_program.is_absolute() || self.timeout.is_zero() {
            return Err("fixed existing-test executor configuration is invalid".into());
        }
        let program = std::fs::read(&self.cargo_program)
            .map_err(|error| format!("fixed Cargo program is unreadable: {error}"))?;
        let config = serde_json::to_vec(&(
            &self.runner_id,
            &self.cargo_program,
            &self.cwd,
            &self.write_root,
            self.timeout.as_millis(),
        ))
        .map_err(|_| "fixed existing-test executor cannot be hashed")?;
        Ok(RustTestExecutorIdentityV1 {
            runner_id: self.runner_id.clone(),
            program_hash: crate::conversation_state::content_hash(&program),
            config_hash: crate::conversation_state::content_hash(&config),
        })
    }

    fn execute(&self, plan: &RustTestExecutionPlanV1) -> RustTestExecutionOutcomeV1 {
        if self.identity().is_err() || plan.invocations.is_empty() {
            return self.unavailable(plan);
        }
        for invocation in &plan.invocations {
            let Some(program) = invocation.argv().first() else {
                return self.unavailable(plan);
            };
            if program != "cargo" {
                return self.unavailable(plan);
            }
            let mut argv = invocation.argv().to_vec();
            argv[0] = self.cargo_program.to_string_lossy().into_owned();
            let checked = crate::common_tests::Plan {
                version: crate::common_tests::SCHEMA_VERSION,
                argv,
                stdin: None,
                cases: vec![crate::common_tests::Case::ExitsWith { status: 0 }],
            };
            let mut outcome = crate::common_tests::run(
                &checked,
                crate::common_tests::RunConfig {
                    policy: &self.policy,
                    cwd: &self.cwd,
                    write_root: &self.write_root,
                    timeout: self.timeout,
                },
            );
            let cleanup_complete = match outcome.cleanup_mut() {
                Some(cleanup) => cleanup.try_stop_reap_and_cleanup().unwrap_or(false),
                None => true,
            };
            let execution_available = matches!(
                &outcome.report,
                crate::common_tests::RunReport::Passed { .. }
                    | crate::common_tests::RunReport::Failed {
                        identity: Some(_),
                        fallback_required: false,
                        ..
                    }
            ) && cleanup_complete;
            let passed = matches!(
                &outcome.report,
                crate::common_tests::RunReport::Passed {
                    cases,
                    fallback_required: false,
                    ..
                } if !cases.is_empty() && cases.iter().all(|case| case.passed)
            );
            if !execution_available {
                return self.unavailable(plan);
            }
            if !passed {
                return RustTestExecutionOutcomeV1 {
                    plan_hash: plan.plan_hash(),
                    execution_available: true,
                    cleanup_complete,
                    passed: Some(false),
                };
            }
        }
        RustTestExecutionOutcomeV1 {
            plan_hash: plan.plan_hash(),
            execution_available: true,
            cleanup_complete: true,
            passed: Some(true),
        }
    }
}

#[derive(Clone)]
pub struct FixedCommonRunnerV1 {
    pub criterion_id: String,
    pub evidence_kind: EvidenceKindV1,
    pub runner_id: String,
    pub plan: crate::common_tests::Plan,
    pub policy: polaris_sandbox::SandboxPolicy,
    pub cwd: PathBuf,
    pub write_root: PathBuf,
    pub timeout: Duration,
}
impl FixedCommonRunnerV1 {
    fn run(&self) -> crate::common_tests::RunOutcome {
        crate::common_tests::run(
            &self.plan,
            crate::common_tests::RunConfig {
                policy: &self.policy,
                cwd: &self.cwd,
                write_root: &self.write_root,
                timeout: self.timeout,
            },
        )
    }

    fn registered_identity(&self) -> Result<RegisteredRunnerIdentityV1, String> {
        self.plan
            .check()
            .map_err(|error| format!("invalid fixed runner plan: {error:?}"))?;
        let program = self
            .plan
            .argv
            .first()
            .ok_or("fixed runner program is absent")?;
        let program_bytes = std::fs::read(program)
            .map_err(|error| format!("fixed runner program is unreadable: {error}"))?;
        let plan_bytes =
            serde_json::to_vec(&self.plan).map_err(|_| "fixed runner plan cannot be hashed")?;
        let plan_hash = crate::conversation_state::content_hash(&plan_bytes);
        let program_hash = crate::conversation_state::content_hash(&program_bytes);
        let analyzer_hash = crate::conversation_state::content_hash(
            format!(
                "common-runner-v1:{:?}:{}",
                self.evidence_kind, self.runner_id
            )
            .as_bytes(),
        );
        Ok(RegisteredRunnerIdentityV1 {
            analyzer_hash,
            tool_hash: program_hash,
            config_hash: plan_hash,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RegisteredRunnerIdentityV1 {
    analyzer_hash: String,
    tool_hash: String,
    config_hash: String,
}

/// The only static adapter response accepted by the session owner.  The host
/// supplies every identity/scope field; the child can report only the typed
/// state and bounded findings.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StaticAdapterResponseV1 {
    schema_version: u32,
    state: StaticEvidenceStateV1,
    findings: Vec<StaticAdapterFindingV1>,
    error_kind: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StaticAdapterFindingV1 {
    rule_id: String,
    path: Option<String>,
    line: Option<u32>,
    symbol: Option<String>,
    introduced_by_current_diff: bool,
    baseline: bool,
}

fn exact_static_response_shape(value: &serde_json::Value) -> bool {
    const RESPONSE_KEYS: [&str; 4] = ["schema_version", "state", "findings", "error_kind"];
    const FINDING_KEYS: [&str; 6] = [
        "rule_id",
        "path",
        "line",
        "symbol",
        "introduced_by_current_diff",
        "baseline",
    ];
    let exact = |value: &serde_json::Value, keys: &[&str]| {
        value.as_object().is_some_and(|object| {
            object.len() == keys.len() && keys.iter().all(|key| object.contains_key(*key))
        })
    };
    exact(value, &RESPONSE_KEYS)
        && value
            .get("findings")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|findings| findings.iter().all(|finding| exact(finding, &FINDING_KEYS)))
}

/// Canonical v1 output contract.  Capabilities with another schema hash are
/// rejected before process execution.
pub fn static_adapter_output_schema_hash_v1() -> String {
    crate::conversation_state::content_hash(
        b"polaris.static-adapter-output.v1:{schema_version,state,findings:[{rule_id,path,line,symbol,introduced_by_current_diff,baseline}],error_kind}",
    )
}

/// One host-owned static executable.  It carries no shell string, environment
/// override, writable root, retry instruction, or model-provided command.
#[derive(Clone)]
pub struct FixedStaticAdapterV1 {
    pub criterion_id: String,
    pub runner_id: String,
    pub capability: StaticCapabilityV1,
    pub rule_id: String,
    pub program: PathBuf,
    /// Arguments after `program`; the array is fixed by the host.
    pub argv: Vec<String>,
    pub policy: polaris_sandbox::SandboxPolicy,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub maximum_output_bytes: usize,
}

impl FixedStaticAdapterV1 {
    fn invocation(&self) -> StaticAdapterInvocationV1 {
        StaticAdapterInvocationV1 {
            program: self.program.clone(),
            argv: self.argv.clone(),
            timeout: self.timeout,
            maximum_output_bytes: self.maximum_output_bytes,
        }
    }

    fn identity(&self) -> Result<RegisteredRunnerIdentityV1, String> {
        if !opaque(&self.criterion_id)
            || !opaque(&self.runner_id)
            || self.policy.mode() != polaris_sandbox::SandboxMode::ReadOnly
            || self.policy.isolated_boundary().is_none_or(|boundary| {
                boundary.workspace != self.cwd || boundary.environment.is_none()
            })
            || self.capability.output_schema_hash != static_adapter_output_schema_hash_v1()
            || self.capability.rule(&self.rule_id).is_none()
        {
            return Err("fixed static adapter configuration is invalid".into());
        }
        let invocation = self.invocation();
        invocation.validate()?;
        let canonical_program = self
            .program
            .canonicalize()
            .map_err(|error| format!("fixed static analyzer is unavailable: {error}"))?;
        if canonical_program != self.program || !canonical_program.is_file() {
            return Err("fixed static analyzer must be one canonical executable".into());
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if canonical_program
                .metadata()
                .map_err(|error| format!("fixed static analyzer is unavailable: {error}"))?
                .permissions()
                .mode()
                & 0o111
                == 0
            {
                return Err("fixed static analyzer must be executable".into());
            }
        }
        let program = std::fs::read(&canonical_program)
            .map_err(|error| format!("fixed static analyzer is unreadable: {error}"))?;
        let tool_hash = crate::conversation_state::content_hash(&program);
        if tool_hash != self.capability.executable_hash {
            return Err("fixed static analyzer executable identity changed".into());
        }
        let capability_hash = crate::conversation_state::content_hash(
            &serde_json::to_vec(&self.capability)
                .map_err(|_| "fixed static capability cannot be hashed")?,
        );
        let boundary = self
            .policy
            .isolated_boundary()
            .ok_or("fixed static analyzer requires an isolated boundary")?;
        let environment = boundary
            .environment
            .as_ref()
            .ok_or("fixed static analyzer requires a private environment")?;
        let config_hash = crate::conversation_state::content_hash(
            &serde_json::to_vec(&(
                &self.runner_id,
                &self.criterion_id,
                &self.rule_id,
                &self.program,
                &self.argv,
                &self.cwd,
                self.timeout.as_millis(),
                self.maximum_output_bytes,
                &self.capability.output_schema_hash,
                &boundary.readable_roots,
                &environment.home,
                &environment.tmpdir,
            ))
            .map_err(|_| "fixed static adapter configuration cannot be hashed")?,
        );
        Ok(RegisteredRunnerIdentityV1 {
            analyzer_hash: capability_hash,
            tool_hash,
            config_hash,
        })
    }

    fn verification_binding(
        &self,
        observed: &HostObservedVerificationV1,
        identity: &RegisteredRunnerIdentityV1,
    ) -> Result<super::store::VerificationBindingV1, String> {
        let prototype = StaticEvidenceV1 {
            schema_version: super::SCHEMA_VERSION,
            evidence_id: "binding-prototype".into(),
            obligation_id: self.criterion_id.clone(),
            obligation_kind: "criterion".into(),
            analyzer_id: self.capability.analyzer_id.clone(),
            rule_id: self.rule_id.clone(),
            tool_hash: identity.tool_hash.clone(),
            config_hash: identity.config_hash.clone(),
            source_hash: observed.source_hash.clone(),
            diff_hash: observed.diff_hash.clone(),
            scope: AnalysisScopeV1::Package,
            scope_completeness: ScopeCompletenessV1::Complete,
            project_graph_basis: None,
            state: StaticEvidenceStateV1::Unknown,
            findings: Vec::new(),
            error_kind: Some("binding_prototype".into()),
            duration_ms: None,
            cache_hit: Some(false),
        };
        let binding = super::store::VerificationBindingV1 {
            project_graph_hash: observed.project_graph_hash.clone(),
            analyzer_hash: identity.analyzer_hash.clone(),
            tool_hash: identity.tool_hash.clone(),
            source_hash: observed.source_hash.clone(),
            diff_hash: observed.diff_hash.clone(),
            config_hash: identity.config_hash.clone(),
            analysis_request_hash: super::static_analysis_request_hash(&prototype),
        };
        binding.validate().map_err(|error| error.to_string())?;
        Ok(binding)
    }

    fn execute(&self) -> Result<FixedStaticAttemptV1, String> {
        let before = self.identity()?;
        let invocation = self.invocation();
        let deadline = Instant::now() + self.timeout;
        let started = Instant::now();
        let mut controlled = match polaris_sandbox::run_confined_controlled_in_dir(
            &self.policy,
            &self.program,
            &self.argv,
            None,
            Some(&self.cwd),
            || Instant::now() >= deadline,
        ) {
            Ok(outcome) => outcome,
            Err(error) => {
                return Ok(FixedStaticAttemptV1 {
                    invocation,
                    outcome: StaticAdapterOutcomeV1::Failed {
                        reason: format!("sandbox_unavailable:{error}"),
                    },
                    duration_ms: started.elapsed().as_millis().try_into().unwrap_or(u64::MAX),
                    cleanup: None,
                    identity: before,
                });
            }
        };
        let elapsed = started.elapsed();
        // The only cancellation source supplied above is the fixed deadline.
        // Use the owned process result rather than comparing two fresh clock
        // reads: the executor may latch cancellation just before `elapsed`
        // observes the nominal boundary.
        let timed_out = controlled.end != polaris_sandbox::ControlledEnd::Exited;
        let clean_output = !controlled.stdout_truncated
            && !controlled.stderr_truncated
            && controlled.stdout_eof
            && controlled.stderr_eof
            && controlled.problem.is_none();
        let outcome = if timed_out {
            StaticAdapterOutcomeV1::TimedOut
        } else if controlled.end == polaris_sandbox::ControlledEnd::Exited && clean_output {
            match controlled.status.and_then(|status| status.code()) {
                Some(status @ (0 | 1)) => StaticAdapterOutcomeV1::Completed {
                    status,
                    stdout: controlled.stdout.into_bytes(),
                    stderr: controlled.stderr.into_bytes(),
                },
                _ => StaticAdapterOutcomeV1::Failed {
                    reason: "static_analyzer_execution_failed".into(),
                },
            }
        } else {
            StaticAdapterOutcomeV1::Failed {
                reason: "static_analyzer_execution_failed".into(),
            }
        };
        let after = self.identity()?;
        if before.analyzer_hash != after.analyzer_hash
            || before.tool_hash != after.tool_hash
            || before.config_hash != after.config_hash
        {
            return Err("fixed static analyzer identity changed during execution".into());
        }
        Ok(FixedStaticAttemptV1 {
            invocation,
            outcome,
            duration_ms: elapsed.as_millis().try_into().unwrap_or(u64::MAX),
            cleanup: controlled.pending.take(),
            identity: after,
        })
    }
}

struct FixedStaticAttemptV1 {
    invocation: StaticAdapterInvocationV1,
    outcome: StaticAdapterOutcomeV1,
    duration_ms: u64,
    cleanup: Option<polaris_sandbox::PendingCleanup>,
    identity: RegisteredRunnerIdentityV1,
}

struct RegisteredStaticParserV1<'a> {
    adapter: &'a FixedStaticAdapterV1,
    observed: &'a HostObservedVerificationV1,
    identity: &'a RegisteredRunnerIdentityV1,
    duration_ms: u64,
}

impl StaticAdapterParserV1 for RegisteredStaticParserV1<'_> {
    fn parse(&self, output: &StaticAdapterOutcomeV1) -> Result<StaticEvidenceV1, String> {
        let (state, findings, error_kind, response_hash) = match output {
            StaticAdapterOutcomeV1::Completed {
                status,
                stdout,
                stderr,
            } => {
                if !stderr.is_empty() {
                    return Err("static adapter emitted unexpected stderr".into());
                }
                let shape: serde_json::Value = serde_json::from_slice(stdout)
                    .map_err(|_| "static adapter output schema is invalid")?;
                if !exact_static_response_shape(&shape) {
                    return Err("static adapter output schema is incomplete".into());
                }
                let response: StaticAdapterResponseV1 = serde_json::from_slice(stdout)
                    .map_err(|_| "static adapter output schema is invalid")?;
                if response.schema_version != super::SCHEMA_VERSION
                    || response
                        .findings
                        .iter()
                        .any(|finding| finding.rule_id != self.adapter.rule_id)
                    || !matches!(
                        (*status, response.state),
                        (0, StaticEvidenceStateV1::Satisfied)
                            | (1, StaticEvidenceStateV1::Violated)
                    )
                    || (response.state == StaticEvidenceStateV1::Satisfied
                        && (!response.findings.is_empty() || response.error_kind.is_some()))
                    || (response.state == StaticEvidenceStateV1::Violated
                        && (response.findings.is_empty() || response.error_kind.is_some()))
                {
                    return Err("static adapter exit status and output disagree".into());
                }
                let findings = response
                    .findings
                    .into_iter()
                    .map(|finding| FindingV1 {
                        fingerprint: "pending-host-fingerprint".into(),
                        rule_id: finding.rule_id,
                        path: finding.path,
                        line: finding.line,
                        symbol: finding.symbol,
                        introduced_by_current_diff: finding.introduced_by_current_diff,
                        baseline: finding.baseline,
                    })
                    .collect();
                (
                    response.state,
                    findings,
                    response.error_kind,
                    crate::conversation_state::content_hash(stdout),
                )
            }
            StaticAdapterOutcomeV1::TimedOut => (
                StaticEvidenceStateV1::Unavailable,
                Vec::new(),
                Some("timeout".into()),
                crate::conversation_state::content_hash(b"timeout"),
            ),
            StaticAdapterOutcomeV1::Failed { reason } => (
                StaticEvidenceStateV1::Unavailable,
                Vec::new(),
                Some("execution_error".into()),
                crate::conversation_state::content_hash(reason.as_bytes()),
            ),
        };
        Ok(StaticEvidenceV1 {
            schema_version: super::SCHEMA_VERSION,
            evidence_id: format!("static-{}", &response_hash[..24]),
            obligation_id: self.adapter.criterion_id.clone(),
            obligation_kind: "criterion".into(),
            analyzer_id: self.adapter.capability.analyzer_id.clone(),
            rule_id: self.adapter.rule_id.clone(),
            tool_hash: self.identity.tool_hash.clone(),
            config_hash: self.identity.config_hash.clone(),
            source_hash: self.observed.source_hash.clone(),
            diff_hash: self.observed.diff_hash.clone(),
            scope: AnalysisScopeV1::Package,
            scope_completeness: ScopeCompletenessV1::Complete,
            project_graph_basis: None,
            state,
            findings,
            error_kind,
            duration_ms: Some(self.duration_ms),
            cache_hit: Some(false),
        })
    }
}

/// Host-owned preparation facts.  Missing inventory is represented as
/// unavailable rather than as an empty-but-complete inventory.  Optional Jev
/// records are accepted only when their exact question/diff/inventory hashes
/// validate during preparation.
#[derive(Clone)]
pub struct HostVerificationPreparationV1 {
    pub inventory: Option<TestInventoryV1>,
    pub candidate_hints: CandidateHintsV1,
    pub jev_decisions: BTreeMap<String, TestDecisionRecordV1>,
    pub review_refs: Vec<VerificationRecordRefV1>,
    pub all_tests_scope: AllTestsScopeV1,
}

impl Default for HostVerificationPreparationV1 {
    fn default() -> Self {
        Self {
            inventory: None,
            candidate_hints: CandidateHintsV1::default(),
            jev_decisions: BTreeMap::new(),
            review_refs: Vec::new(),
            all_tests_scope: AllTestsScopeV1::Workspace,
        }
    }
}
pub enum RunnerResultV1 {
    Pending,
    Terminal {
        provenance: RunnerProvenanceV1,
        passed: bool,
    },
    OutcomeUnknown,
}

/// Reusable legacy/CLI owner.  It reads only the session's currently active
/// immutable graph, never a model-selected file or artifact.  The initial CLI
/// integration intentionally has no fixed common-runner configuration, so
/// `run` becomes outcome_unknown and can never publish evidence.
pub struct SessionVerificationToolOwnerV1 {
    session: crate::session_store::PersistedSession,
    observer: Arc<dyn VerificationObserverV1>,
    fixed_runners: BTreeMap<String, FixedCommonRunnerV1>,
    existing_test_executor: Option<Arc<dyn RustTestExecutorV1>>,
    static_adapters: BTreeMap<String, FixedStaticAdapterV1>,
    preparation: HostVerificationPreparationV1,
    static_catalog: StaticCapabilityCatalogV1,
    checkpoints: Mutex<BTreeMap<String, SavedVerificationCheckpointV1>>,
    attempts: Mutex<BTreeMap<String, Vec<CriterionAttemptV1>>>,
}

struct CriterionAttemptV1 {
    criterion_id: String,
    evidence_kind: EvidenceKindV1,
    outcome: CriterionAttemptOutcomeV1,
}

#[expect(
    clippy::large_enum_variant,
    reason = "attempt outcomes are private short-lived records and preserve direct ownership"
)]
enum CriterionAttemptOutcomeV1 {
    Common(crate::common_tests::RunOutcome),
    Existing {
        plan: RustTestExecutionPlanV1,
        outcome: RustTestExecutionOutcomeV1,
        executor: RustTestExecutorIdentityV1,
    },
    Static {
        reference: VerificationRecordRefV1,
        verification_binding: super::store::VerificationBindingV1,
        identity: RegisteredRunnerIdentityV1,
        cleanup: Option<polaris_sandbox::PendingCleanup>,
    },
}

impl SessionVerificationToolOwnerV1 {
    pub fn new(
        session: crate::session_store::PersistedSession,
        observer: Arc<dyn VerificationObserverV1>,
        runners: Vec<FixedCommonRunnerV1>,
    ) -> Result<Self, String> {
        let mut fixed_runners = BTreeMap::new();
        for runner in runners {
            if !opaque(&runner.criterion_id)
                || !opaque(&runner.runner_id)
                || !matches!(
                    runner.evidence_kind,
                    EvidenceKindV1::CommonCase | EvidenceKindV1::NewRegression
                )
                || runner.plan.check().is_err()
                || runner.registered_identity().is_err()
                || fixed_runners
                    .insert(runner.criterion_id.clone(), runner)
                    .is_some()
            {
                return Err("fixed common runner configuration is invalid".into());
            }
        }
        let owner = Self {
            session,
            observer,
            fixed_runners,
            existing_test_executor: None,
            static_adapters: BTreeMap::new(),
            preparation: HostVerificationPreparationV1::default(),
            static_catalog: StaticCapabilityCatalogV1::default(),
            checkpoints: Mutex::new(BTreeMap::new()),
            attempts: Mutex::new(BTreeMap::new()),
        };
        owner.reload_checkpoint()?;
        Ok(owner)
    }

    pub fn with_preparation(
        mut self,
        preparation: HostVerificationPreparationV1,
    ) -> Result<Self, String> {
        if let Some(inventory) = &preparation.inventory {
            inventory.validate().map_err(|error| error.to_string())?;
        }
        for reference in &preparation.review_refs {
            reference.validate().map_err(|error| error.to_string())?;
        }
        self.preparation = preparation;
        Ok(self)
    }

    /// Inject a host-owned exact-test executor.  The executor is optional so
    /// existing callers remain fail-closed: criteria that require existing
    /// tests become unavailable rather than falling back to a model command.
    pub fn with_existing_test_executor(
        mut self,
        executor: Arc<dyn RustTestExecutorV1>,
    ) -> Result<Self, String> {
        executor.identity()?.validate()?;
        self.existing_test_executor = Some(executor);
        Ok(self)
    }

    /// Register executable adapters and their capability contracts together.
    /// Supplying only a catalog cannot make a criterion runnable.
    pub fn with_static_adapters(
        mut self,
        adapters: Vec<FixedStaticAdapterV1>,
    ) -> Result<Self, String> {
        let mut registered = BTreeMap::new();
        let mut catalog = StaticCapabilityCatalogV1::default();
        for adapter in adapters {
            adapter.identity()?;
            if registered
                .insert(adapter.criterion_id.clone(), adapter.clone())
                .is_some()
            {
                return Err("duplicate static adapter criterion".into());
            }
            catalog
                .register(adapter.capability.clone())
                .map_err(|error| error.to_string())?;
        }
        self.static_adapters = registered;
        self.static_catalog = catalog;
        Ok(self)
    }

    fn reload_checkpoint(&self) -> Result<(), String> {
        let snapshot = self.session.snapshot().map_err(|error| error.to_string())?;
        let Some(mut checkpoint) = snapshot.state.verification_checkpoint else {
            return Ok(());
        };
        if checkpoint.stage == VerificationCheckpointStageV1::Running {
            checkpoint.mark_crash_recovered();
            self.session
                .publish_verification_checkpoint(snapshot.state.generation, checkpoint.clone())
                .map_err(|error| error.to_string())?;
        }
        checkpoint.validate()?;
        self.checkpoints
            .lock()
            .map_err(|_| "verification owner lock poisoned")?
            .insert(checkpoint.checkpoint_id.clone(), checkpoint);
        Ok(())
    }

    fn verification_binding(
        snapshot: &crate::conversation_state::ConversationSnapshot,
        observed: HostObservedVerificationV1,
    ) -> Result<crate::workflow::EvidenceBindingV1, String> {
        let state = &snapshot.state;
        let (task_id, task_definition_hash) = if let Some(task) = &state.workflow.active_task {
            state.workflow_artifacts.validate_active_task(task)?;
            (task.task_id.clone(), task.definition_hash.clone())
        } else {
            let contract = state
                .workflow
                .active_change_contract
                .as_ref()
                .ok_or("active verification artifact is absent")?;
            state
                .workflow_artifacts
                .approved_routine_contract(contract)?;
            (contract.id.clone(), contract.content_hash.clone())
        };
        let graph = state.workflow_artifacts.basis_hash()?;
        observed.validate()?;
        Ok(crate::workflow::EvidenceBindingV1 {
            task_id,
            task_definition_hash,
            subject_hash: observed.subject_hash,
            source_hash: observed.source_hash,
            diff_hash: observed.diff_hash,
            artifact_graph_basis_hash: graph,
        })
    }

    fn selected_runners(
        &self,
        needs: &[super::VerificationNeedV1],
    ) -> Result<Vec<FixedCommonRunnerV1>, String> {
        let mut selected = BTreeMap::<String, FixedCommonRunnerV1>::new();
        for need in needs {
            if self.uses_existing_test_executor(need) || self.uses_static_adapter(need) {
                continue;
            }
            let runner = self.fixed_runners.get(&need.criterion_id).ok_or_else(|| {
                format!(
                    "verification unavailable: no host-fixed runner is registered for {}",
                    need.criterion_id
                )
            })?;
            if !need.evidence_kinds.contains(&runner.evidence_kind) {
                return Err(format!(
                    "verification unavailable: registered runner evidence kind does not satisfy {}",
                    need.criterion_id
                ));
            }
            if let Some(previous) = selected.insert(need.criterion_id.clone(), runner.clone())
                && (previous.runner_id != runner.runner_id
                    || previous.evidence_kind != runner.evidence_kind)
            {
                return Err("verification criterion resolves to inconsistent runners".into());
            }
        }
        Ok(selected.into_values().collect())
    }

    fn uses_existing_test_executor(&self, need: &super::VerificationNeedV1) -> bool {
        self.existing_test_executor.is_some()
            && need.evidence_kinds.contains(&EvidenceKindV1::ExistingTest)
    }

    fn uses_static_adapter(&self, need: &super::VerificationNeedV1) -> bool {
        self.static_adapters.contains_key(&need.criterion_id)
            && need.evidence_kinds.contains(&EvidenceKindV1::Static)
    }

    fn selected_criterion_runs(
        &self,
        needs: &[super::VerificationNeedV1],
        runners: &[FixedCommonRunnerV1],
    ) -> Result<Vec<CriterionRunV1>, String> {
        let common = runners
            .iter()
            .map(|runner| (runner.criterion_id.as_str(), runner))
            .collect::<BTreeMap<_, _>>();
        let executor = self
            .existing_test_executor
            .as_ref()
            .map(|executor| executor.identity())
            .transpose()?;
        needs
            .iter()
            .map(|need| {
                if self.uses_static_adapter(need) {
                    let adapter = self
                        .static_adapters
                        .get(&need.criterion_id)
                        .ok_or("static adapter is unavailable")?;
                    Ok(CriterionRunV1 {
                        criterion_id: need.criterion_id.clone(),
                        evidence_kind: EvidenceKindV1::Static,
                        runner_id: adapter.runner_id.clone(),
                        provenance: None,
                    })
                } else if self.uses_existing_test_executor(need) {
                    let executor = executor
                        .as_ref()
                        .ok_or("existing-test executor is unavailable")?;
                    Ok(CriterionRunV1 {
                        criterion_id: need.criterion_id.clone(),
                        evidence_kind: EvidenceKindV1::ExistingTest,
                        runner_id: executor.runner_id.clone(),
                        provenance: None,
                    })
                } else {
                    let runner = common.get(need.criterion_id.as_str()).ok_or_else(|| {
                        format!(
                            "verification unavailable: no host-fixed runner is registered for {}",
                            need.criterion_id
                        )
                    })?;
                    Ok(CriterionRunV1 {
                        criterion_id: runner.criterion_id.clone(),
                        evidence_kind: runner.evidence_kind,
                        runner_id: runner.runner_id.clone(),
                        provenance: None,
                    })
                }
            })
            .collect()
    }

    fn store_binding(
        &self,
        binding: &crate::workflow::EvidenceBindingV1,
        observed: &HostObservedVerificationV1,
        needs: &[super::VerificationNeedV1],
        runners: &[FixedCommonRunnerV1],
    ) -> Result<super::store::VerificationBindingV1, String> {
        let mut registered = runners
            .iter()
            .map(|runner| {
                runner.registered_identity().map(|identity| {
                    (
                        runner.criterion_id.clone(),
                        runner.runner_id.clone(),
                        runner.evidence_kind,
                        identity,
                    )
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(executor) = &self.existing_test_executor {
            let identity = executor.identity()?;
            identity.validate()?;
            for need in needs
                .iter()
                .filter(|need| self.uses_existing_test_executor(need))
            {
                registered.push((
                    need.criterion_id.clone(),
                    identity.runner_id.clone(),
                    EvidenceKindV1::ExistingTest,
                    RegisteredRunnerIdentityV1 {
                        analyzer_hash: crate::conversation_state::content_hash(
                            format!("existing-test-executor-v1:{}", identity.runner_id).as_bytes(),
                        ),
                        tool_hash: identity.program_hash.clone(),
                        config_hash: identity.config_hash.clone(),
                    },
                ));
            }
        }
        for need in needs.iter().filter(|need| self.uses_static_adapter(need)) {
            let adapter = self
                .static_adapters
                .get(&need.criterion_id)
                .ok_or("static adapter is unavailable")?;
            registered.push((
                need.criterion_id.clone(),
                adapter.runner_id.clone(),
                EvidenceKindV1::Static,
                adapter.identity()?,
            ));
        }
        if registered.is_empty() {
            return Err("verification unavailable: no host-fixed runner is registered".into());
        }
        registered.sort_by(|left, right| left.0.cmp(&right.0));
        let analyzer_hash = crate::conversation_state::content_hash(
            &serde_json::to_vec(
                &registered
                    .iter()
                    .map(|(criterion, runner, kind, identity)| {
                        (criterion, runner, kind, &identity.analyzer_hash)
                    })
                    .collect::<Vec<_>>(),
            )
            .map_err(|_| "registered runner analyzers cannot be hashed")?,
        );
        let tool_hash = crate::conversation_state::content_hash(
            &serde_json::to_vec(
                &registered
                    .iter()
                    .map(|(criterion, _, _, identity)| (criterion, &identity.tool_hash))
                    .collect::<Vec<_>>(),
            )
            .map_err(|_| "registered runner tools cannot be hashed")?,
        );
        let config_hash = crate::conversation_state::content_hash(
            &serde_json::to_vec(
                &registered
                    .iter()
                    .map(|(criterion, _, _, identity)| (criterion, &identity.config_hash))
                    .collect::<Vec<_>>(),
            )
            .map_err(|_| "registered runner configs cannot be hashed")?,
        );
        let analysis_request_hash = crate::conversation_state::content_hash(
            &serde_json::to_vec(&(
                &binding.task_definition_hash,
                registered
                    .iter()
                    .map(|(criterion, runner, kind, _)| (criterion, runner, kind))
                    .collect::<Vec<_>>(),
            ))
            .map_err(|_| "verification request cannot be hashed")?,
        );
        let value = super::store::VerificationBindingV1 {
            project_graph_hash: observed.project_graph_hash.clone(),
            analyzer_hash,
            tool_hash,
            source_hash: binding.source_hash.clone(),
            diff_hash: binding.diff_hash.clone(),
            config_hash,
            analysis_request_hash,
        };
        value.validate().map_err(|error| error.to_string())?;
        Ok(value)
    }

    #[expect(
        clippy::type_complexity,
        reason = "the tuple is an internal atomic snapshot assembled together"
    )]
    fn current_bindings(
        &self,
    ) -> Result<
        (
            crate::conversation_state::ConversationSnapshot,
            HostObservedVerificationV1,
            crate::workflow::EvidenceBindingV1,
            super::store::VerificationBindingV1,
            Vec<super::VerificationNeedV1>,
            Vec<FixedCommonRunnerV1>,
        ),
        String,
    > {
        let snapshot = self.session.snapshot().map_err(|e| e.to_string())?;
        let observed = self.observer.observe()?;
        let binding = Self::verification_binding(&snapshot, observed.clone())?;
        let needs = self.derive_needs(&snapshot)?;
        let runners = self.selected_runners(&needs)?;
        let store_binding = self.store_binding(&binding, &observed, &needs, &runners)?;
        Ok((snapshot, observed, binding, store_binding, needs, runners))
    }

    fn sealed_bundle_for_checkpoint(
        &self,
        checkpoint: &SavedVerificationCheckpointV1,
    ) -> Result<
        (
            crate::conversation_state::ConversationSnapshot,
            super::store::VerificationBindingV1,
            VerifiedConvergenceBundleV1,
        ),
        String,
    > {
        let (snapshot, _observed, binding, verification_binding, needs, runners) =
            self.current_bindings()?;
        if binding != checkpoint.evidence_binding
            || checkpoint.evidence_refs.len() != checkpoint.criterion_runs.len()
            || validate_exact_runs(
                &checkpoint.criterion_runs,
                &needs,
                &runners,
                self.existing_test_executor.as_deref(),
                &self.static_adapters,
            )
            .is_err()
        {
            return Err("terminal verification evidence is stale or incomplete".into());
        }
        let store = super::store::VerificationStore::open(
            &self.session.verification_store_root(),
            &snapshot.state.project_id,
        )
        .map_err(|e| e.to_string())?;
        validate_exact_needs(&store, &verification_binding, &checkpoint.need_refs, &needs)?;
        let required_kinds = required_kinds_by_criterion(&needs)?;
        let run_kinds = checkpoint
            .criterion_runs
            .iter()
            .map(|run| (run.criterion_id.clone(), run.evidence_kind))
            .collect::<BTreeMap<_, _>>();
        let mut evidence = Vec::new();
        for reference in &checkpoint.evidence_refs {
            let record = store
                .load_final_evidence_record(reference, &verification_binding, &binding)
                .map_err(|error| error.to_string())?;
            let kind = run_kinds
                .get(&record.criterion_id)
                .ok_or("final evidence criterion is not required")?;
            if !required_kinds
                .get(&record.criterion_id)
                .is_some_and(|required| required.contains(kind))
            {
                return Err("final evidence kind does not satisfy the current criterion".into());
            }
            evidence.push(record);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "system clock is before unix epoch")?
            .as_secs();
        let owner = super::owner::VerificationOwnerV1::new(&store, &self.static_catalog);
        let mut bundle = owner.bundle_from_final_evidence_records(&binding, &evidence, now)?;
        for reference in &checkpoint.review_refs {
            bundle = owner.add_review_reference(bundle, &verification_binding, reference)?;
        }
        Ok((snapshot, verification_binding, bundle))
    }

    /// Publish exactly one host-authored review for a sealed verification.
    /// Bindings, record ID, storage references and phase transitions are all
    /// derived here; no conversational tool receives this operation.
    pub fn publish_host_review(
        &self,
        expected_workflow_revision: u64,
        checkpoint_id: &str,
        reviewer_id: &str,
        findings: Vec<crate::workflow::ReviewFindingV1>,
    ) -> Result<
        (
            crate::workflow::SessionWorkflow,
            SavedVerificationCheckpointV1,
        ),
        String,
    > {
        if !opaque(checkpoint_id) || !opaque(reviewer_id) {
            return Err("review authority identity is invalid".into());
        }
        if findings
            .iter()
            .any(|finding| finding.disposition == crate::workflow::ReviewFindingDispositionV1::Open)
        {
            return Err("open review findings cannot be published".into());
        }
        let durable = self.session.snapshot().map_err(|error| error.to_string())?;
        if durable.state.workflow.revision != expected_workflow_revision
            || !matches!(
                durable.state.workflow.phase,
                crate::workflow::Phase::Implement | crate::workflow::Phase::Review
            )
        {
            return Err("review authority is stale for the current workflow".into());
        }
        let checkpoint = durable
            .state
            .verification_checkpoint
            .clone()
            .ok_or("published verification checkpoint is absent")?;
        let loaded = self
            .checkpoints
            .lock()
            .map_err(|_| "verification owner lock poisoned")?
            .get(checkpoint_id)
            .cloned()
            .ok_or("verification checkpoint is unknown")?;
        if checkpoint != loaded
            || checkpoint.checkpoint_id != checkpoint_id
            || checkpoint.stage != VerificationCheckpointStageV1::Published
            || !checkpoint.review_refs.is_empty()
        {
            return Err("published verification checkpoint changed before review".into());
        }
        let (snapshot, verification_binding, bundle) =
            self.sealed_bundle_for_checkpoint(&checkpoint)?;
        if snapshot.state.generation != durable.state.generation
            || snapshot.state.workflow.revision != expected_workflow_revision
        {
            return Err("review authority changed during binding validation".into());
        }
        let record_bytes = serde_json::to_vec(&(
            "polaris.host-review.v1",
            reviewer_id,
            checkpoint_id,
            &checkpoint.evidence_binding,
            &findings,
        ))
        .map_err(|_| "review record cannot be hashed")?;
        let review = crate::workflow::ReviewRecordV1 {
            schema_version: crate::workflow::EVIDENCE_SCHEMA_VERSION,
            record_id: format!(
                "review-{}",
                crate::conversation_state::content_hash(&record_bytes)
            ),
            binding: checkpoint.evidence_binding.clone(),
            findings,
        };
        review.validate()?;
        let store = super::store::VerificationStore::open(
            &self.session.verification_store_root(),
            &snapshot.state.project_id,
        )
        .map_err(|error| error.to_string())?;
        let reference = store
            .put_review_record(&verification_binding, &checkpoint.evidence_binding, &review)
            .map_err(|error| error.to_string())?;
        let owner = super::owner::VerificationOwnerV1::new(&store, &self.static_catalog);
        let bundle = owner.add_review_reference(bundle, &verification_binding, &reference)?;
        let (workflow, published) = self
            .session
            .publish_review_convergence(
                snapshot.state.generation,
                expected_workflow_revision,
                checkpoint_id.to_owned(),
                reference,
                bundle,
            )
            .map_err(|error| error.to_string())?;
        self.checkpoints
            .lock()
            .map_err(|_| "verification owner lock poisoned")?
            .insert(checkpoint_id.to_owned(), published.clone());
        Ok((workflow, published))
    }

    fn derive_needs(
        &self,
        snapshot: &crate::conversation_state::ConversationSnapshot,
    ) -> Result<Vec<super::VerificationNeedV1>, String> {
        if let Some(contract_ref) = &snapshot.state.workflow.active_change_contract {
            let contract = self
                .session
                .load_change_contract(contract_ref)
                .map_err(|error| error.to_string())?;
            return super::pipeline::derive_verification_needs(
                super::pipeline::ActiveVerificationArtifactsV1::RoutineContract {
                    active_contract: contract_ref,
                    contract: &contract,
                },
            );
        }
        let task = snapshot
            .state
            .workflow
            .active_task
            .as_ref()
            .ok_or("active task is absent")?;
        let feature_ref = snapshot
            .state
            .workflow
            .active_feature
            .as_ref()
            .ok_or("active feature is absent")?;
        let feature = self
            .session
            .load_feature_spec(feature_ref)
            .map_err(|error| error.to_string())?;
        let graph = self
            .session
            .load_task_graph(&task.task_graph)
            .map_err(|error| error.to_string())?;
        super::pipeline::derive_verification_needs(
            super::pipeline::ActiveVerificationArtifactsV1::PlannedTask {
                active_feature: feature_ref,
                active_task: task,
                feature: &feature,
                task_graph: &graph,
            },
        )
    }
}

impl VerificationToolOwnerV1 for SessionVerificationToolOwnerV1 {
    fn prepare(&self) -> Result<SavedVerificationCheckpointV1, String> {
        let (snapshot, _observed, binding, verification_binding, needs, runners) =
            self.current_bindings()?;
        let data_root = self.session.verification_store_root();
        let store = super::store::VerificationStore::open(&data_root, &snapshot.state.project_id)
            .map_err(|e| e.to_string())?;
        let active_artifacts =
            if let Some(contract_ref) = &snapshot.state.workflow.active_change_contract {
                vec![contract_ref.clone()]
            } else {
                let task = snapshot
                    .state
                    .workflow
                    .active_task
                    .as_ref()
                    .ok_or("active task is absent")?;
                let feature_ref = snapshot
                    .state
                    .workflow
                    .active_feature
                    .as_ref()
                    .ok_or("active feature is absent")?;
                vec![feature_ref.clone(), task.task_graph.clone()]
            };
        let need_refs = needs
            .iter()
            .map(|need| {
                store
                    .put_need(&verification_binding, need)
                    .map_err(|e| e.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut inventory_ref = None;
        let mut candidate_ref = None;
        let mut selections = BTreeMap::new();
        let mut test_execution_plans = BTreeMap::new();
        let mut jev_decision_refs = Vec::new();
        if let Some(inventory) = &self.preparation.inventory {
            let (saved_inventory, candidates, saved_candidates) =
                super::pipeline::persist_inventory_and_candidates(
                    &store,
                    &verification_binding,
                    inventory,
                    &self.preparation.candidate_hints,
                )?;
            inventory_ref = Some(saved_inventory);
            candidate_ref = Some(saved_candidates);
            let mut unique = BTreeMap::new();
            for need in &needs {
                unique.entry(need.criterion_id.clone()).or_insert(need);
            }
            for (criterion_id, need) in unique {
                let question_hash =
                    super::pipeline::test_selection_question_hash(need, inventory, &candidates)
                        .unwrap_or_default();
                let decision_ref =
                    self.preparation
                        .jev_decisions
                        .get(&criterion_id)
                        .and_then(|decision| {
                            store
                                .put_jev_decision(
                                    &verification_binding,
                                    inventory,
                                    &candidates,
                                    decision,
                                )
                                .ok()
                        });
                if let Some(reference) = &decision_ref {
                    jev_decision_refs.push(reference.clone());
                }
                let selection = store.select_tests_offline(
                    decision_ref.as_ref(),
                    &verification_binding,
                    inventory,
                    &candidates,
                    &question_hash,
                    super::SCORE_CUTOFF,
                    self.preparation.all_tests_scope,
                );
                let package_for_all = package_for_all_selection(inventory);
                let execution_plan = RustTestExecutionPlanV1::build(
                    &selection,
                    inventory,
                    package_for_all,
                )
                .map_err(|_| {
                    "verification selection cannot be projected to a fixed Cargo execution plan"
                })?;
                test_execution_plans.insert(criterion_id.clone(), execution_plan);
                selections.insert(criterion_id, selection);
            }
        } else {
            for need in &needs {
                selections
                    .entry(need.criterion_id.clone())
                    .or_insert(TestSelectionV1::All {
                        scope: self.preparation.all_tests_scope,
                        reason: AllReasonV1::IncompleteInventory,
                    });
            }
        }
        let stable = crate::conversation_state::content_hash(
            format!(
                "{}:{}:{}:{}:{}",
                snapshot.state.session_id,
                snapshot.state.generation,
                snapshot.state.workflow.revision,
                binding.artifact_graph_basis_hash,
                verification_binding.analysis_request_hash,
            )
            .as_bytes(),
        );
        let mut checkpoint = SavedVerificationCheckpointV1::new(
            format!("verify-{}", &stable[..24]),
            format!("verify-idem-{}", &stable[..24]),
            snapshot.state.workflow.revision,
            active_artifacts,
            binding,
        )?;
        checkpoint.need_refs = need_refs;
        checkpoint.all_tests_scope = inventory_ref
            .as_ref()
            .map(|_| self.preparation.all_tests_scope);
        checkpoint.inventory_ref = inventory_ref;
        checkpoint.candidate_ref = candidate_ref;
        checkpoint.selection = selections.values().next().cloned();
        checkpoint.selections = selections;
        checkpoint.test_execution_plans = test_execution_plans;
        checkpoint.jev_decision_ref = jev_decision_refs.first().cloned();
        checkpoint.jev_decision_refs = jev_decision_refs;
        checkpoint.review_refs = self.preparation.review_refs.clone();
        checkpoint.criterion_runs = self.selected_criterion_runs(&needs, &runners)?;
        checkpoint.validate()?;
        self.checkpoints
            .lock()
            .map_err(|_| "verification owner lock poisoned")?
            .insert(checkpoint.checkpoint_id.clone(), checkpoint.clone());
        Ok(checkpoint)
    }

    fn begin_run(&self, preparation_id: &str) -> Result<SavedVerificationCheckpointV1, String> {
        let original = self
            .checkpoints
            .lock()
            .map_err(|_| "verification owner lock poisoned")?
            .get(preparation_id)
            .cloned()
            .ok_or("verification preparation is unknown")?;
        if original.stage == VerificationCheckpointStageV1::Running {
            return Ok(original);
        }
        if original.stage != VerificationCheckpointStageV1::Prepared {
            return Err("verification preparation is not runnable".into());
        }
        let (snapshot, observed, binding, verification_binding, needs, runners) =
            self.current_bindings()?;
        if binding != original.evidence_binding {
            return Err("verification preparation is stale".into());
        }
        let store = super::store::VerificationStore::open(
            &self.session.verification_store_root(),
            &snapshot.state.project_id,
        )
        .map_err(|error| error.to_string())?;
        validate_exact_needs(&store, &verification_binding, &original.need_refs, &needs)?;
        validate_persisted_test_execution_plans(&original, &store, &verification_binding, &needs)?;
        validate_exact_runs(
            &original.criterion_runs,
            &needs,
            &runners,
            self.existing_test_executor.as_deref(),
            &self.static_adapters,
        )?;

        let mut running = original.clone();
        running.transition(VerificationCheckpointStageV1::Running)?;
        {
            let mut checkpoints = self
                .checkpoints
                .lock()
                .map_err(|_| "verification owner lock poisoned")?;
            let current = checkpoints
                .get(preparation_id)
                .ok_or("verification preparation is unknown")?;
            if current != &original {
                return Err("verification preparation changed before execution".into());
            }
            checkpoints.insert(preparation_id.into(), running.clone());
        }

        // This durable CAS is deliberately before the first process spawn.
        // A crash on the following line leaves Running, which constructor
        // recovery turns into OutcomeUnknown and never replays.
        if let Err(error) = self
            .session
            .publish_verification_checkpoint(snapshot.state.generation, running.clone())
        {
            self.checkpoints
                .lock()
                .map_err(|_| "verification owner lock poisoned")?
                .insert(preparation_id.into(), original);
            return Err(error.to_string());
        }

        let common = runners
            .into_iter()
            .map(|runner| (runner.criterion_id.clone(), runner))
            .collect::<BTreeMap<_, _>>();
        let static_owner = super::owner::VerificationOwnerV1::new(&store, &self.static_catalog);
        let attempts = original
            .criterion_runs
            .iter()
            .map(|run| match run.evidence_kind {
                EvidenceKindV1::Static => {
                    let adapter = self
                        .static_adapters
                        .get(&run.criterion_id)
                        .ok_or("static adapter is not registered")?;
                    let mut attempt = adapter.execute()?;
                    let static_binding =
                        adapter.verification_binding(&observed, &attempt.identity)?;
                    let parser = RegisteredStaticParserV1 {
                        adapter,
                        observed: &observed,
                        identity: &attempt.identity,
                        duration_ms: attempt.duration_ms,
                    };
                    let reference = static_owner.record_registered_static_outcome(
                        &static_binding,
                        &attempt.invocation,
                        &attempt.outcome,
                        &parser,
                    )?;
                    Ok(CriterionAttemptV1 {
                        criterion_id: run.criterion_id.clone(),
                        evidence_kind: run.evidence_kind,
                        outcome: CriterionAttemptOutcomeV1::Static {
                            reference,
                            verification_binding: static_binding,
                            identity: attempt.identity,
                            cleanup: attempt.cleanup.take(),
                        },
                    })
                }
                EvidenceKindV1::ExistingTest => {
                    let plan = original
                        .test_execution_plans
                        .get(&run.criterion_id)
                        .cloned()
                        .ok_or("persisted existing-test execution plan is absent")?;
                    let executor = self
                        .existing_test_executor
                        .as_ref()
                        .ok_or("existing-test executor is not registered")?;
                    let identity = executor.identity()?;
                    Ok(CriterionAttemptV1 {
                        criterion_id: run.criterion_id.clone(),
                        evidence_kind: run.evidence_kind,
                        outcome: CriterionAttemptOutcomeV1::Existing {
                            outcome: executor.execute(&plan),
                            plan,
                            executor: identity,
                        },
                    })
                }
                _ => {
                    let runner = common
                        .get(&run.criterion_id)
                        .ok_or("fixed common runner is absent")?;
                    Ok(CriterionAttemptV1 {
                        criterion_id: run.criterion_id.clone(),
                        evidence_kind: run.evidence_kind,
                        outcome: CriterionAttemptOutcomeV1::Common(runner.run()),
                    })
                }
            })
            .collect::<Result<Vec<_>, String>>()?;
        self.attempts
            .lock()
            .map_err(|_| "verification attempt lock poisoned")?
            .insert(preparation_id.into(), attempts);
        Ok(running)
    }

    fn poll_run(&self, preparation_id: &str) -> Result<SavedVerificationCheckpointV1, String> {
        let mut attempts_guard = self
            .attempts
            .lock()
            .map_err(|_| "verification attempt lock poisoned")?;
        let attempts = attempts_guard
            .get_mut(preparation_id)
            .ok_or("verification attempt is absent")?;
        for attempt in attempts.iter_mut() {
            match &mut attempt.outcome {
                CriterionAttemptOutcomeV1::Common(outcome) => {
                    if let Some(cleanup) = outcome.cleanup_mut() {
                        if !cleanup.try_stop_reap_and_cleanup()? {
                            return Ok(self
                                .checkpoints
                                .lock()
                                .map_err(|_| "verification checkpoint lock poisoned")?
                                .get(preparation_id)
                                .cloned()
                                .ok_or("verification checkpoint is absent")?);
                        }
                        let _ = outcome.take_cleanup();
                    }
                }
                CriterionAttemptOutcomeV1::Static { cleanup, .. } => {
                    if let Some(owner) = cleanup.as_mut() {
                        match owner.try_stop_and_reap() {
                            Ok(Some(_)) => *cleanup = None,
                            Ok(None) if owner.child_id().is_none() => *cleanup = None,
                            Ok(None) => {
                                return Ok(self
                                    .checkpoints
                                    .lock()
                                    .map_err(|_| "verification checkpoint lock poisoned")?
                                    .get(preparation_id)
                                    .cloned()
                                    .ok_or("verification checkpoint is absent")?);
                            }
                            Err(error) => {
                                return Err(format!("static analyzer cleanup failed: {error}"));
                            }
                        }
                    }
                }
                CriterionAttemptOutcomeV1::Existing { .. } => {}
            }
        }
        #[expect(
            clippy::large_enum_variant,
            reason = "local observation enum is immediately consumed and avoids duplicate allocations"
        )]
        enum ObservedAttemptV1 {
            Common {
                criterion_id: String,
                evidence_kind: EvidenceKindV1,
                report: crate::common_tests::RunReport,
                cleanup_pending: bool,
            },
            Existing {
                criterion_id: String,
                plan: RustTestExecutionPlanV1,
                outcome: RustTestExecutionOutcomeV1,
                executor: RustTestExecutorIdentityV1,
            },
            Static {
                criterion_id: String,
                reference: VerificationRecordRefV1,
                verification_binding: super::store::VerificationBindingV1,
                identity: RegisteredRunnerIdentityV1,
                cleanup_pending: bool,
            },
        }
        let observed_attempts = attempts
            .iter()
            .map(|attempt| match &attempt.outcome {
                CriterionAttemptOutcomeV1::Common(outcome) => ObservedAttemptV1::Common {
                    criterion_id: attempt.criterion_id.clone(),
                    evidence_kind: attempt.evidence_kind,
                    report: outcome.report.clone(),
                    cleanup_pending: outcome.has_pending_cleanup(),
                },
                CriterionAttemptOutcomeV1::Existing {
                    plan,
                    outcome,
                    executor,
                } => ObservedAttemptV1::Existing {
                    criterion_id: attempt.criterion_id.clone(),
                    plan: plan.clone(),
                    outcome: outcome.clone(),
                    executor: executor.clone(),
                },
                CriterionAttemptOutcomeV1::Static {
                    reference,
                    verification_binding,
                    identity,
                    cleanup,
                    ..
                } => ObservedAttemptV1::Static {
                    criterion_id: attempt.criterion_id.clone(),
                    reference: reference.clone(),
                    verification_binding: verification_binding.clone(),
                    identity: identity.clone(),
                    cleanup_pending: cleanup.is_some(),
                },
            })
            .collect::<Vec<_>>();
        drop(attempts_guard);
        let mut checkpoints = self
            .checkpoints
            .lock()
            .map_err(|_| "verification checkpoint lock poisoned")?;
        let checkpoint = checkpoints
            .get_mut(preparation_id)
            .ok_or("verification checkpoint is absent")?;
        let Ok((snapshot, _observed, current, verification_binding, needs, runners)) =
            self.current_bindings()
        else {
            checkpoint.stage = VerificationCheckpointStageV1::OutcomeUnknown;
            return Ok(checkpoint.clone());
        };
        if current != checkpoint.evidence_binding {
            checkpoint.stage = VerificationCheckpointStageV1::OutcomeUnknown;
            return Ok(checkpoint.clone());
        }
        if validate_exact_runs(
            &checkpoint.criterion_runs,
            &needs,
            &runners,
            self.existing_test_executor.as_deref(),
            &self.static_adapters,
        )
        .is_err()
        {
            checkpoint.stage = VerificationCheckpointStageV1::OutcomeUnknown;
            return Ok(checkpoint.clone());
        }
        let store = super::store::VerificationStore::open(
            &self.session.verification_store_root(),
            &snapshot.state.project_id,
        )
        .map_err(|error| error.to_string())?;
        validate_exact_needs(&store, &verification_binding, &checkpoint.need_refs, &needs)?;
        let owner = super::owner::VerificationOwnerV1::new(&store, &self.static_catalog);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "system clock is before unix epoch")?
            .as_secs();
        let required_kinds = required_kinds_by_criterion(&needs)?;
        let mut evidence_refs = Vec::new();
        let mut provenance = BTreeMap::<String, RunnerProvenanceV1>::new();
        for observed in observed_attempts {
            match observed {
                ObservedAttemptV1::Static {
                    criterion_id,
                    reference,
                    verification_binding: static_binding,
                    identity,
                    cleanup_pending,
                } => {
                    if cleanup_pending
                        || !required_kinds
                            .get(&criterion_id)
                            .is_some_and(|kinds| kinds.contains(&EvidenceKindV1::Static))
                    {
                        checkpoint.stage = VerificationCheckpointStageV1::OutcomeUnknown;
                        return Ok(checkpoint.clone());
                    }
                    let raw = store
                        .load_registered_static_evidence(
                            &reference,
                            &static_binding,
                            &self.static_catalog,
                        )
                        .map_err(|error| error.to_string())?;
                    checkpoint.static_evidence_refs.push(reference.clone());
                    let capability = self
                        .static_catalog
                        .resolve(&raw.analyzer_id)
                        .ok_or("static analyzer is no longer registered")?;
                    if raw.scope_completeness != ScopeCompletenessV1::Complete
                        || raw.effective_state(Some(capability)) != StaticEvidenceStateV1::Satisfied
                    {
                        checkpoint.stage = VerificationCheckpointStageV1::OutcomeUnknown;
                        return Ok(checkpoint.clone());
                    }
                    let evidence = owner.static_evidence(
                        &current,
                        &criterion_id,
                        &reference,
                        &static_binding,
                        now,
                    )?;
                    evidence_refs.push(
                        store
                            .put_final_evidence_record(&verification_binding, evidence.record())
                            .map_err(|error| error.to_string())?,
                    );
                    provenance.insert(
                        criterion_id,
                        RunnerProvenanceV1 {
                            plan_hash: identity.config_hash,
                            program_hash: identity.tool_hash,
                            report_hash: reference.content_hash,
                            cleanup_complete: true,
                            execution_available: true,
                        },
                    );
                }
                ObservedAttemptV1::Common {
                    criterion_id,
                    evidence_kind,
                    report,
                    cleanup_pending,
                } => {
                    if !required_kinds
                        .get(&criterion_id)
                        .is_some_and(|kinds| kinds.contains(&evidence_kind))
                    {
                        return Err(
                            "verification evidence kind no longer satisfies the criterion".into(),
                        );
                    }
                    let Some(identity) = report.identity() else {
                        checkpoint.stage = VerificationCheckpointStageV1::OutcomeUnknown;
                        return Ok(checkpoint.clone());
                    };
                    let execution_available = matches!(
                        &report,
                        crate::common_tests::RunReport::Passed { .. }
                            | crate::common_tests::RunReport::Failed {
                                identity: Some(_),
                                fallback_required: false,
                                ..
                            }
                    );
                    let runner = RunnerProvenanceV1 {
                        plan_hash: identity.plan_hash.clone(),
                        program_hash: identity.program_hash.clone(),
                        report_hash: report.bounded_evidence_hash(),
                        cleanup_complete: !cleanup_pending,
                        execution_available,
                    };
                    if !runner.can_pass() {
                        checkpoint.stage = VerificationCheckpointStageV1::OutcomeUnknown;
                        return Ok(checkpoint.clone());
                    }
                    let evidence =
                        owner.common_case_evidence(&current, &criterion_id, &report, now)?;
                    evidence_refs.push(
                        store
                            .put_final_evidence_record(&verification_binding, evidence.record())
                            .map_err(|error| error.to_string())?,
                    );
                    provenance.insert(criterion_id, runner);
                }
                ObservedAttemptV1::Existing {
                    criterion_id,
                    plan,
                    outcome,
                    executor,
                } => {
                    if !required_kinds
                        .get(&criterion_id)
                        .is_some_and(|kinds| kinds.contains(&EvidenceKindV1::ExistingTest))
                        || outcome.validate_for(&plan).is_err()
                        || !outcome.execution_available
                        || !outcome.cleanup_complete
                    {
                        checkpoint.stage = VerificationCheckpointStageV1::OutcomeUnknown;
                        return Ok(checkpoint.clone());
                    }
                    let report_hash = crate::conversation_state::content_hash(
                        &serde_json::to_vec(&outcome)
                            .map_err(|_| "existing-test outcome cannot be hashed")?,
                    );
                    let runner = RunnerProvenanceV1 {
                        plan_hash: plan.plan_hash(),
                        program_hash: executor.program_hash,
                        report_hash,
                        cleanup_complete: outcome.cleanup_complete,
                        execution_available: outcome.execution_available,
                    };
                    if !runner.can_pass() {
                        checkpoint.stage = VerificationCheckpointStageV1::OutcomeUnknown;
                        return Ok(checkpoint.clone());
                    }
                    let evidence = owner.existing_test_evidence(
                        &current,
                        &criterion_id,
                        &plan,
                        &outcome,
                        now,
                    )?;
                    evidence_refs.push(
                        store
                            .put_final_evidence_record(&verification_binding, evidence.record())
                            .map_err(|error| error.to_string())?,
                    );
                    provenance.insert(criterion_id, runner);
                }
            }
        }
        for run in &mut checkpoint.criterion_runs {
            run.provenance = provenance.remove(&run.criterion_id);
        }
        if !provenance.is_empty()
            || checkpoint
                .criterion_runs
                .iter()
                .any(|run| run.provenance.is_none())
        {
            checkpoint.stage = VerificationCheckpointStageV1::OutcomeUnknown;
            return Ok(checkpoint.clone());
        }
        checkpoint.evidence_refs = evidence_refs;
        checkpoint.transition(VerificationCheckpointStageV1::Terminal)?;
        Ok(checkpoint.clone())
    }

    fn publish(
        &self,
        checkpoint_id: &str,
    ) -> Result<(SavedVerificationCheckpointV1, VerifiedConvergenceBundleV1), String> {
        let checkpoints = self
            .checkpoints
            .lock()
            .map_err(|_| "verification owner lock poisoned")?;
        let checkpoint = checkpoints
            .get(checkpoint_id)
            .ok_or("verification checkpoint is unknown")?
            .clone();
        if checkpoint.stage != VerificationCheckpointStageV1::Terminal {
            return Err("verification has no sealed terminal evidence".into());
        }
        let (snapshot, _observed, binding, verification_binding, needs, runners) =
            self.current_bindings()?;
        if binding != checkpoint.evidence_binding
            || checkpoint.evidence_refs.len() != checkpoint.criterion_runs.len()
            || validate_exact_runs(
                &checkpoint.criterion_runs,
                &needs,
                &runners,
                self.existing_test_executor.as_deref(),
                &self.static_adapters,
            )
            .is_err()
        {
            return Err("terminal verification evidence is stale or incomplete".into());
        }
        let store = super::store::VerificationStore::open(
            &self.session.verification_store_root(),
            &snapshot.state.project_id,
        )
        .map_err(|e| e.to_string())?;
        validate_exact_needs(&store, &verification_binding, &checkpoint.need_refs, &needs)?;
        let required_kinds = required_kinds_by_criterion(&needs)?;
        let run_kinds = checkpoint
            .criterion_runs
            .iter()
            .map(|run| (run.criterion_id.clone(), run.evidence_kind))
            .collect::<BTreeMap<_, _>>();
        let mut evidence = Vec::new();
        for reference in &checkpoint.evidence_refs {
            let record = store
                .load_final_evidence_record(reference, &verification_binding, &binding)
                .map_err(|error| error.to_string())?;
            let kind = run_kinds
                .get(&record.criterion_id)
                .ok_or("final evidence criterion is not required")?;
            if !required_kinds
                .get(&record.criterion_id)
                .is_some_and(|required| required.contains(kind))
            {
                return Err("final evidence kind does not satisfy the current criterion".into());
            }
            evidence.push(record);
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| "system clock is before unix epoch")?
            .as_secs();
        let owner = super::owner::VerificationOwnerV1::new(&store, &self.static_catalog);
        let mut bundle = owner.bundle_from_final_evidence_records(&binding, &evidence, now)?;
        for reference in &checkpoint.review_refs {
            bundle = owner.add_review_reference(bundle, &verification_binding, reference)?;
        }
        Ok((checkpoint, bundle))
    }
}

/// The only package identity available for an `All::Package` projection is a
/// complete inventory with exactly one package.  A mixed-package listing must
/// use the caller's explicit workspace fallback rather than guessing which
/// package the judge meant.
fn package_for_all_selection(inventory: &TestInventoryV1) -> Option<&str> {
    let mut packages = inventory.entries.iter().map(|entry| entry.package.as_str());
    let package = packages.next()?;
    packages
        .all(|current| current == package)
        .then_some(package)
}

/// Rebuild every execution plan from durable inputs before any runner starts.
/// A stale decision, altered catalog, or hand-edited argv therefore fails
/// before process ownership crosses this module boundary.  `All` remains the
/// conservative recovered selection whenever a persisted Jev record cannot
/// be loaded.
fn validate_persisted_test_execution_plans(
    checkpoint: &SavedVerificationCheckpointV1,
    store: &super::store::VerificationStore,
    binding: &super::store::VerificationBindingV1,
    needs: &[super::VerificationNeedV1],
) -> Result<(), String> {
    match (&checkpoint.inventory_ref, &checkpoint.candidate_ref) {
        (None, None) => {
            if !checkpoint.test_execution_plans.is_empty() {
                return Err("verification execution plans require a persisted inventory".into());
            }
            Ok(())
        }
        (Some(inventory_ref), Some(candidate_ref)) => {
            let inventory = store
                .load_inventory(inventory_ref, binding)
                .map_err(|error| error.to_string())?;
            let candidates = store
                .load_candidates(candidate_ref, binding, &inventory)
                .map_err(|error| error.to_string())?;
            let all_scope = checkpoint
                .all_tests_scope
                .unwrap_or(AllTestsScopeV1::Workspace);
            let package_for_all = package_for_all_selection(&inventory);
            let mut criteria = BTreeMap::new();
            for need in needs {
                if criteria.insert(need.criterion_id.clone(), need).is_some() {
                    return Err("verification needs contain duplicate criteria".into());
                }
            }
            if checkpoint.selections.len() != criteria.len()
                || checkpoint.test_execution_plans.len() != criteria.len()
            {
                return Err("verification selection plans are incomplete".into());
            }

            let decisions = checkpoint
                .jev_decision_refs
                .iter()
                .map(|reference| {
                    store.load_jev_decision(reference, binding, &inventory, &candidates)
                })
                .collect::<Result<Vec<_>, _>>();
            for (criterion_id, need) in criteria {
                let question_hash =
                    super::pipeline::test_selection_question_hash(need, &inventory, &candidates)
                        .ok_or("verification test-selection question is invalid")?;
                let decision = decisions.as_ref().ok().and_then(|records| {
                    let mut matches = records
                        .iter()
                        .filter(|record| record.question_hash == question_hash);
                    let first = matches.next()?;
                    matches.next().is_none().then_some(first)
                });
                let recovered = if decisions.is_ok() {
                    super::select_tests_offline(
                        decision,
                        &inventory,
                        &candidates,
                        &question_hash,
                        &binding.diff_hash,
                        super::SCORE_CUTOFF,
                        all_scope,
                    )
                } else {
                    TestSelectionV1::All {
                        scope: all_scope,
                        reason: AllReasonV1::HashMismatch,
                    }
                };
                let saved = checkpoint
                    .selections
                    .get(&criterion_id)
                    .ok_or("verification selection is absent for a criterion")?;
                if *saved != recovered {
                    return Err("persisted verification selection is stale".into());
                }
                let plan = checkpoint
                    .test_execution_plans
                    .get(&criterion_id)
                    .ok_or("verification execution plan is absent for a criterion")?;
                plan.validate_against(&inventory, package_for_all)
                    .map_err(|_| "persisted verification execution plan is stale")?;
                if plan.selection != *saved {
                    return Err("persisted verification execution plan selection is stale".into());
                }
            }
            Ok(())
        }
        _ => Err("verification inventory references are incomplete".into()),
    }
}

fn required_kinds_by_criterion(
    needs: &[super::VerificationNeedV1],
) -> Result<BTreeMap<String, std::collections::BTreeSet<EvidenceKindV1>>, String> {
    let mut result = BTreeMap::new();
    for need in needs {
        if let Some(previous) =
            result.insert(need.criterion_id.clone(), need.evidence_kinds.clone())
            && previous != need.evidence_kinds
        {
            return Err("duplicate criterion has inconsistent evidence requirements".into());
        }
    }
    if result.is_empty() {
        return Err("verification has no required criteria".into());
    }
    Ok(result)
}

fn validate_exact_needs(
    store: &super::store::VerificationStore,
    binding: &super::store::VerificationBindingV1,
    references: &[VerificationRecordRefV1],
    current: &[super::VerificationNeedV1],
) -> Result<(), String> {
    if references.len() != current.len() {
        return Err("verification needs are stale or incomplete".into());
    }
    for (reference, expected) in references.iter().zip(current) {
        let loaded = store
            .load_need(reference, binding)
            .map_err(|error| error.to_string())?;
        if loaded != *expected {
            return Err("verification need does not match the active artifacts".into());
        }
    }
    Ok(())
}

fn validate_exact_runs(
    runs: &[CriterionRunV1],
    needs: &[super::VerificationNeedV1],
    runners: &[FixedCommonRunnerV1],
    existing_executor: Option<&dyn RustTestExecutorV1>,
    static_adapters: &BTreeMap<String, FixedStaticAdapterV1>,
) -> Result<(), String> {
    let required = required_kinds_by_criterion(needs)?;
    if runs.len() != required.len() {
        return Err("verification runner set is incomplete".into());
    }
    let mut registered = runners
        .iter()
        .map(|runner| {
            (
                runner.criterion_id.clone(),
                (runner.runner_id.clone(), runner.evidence_kind),
            )
        })
        .collect::<BTreeMap<String, (String, EvidenceKindV1)>>();
    if let Some(executor) = existing_executor {
        let identity = executor.identity()?;
        identity.validate()?;
        for need in needs
            .iter()
            .filter(|need| need.evidence_kinds.contains(&EvidenceKindV1::ExistingTest))
        {
            registered.insert(
                need.criterion_id.clone(),
                (identity.runner_id.clone(), EvidenceKindV1::ExistingTest),
            );
        }
    }
    for need in needs.iter().filter(|need| {
        need.evidence_kinds.contains(&EvidenceKindV1::Static)
            && static_adapters.contains_key(&need.criterion_id)
    }) {
        let adapter = static_adapters
            .get(&need.criterion_id)
            .ok_or("static adapter is unavailable")?;
        adapter.identity()?;
        registered.insert(
            need.criterion_id.clone(),
            (adapter.runner_id.clone(), EvidenceKindV1::Static),
        );
    }
    for run in runs {
        if !required
            .get(&run.criterion_id)
            .is_some_and(|kinds| kinds.contains(&run.evidence_kind))
            || registered.get(&run.criterion_id)
                != Some(&(run.runner_id.clone(), run.evidence_kind))
        {
            return Err("verification runner no longer satisfies the current criterion".into());
        }
    }
    Ok(())
}

pub fn parse_tool_action(
    value: &serde_json::Value,
) -> Result<VerificationCheckpointActionV1, String> {
    #[derive(Deserialize)]
    #[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
    enum Wire {
        Prepare,
        Run { preparation_id: String },
        Publish { checkpoint_id: String },
    }
    let encoded =
        serde_json::to_vec(value).map_err(|_| "verification arguments are invalid".to_string())?;
    if encoded.len() > 4096 {
        return Err("verification arguments exceed limit".into());
    }
    match serde_json::from_value::<Wire>(value.clone())
        .map_err(|_| "verification arguments are invalid".to_string())?
    {
        Wire::Prepare => Ok(VerificationCheckpointActionV1::Prepare),
        Wire::Run { preparation_id } if opaque(&preparation_id) => {
            Ok(VerificationCheckpointActionV1::Run { preparation_id })
        }
        Wire::Publish { checkpoint_id } if opaque(&checkpoint_id) => {
            Ok(VerificationCheckpointActionV1::Publish { checkpoint_id })
        }
        _ => Err("verification opaque identifier is invalid".into()),
    }
}

fn opaque(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_ID
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;
    fn hash(c: char) -> String {
        std::iter::repeat_n(c, 64).collect()
    }
    fn binding() -> EvidenceBindingV1 {
        EvidenceBindingV1 {
            task_id: "task".into(),
            task_definition_hash: hash('a'),
            subject_hash: hash('b'),
            source_hash: hash('c'),
            diff_hash: hash('d'),
            artifact_graph_basis_hash: hash('e'),
        }
    }
    fn artifact() -> ArtifactRefV1 {
        ArtifactRefV1 {
            id: "contract".into(),
            revision: 1,
            content_hash: hash('f'),
        }
    }
    fn store_binding(seed: char) -> super::super::store::VerificationBindingV1 {
        super::super::store::VerificationBindingV1 {
            project_graph_hash: hash(seed),
            analyzer_hash: hash('6'),
            tool_hash: hash('7'),
            source_hash: hash('8'),
            diff_hash: hash('9'),
            config_hash: hash('a'),
            analysis_request_hash: hash('b'),
        }
    }
    fn evidence_ref(seed: char) -> VerificationRecordRefV1 {
        VerificationRecordRefV1 {
            kind: super::super::store::VerificationRecordKindV1::Evidence,
            content_hash: hash(seed),
            binding: store_binding('5'),
        }
    }
    fn provenance() -> RunnerProvenanceV1 {
        RunnerProvenanceV1 {
            plan_hash: hash('1'),
            program_hash: hash('2'),
            report_hash: hash('3'),
            cleanup_complete: true,
            execution_available: true,
        }
    }
    #[test]
    fn parser_accepts_only_opaque_handles() {
        assert!(matches!(
            parse_tool_action(&serde_json::json!({"op":"prepare"})),
            Ok(VerificationCheckpointActionV1::Prepare)
        ));
        assert!(
            parse_tool_action(
                &serde_json::json!({"op":"run","preparation_id":"p-1","command":"x"})
            )
            .is_err()
        );
        assert!(
            parse_tool_action(&serde_json::json!({"op":"publish","checkpoint_id":"/tmp/x"}))
                .is_err()
        );
    }
    #[test]
    fn running_crash_never_reexecutes() {
        let mut value = SavedVerificationCheckpointV1::new(
            "checkpoint".into(),
            "idem".into(),
            1,
            vec![artifact()],
            binding(),
        )
        .unwrap();
        value
            .transition(VerificationCheckpointStageV1::Running)
            .unwrap();
        value.mark_crash_recovered();
        assert_eq!(value.stage, VerificationCheckpointStageV1::OutcomeUnknown);
        assert!(
            value
                .transition(VerificationCheckpointStageV1::Running)
                .is_err()
        );
    }
    #[test]
    fn pending_cleanup_cannot_terminal() {
        let mut value = SavedVerificationCheckpointV1::new(
            "checkpoint".into(),
            "idem".into(),
            1,
            vec![artifact()],
            binding(),
        )
        .unwrap();
        value
            .transition(VerificationCheckpointStageV1::Running)
            .unwrap();
        value.criterion_runs = vec![CriterionRunV1 {
            criterion_id: "criterion".into(),
            evidence_kind: EvidenceKindV1::CommonCase,
            runner_id: "runner".into(),
            provenance: Some(RunnerProvenanceV1 {
                plan_hash: hash('1'),
                program_hash: hash('2'),
                report_hash: hash('3'),
                cleanup_complete: false,
                execution_available: true,
            }),
        }];
        value.evidence_refs = vec![evidence_ref('4')];
        assert!(
            value
                .transition(VerificationCheckpointStageV1::Terminal)
                .is_err()
        );
    }

    #[test]
    fn terminal_requires_one_record_for_every_criterion() {
        let mut value = SavedVerificationCheckpointV1::new(
            "checkpoint".into(),
            "idem".into(),
            1,
            vec![artifact()],
            binding(),
        )
        .unwrap();
        value.criterion_runs = ["one", "two"]
            .into_iter()
            .map(|criterion| CriterionRunV1 {
                criterion_id: criterion.into(),
                evidence_kind: EvidenceKindV1::CommonCase,
                runner_id: format!("runner-{criterion}"),
                provenance: Some(provenance()),
            })
            .collect();
        value
            .transition(VerificationCheckpointStageV1::Running)
            .unwrap();
        value.evidence_refs = vec![evidence_ref('c')];
        assert!(
            value
                .transition(VerificationCheckpointStageV1::Terminal)
                .is_err()
        );
        value.evidence_refs.push(evidence_ref('d'));
        value
            .transition(VerificationCheckpointStageV1::Terminal)
            .unwrap();
    }

    #[test]
    fn required_evidence_kind_mismatch_is_rejected() {
        let need = super::super::VerificationNeedV1 {
            schema_version: super::super::SCHEMA_VERSION,
            requirement_id: "requirement".into(),
            task_id: "task".into(),
            criterion_id: "criterion".into(),
            observable_failure: "failure".into(),
            symbols: Vec::new(),
            evidence_kinds: BTreeSet::from([EvidenceKindV1::CommonCase]),
        };
        let run = CriterionRunV1 {
            criterion_id: "criterion".into(),
            evidence_kind: EvidenceKindV1::ExistingTest,
            runner_id: "runner".into(),
            provenance: None,
        };
        let runner = FixedCommonRunnerV1 {
            criterion_id: "criterion".into(),
            evidence_kind: EvidenceKindV1::ExistingTest,
            runner_id: "runner".into(),
            plan: crate::common_tests::Plan {
                version: crate::common_tests::SCHEMA_VERSION,
                argv: vec!["/usr/bin/true".into()],
                stdin: None,
                cases: vec![crate::common_tests::Case::ExitsWith { status: 0 }],
            },
            policy: polaris_sandbox::SandboxPolicy::new(
                polaris_sandbox::SandboxMode::FullAccess,
                &[],
            )
            .unwrap(),
            cwd: std::env::temp_dir(),
            write_root: std::env::temp_dir(),
            timeout: Duration::from_secs(1),
        };
        assert!(validate_exact_runs(&[run], &[need], &[runner], None, &BTreeMap::new()).is_err());
    }

    struct RegisteredExistingExecutor;

    impl RustTestExecutorV1 for RegisteredExistingExecutor {
        fn identity(&self) -> Result<RustTestExecutorIdentityV1, String> {
            Ok(RustTestExecutorIdentityV1 {
                runner_id: "existing-runner".into(),
                program_hash: hash('1'),
                config_hash: hash('2'),
            })
        }

        fn execute(&self, plan: &RustTestExecutionPlanV1) -> RustTestExecutionOutcomeV1 {
            RustTestExecutionOutcomeV1 {
                plan_hash: plan.plan_hash(),
                execution_available: false,
                cleanup_complete: false,
                passed: None,
            }
        }
    }

    #[test]
    fn existing_test_run_requires_registered_executor_and_matching_identity() {
        let need = super::super::VerificationNeedV1 {
            schema_version: super::super::SCHEMA_VERSION,
            requirement_id: "requirement".into(),
            task_id: "task".into(),
            criterion_id: "criterion".into(),
            observable_failure: "failure".into(),
            symbols: Vec::new(),
            evidence_kinds: BTreeSet::from([EvidenceKindV1::ExistingTest]),
        };
        let run = CriterionRunV1 {
            criterion_id: "criterion".into(),
            evidence_kind: EvidenceKindV1::ExistingTest,
            runner_id: "existing-runner".into(),
            provenance: None,
        };
        assert!(
            validate_exact_runs(
                std::slice::from_ref(&run),
                std::slice::from_ref(&need),
                &[],
                None,
                &BTreeMap::new(),
            )
            .is_err()
        );
        assert!(
            validate_exact_runs(
                std::slice::from_ref(&run),
                std::slice::from_ref(&need),
                &[],
                Some(&RegisteredExistingExecutor),
                &BTreeMap::new(),
            )
            .is_ok()
        );

        let mut mismatched = run;
        mismatched.runner_id = "other-runner".into();
        assert!(
            validate_exact_runs(
                &[mismatched],
                &[need],
                &[],
                Some(&RegisteredExistingExecutor),
                &BTreeMap::new(),
            )
            .is_err()
        );
    }

    struct Observed;
    impl VerificationObserverV1 for Observed {
        fn observe(&self) -> Result<HostObservedVerificationV1, String> {
            Ok(HostObservedVerificationV1 {
                subject_hash: hash('1'),
                source_hash: hash('2'),
                diff_hash: hash('3'),
                project_graph_hash: hash('4'),
            })
        }
    }

    fn static_test_adapter(root: &Path, command: &str, timeout: Duration) -> FixedStaticAdapterV1 {
        use std::os::unix::fs::PermissionsExt;

        let workspace = root.join("workspace");
        let container = root.join("private");
        let home = container.join("home");
        let tmpdir = container.join("tmp");
        for path in [&workspace, &container, &home, &tmpdir] {
            std::fs::create_dir_all(path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let workspace = workspace.canonicalize().unwrap();
        let container = container.canonicalize().unwrap();
        let home = home.canonicalize().unwrap();
        let tmpdir = tmpdir.canonicalize().unwrap();
        let program = Path::new("/bin/sh").canonicalize().unwrap();
        let executable_hash =
            crate::conversation_state::content_hash(&std::fs::read(&program).unwrap());
        let runtime_roots = ["/bin", "/usr/bin", "/usr/lib", "/System/Library"]
            .into_iter()
            .filter_map(|candidate| {
                let path = Path::new(candidate);
                path.is_dir().then(|| path.canonicalize().unwrap())
            })
            .collect::<Vec<_>>();
        let policy = polaris_sandbox::SandboxPolicy::isolated(
            polaris_sandbox::SandboxMode::ReadOnly,
            &workspace,
            &runtime_roots,
        )
        .unwrap()
        .with_isolated_private_environment(&container, &home, &tmpdir)
        .unwrap()
        .require_process_containment()
        .unwrap();
        FixedStaticAdapterV1 {
            criterion_id: "criterion-static".into(),
            runner_id: "static-fixture".into(),
            capability: StaticCapabilityV1 {
                schema_version: super::super::SCHEMA_VERSION,
                analyzer_id: "static-fixture".into(),
                executable_hash,
                version: "1".into(),
                output_schema_hash: static_adapter_output_schema_hash_v1(),
                rules: vec![super::super::StaticRuleCapabilityV1 {
                    rule_id: "fixture-rule".into(),
                    obligation_kinds: ["criterion".into()].into_iter().collect(),
                    maximum_scope: AnalysisScopeV1::Package,
                    clean_proves_satisfied: true,
                    requires_project_graph: false,
                }],
            },
            rule_id: "fixture-rule".into(),
            program,
            argv: vec!["-c".into(), command.into()],
            policy,
            cwd: workspace,
            timeout,
            maximum_output_bytes: 4096,
        }
    }

    fn static_observed() -> HostObservedVerificationV1 {
        HostObservedVerificationV1 {
            subject_hash: hash('1'),
            source_hash: hash('2'),
            diff_hash: hash('3'),
            project_graph_hash: hash('4'),
        }
    }

    fn record_static_fixture(
        adapter: &FixedStaticAdapterV1,
        store: &super::super::store::VerificationStore,
        outcome: StaticAdapterOutcomeV1,
    ) -> Result<(VerificationRecordRefV1, StaticEvidenceV1), String> {
        let observed = static_observed();
        let mut catalog = StaticCapabilityCatalogV1::default();
        catalog
            .register(adapter.capability.clone())
            .map_err(|error| error.to_string())?;
        let identity = adapter.identity()?;
        let binding = adapter.verification_binding(&observed, &identity)?;
        let parser = RegisteredStaticParserV1 {
            adapter,
            observed: &observed,
            identity: &identity,
            duration_ms: 1,
        };
        let owner = super::super::owner::VerificationOwnerV1::new(store, &catalog);
        let reference = owner.record_registered_static_outcome(
            &binding,
            &adapter.invocation(),
            &outcome,
            &parser,
        )?;
        let evidence = store
            .load_registered_static_evidence(&reference, &binding, &catalog)
            .map_err(|error| error.to_string())?;
        Ok((reference, evidence))
    }

    #[test]
    fn registered_static_adapter_persists_complete_satisfied_and_violated_results() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store =
            super::super::store::VerificationStore::open(store_root.path(), "project").unwrap();
        let satisfied = static_test_adapter(
            root.path(),
            r#"printf '%s' '{"schema_version":1,"state":"satisfied","findings":[],"error_kind":null}'"#,
            Duration::from_secs(2),
        );
        let (_, evidence) = record_static_fixture(
            &satisfied,
            &store,
            StaticAdapterOutcomeV1::Completed {
                status: 0,
                stdout:
                    br#"{"schema_version":1,"state":"satisfied","findings":[],"error_kind":null}"#
                        .to_vec(),
                stderr: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(evidence.state, StaticEvidenceStateV1::Satisfied);
        assert_eq!(
            evidence.effective_state(Some(&satisfied.capability)),
            StaticEvidenceStateV1::Satisfied
        );

        let violated = static_test_adapter(
            root.path(),
            r#"printf '%s' '{"schema_version":1,"state":"violated","findings":[{"rule_id":"fixture-rule","path":"src/lib.rs","line":1,"symbol":null,"introduced_by_current_diff":true,"baseline":false}],"error_kind":null}'; exit 1"#,
            Duration::from_secs(2),
        );
        let (_, evidence) = record_static_fixture(
            &violated,
            &store,
            StaticAdapterOutcomeV1::Completed {
                status: 1,
                stdout: br#"{"schema_version":1,"state":"violated","findings":[{"rule_id":"fixture-rule","path":"src/lib.rs","line":1,"symbol":null,"introduced_by_current_diff":true,"baseline":false}],"error_kind":null}"#.to_vec(),
                stderr: Vec::new(),
            },
        )
        .unwrap();
        assert_eq!(evidence.state, StaticEvidenceStateV1::Violated);
        assert_eq!(evidence.findings.len(), 1);
        assert_ne!(evidence.findings[0].fingerprint, "pending-host-fingerprint");
    }

    #[test]
    fn static_timeout_and_execution_failure_are_unavailable_not_satisfied() {
        let root = tempfile::tempdir().unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store =
            super::super::store::VerificationStore::open(store_root.path(), "project").unwrap();
        let timeout = static_test_adapter(
            root.path(),
            "while :; do :; done",
            Duration::from_millis(20),
        );
        let (_, evidence) =
            record_static_fixture(&timeout, &store, StaticAdapterOutcomeV1::TimedOut).unwrap();
        assert_eq!(evidence.state, StaticEvidenceStateV1::Unavailable);
        assert_eq!(evidence.error_kind.as_deref(), Some("timeout"));

        let unavailable = static_test_adapter(root.path(), "exit 2", Duration::from_secs(2));
        let (_, evidence) = record_static_fixture(
            &unavailable,
            &store,
            StaticAdapterOutcomeV1::Failed {
                reason: "sandbox_unavailable:test".into(),
            },
        )
        .unwrap();
        assert_eq!(evidence.state, StaticEvidenceStateV1::Unavailable);
        assert_eq!(evidence.error_kind.as_deref(), Some("execution_error"));
    }

    #[test]
    fn satisfied_static_evidence_reaches_the_durable_phase_gate() {
        use crate::{
            conversation_state::HistoryMode,
            session_store::PersistedSession,
            workflow::{
                ChangeContractV1, CriterionEvidenceKindV1, Phase, RoutineEligibilityV1,
                SessionWorkflow, VerificationCriterionV1, WorkflowConfig,
            },
        };

        let root = tempfile::tempdir().unwrap();
        let workflow = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            initial_phase: Phase::Specify,
            ..WorkflowConfig::default()
        });
        let saved = PersistedSession::create(
            root.path(),
            &root.path().join("memory.sqlite3"),
            "static-phase-gate",
            HistoryMode::Legacy,
            Some(&workflow),
        )
        .unwrap();
        let criterion = VerificationCriterionV1::typed(
            "The registered static condition is satisfied.",
            "The registered static condition is violated.",
            true,
            [CriterionEvidenceKindV1::Static].into_iter().collect(),
        )
        .unwrap();
        let criterion_id = criterion.id.clone();
        let contract = ChangeContractV1::draft_typed(
            "change-static-phase-gate",
            RoutineEligibilityV1::local_component("polaris-core"),
            "validate the registered static condition",
            "the condition may be violated",
            "the condition is satisfied",
            vec![criterion],
        )
        .unwrap();
        let implemented = saved
            .publish_and_approve_routine_contract(workflow.state().revision, contract)
            .unwrap();
        implemented.request_phase(Phase::Implement).unwrap();
        saved.checkpoint(Some(&implemented)).unwrap();

        let mut adapter = static_test_adapter(
            root.path(),
            r#"printf '%s' '{"schema_version":1,"state":"satisfied","findings":[],"error_kind":null}'"#,
            Duration::from_secs(1),
        );
        adapter.criterion_id = criterion_id.clone();
        adapter.runner_id = format!("static-{criterion_id}");
        let owner =
            SessionVerificationToolOwnerV1::new(saved.clone(), Arc::new(Observed), Vec::new())
                .unwrap()
                .with_static_adapters(vec![adapter.clone()])
                .unwrap();
        let prepared = owner.prepare().unwrap();
        let generation = saved.snapshot().unwrap().state.generation;
        saved
            .publish_verification_checkpoint(generation, prepared.clone())
            .unwrap();
        let running = owner.begin_run(&prepared.checkpoint_id).unwrap();
        assert_eq!(running.stage, VerificationCheckpointStageV1::Running);

        // Native sandbox nesting is not available in every test host. Replace
        // only the observed adapter result with the same registered identity;
        // persistence, projection, convergence and the durable phase gate all
        // remain the product path exercised below.
        let observed = static_observed();
        let identity = adapter.identity().unwrap();
        let static_binding = adapter.verification_binding(&observed, &identity).unwrap();
        let mut catalog = StaticCapabilityCatalogV1::default();
        catalog.register(adapter.capability.clone()).unwrap();
        let store = super::super::store::VerificationStore::open(
            &saved.verification_store_root(),
            "static-phase-gate",
        )
        .unwrap();
        let static_owner = super::super::owner::VerificationOwnerV1::new(&store, &catalog);
        let reference = static_owner
            .record_registered_static_outcome(
                &static_binding,
                &adapter.invocation(),
                &StaticAdapterOutcomeV1::Completed {
                    status: 0,
                    stdout: br#"{"schema_version":1,"state":"satisfied","findings":[],"error_kind":null}"#
                        .to_vec(),
                    stderr: Vec::new(),
                },
                &RegisteredStaticParserV1 {
                    adapter: &adapter,
                    observed: &observed,
                    identity: &identity,
                    duration_ms: 1,
                },
            )
            .unwrap();
        owner.attempts.lock().unwrap().insert(
            prepared.checkpoint_id.clone(),
            vec![CriterionAttemptV1 {
                criterion_id: criterion_id.clone(),
                evidence_kind: EvidenceKindV1::Static,
                outcome: CriterionAttemptOutcomeV1::Static {
                    reference,
                    verification_binding: static_binding,
                    identity,
                    cleanup: None,
                },
            }],
        );
        let terminal = owner.poll_run(&prepared.checkpoint_id).unwrap();
        assert_eq!(terminal.stage, VerificationCheckpointStageV1::Terminal);
        assert_eq!(terminal.evidence_refs.len(), 1);
        let generation = saved.snapshot().unwrap().state.generation;
        saved
            .publish_verification_checkpoint(generation, terminal.clone())
            .unwrap();
        let (terminal, bundle) = owner.publish(&prepared.checkpoint_id).unwrap();
        let generation = saved.snapshot().unwrap().state.generation;
        let (workflow, published) = saved
            .publish_verification_convergence(generation, terminal, bundle)
            .unwrap();
        assert_eq!(published.stage, VerificationCheckpointStageV1::Published);
        let convergence = workflow.gates.convergence.as_ref().unwrap();
        assert_eq!(convergence.evidence.len(), 1);
        assert_eq!(convergence.evidence[0].criterion_id, criterion_id);
    }

    #[test]
    fn static_adapter_rejects_tampered_executable_config_binding_and_schema() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("static-tool");
        std::fs::copy(Path::new("/bin/sh").canonicalize().unwrap(), &program).unwrap();
        let mut permissions = std::fs::metadata(&program).unwrap().permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&program, permissions).unwrap();
        let program = program.canonicalize().unwrap();
        let mut adapter = static_test_adapter(
            root.path(),
            r#"printf '%s' '{"schema_version":1,"state":"satisfied","findings":[],"error_kind":null}'"#,
            Duration::from_secs(2),
        );
        adapter.program = program.clone();
        adapter.capability.executable_hash =
            crate::conversation_state::content_hash(&std::fs::read(&program).unwrap());
        assert!(adapter.identity().is_ok());
        std::fs::write(&program, b"tampered").unwrap();
        assert!(adapter.identity().is_err());

        let valid = static_test_adapter(
            root.path(),
            r#"printf '%s' '{"schema_version":1,"state":"satisfied","findings":[],"error_kind":null}'"#,
            Duration::from_secs(2),
        );
        let observed = static_observed();
        let attempt = valid.execute().unwrap();
        let binding = valid
            .verification_binding(&observed, &attempt.identity)
            .unwrap();
        let mut changed = valid.clone();
        changed.argv.push("ignored".into());
        let changed_identity = changed.identity().unwrap();
        assert_ne!(attempt.identity.config_hash, changed_identity.config_hash);

        let parser = RegisteredStaticParserV1 {
            adapter: &changed,
            observed: &observed,
            identity: &changed_identity,
            duration_ms: attempt.duration_ms,
        };
        let mut catalog = StaticCapabilityCatalogV1::default();
        catalog.register(changed.capability.clone()).unwrap();
        let store_root = tempfile::tempdir().unwrap();
        let store =
            super::super::store::VerificationStore::open(store_root.path(), "project").unwrap();
        let owner = super::super::owner::VerificationOwnerV1::new(&store, &catalog);
        assert!(
            owner
                .record_registered_static_outcome(
                    &binding,
                    &changed.invocation(),
                    &attempt.outcome,
                    &parser,
                )
                .is_err()
        );

        let mut wrong_binding = valid
            .verification_binding(&observed, &attempt.identity)
            .unwrap();
        wrong_binding.source_hash = hash('f');
        assert!(
            owner
                .record_registered_static_outcome(
                    &wrong_binding,
                    &valid.invocation(),
                    &attempt.outcome,
                    &RegisteredStaticParserV1 {
                        adapter: &valid,
                        observed: &observed,
                        identity: &attempt.identity,
                        duration_ms: attempt.duration_ms,
                    },
                )
                .is_err()
        );

        let unknown_field = StaticAdapterOutcomeV1::Completed {
            status: 0,
            stdout: br#"{"schema_version":1,"state":"satisfied","findings":[],"error_kind":null,"extra":true}"#.to_vec(),
            stderr: Vec::new(),
        };
        assert!(
            owner
                .record_registered_static_outcome(
                    &binding,
                    &valid.invocation(),
                    &unknown_field,
                    &RegisteredStaticParserV1 {
                        adapter: &valid,
                        observed: &observed,
                        identity: &attempt.identity,
                        duration_ms: 1,
                    },
                )
                .is_err()
        );
        let partial = StaticAdapterOutcomeV1::Completed {
            status: 0,
            stdout: br#"{"schema_version":1,"state":"satisfied","findings":[]}"#.to_vec(),
            stderr: Vec::new(),
        };
        assert!(
            owner
                .record_registered_static_outcome(
                    &binding,
                    &valid.invocation(),
                    &partial,
                    &RegisteredStaticParserV1 {
                        adapter: &valid,
                        observed: &observed,
                        identity: &attempt.identity,
                        duration_ms: 1,
                    },
                )
                .is_err()
        );
    }

    #[test]
    fn restart_reloads_prepared_and_converts_running_to_unknown() {
        let root = tempfile::tempdir().unwrap();
        let saved = crate::session_store::PersistedSession::create(
            root.path(),
            &root.path().join("memory.sqlite3"),
            "verification-restart",
            crate::conversation_state::HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let prepared = SavedVerificationCheckpointV1::new(
            "checkpoint".into(),
            "idem".into(),
            0,
            vec![artifact()],
            binding(),
        )
        .unwrap();
        saved
            .publish_verification_checkpoint(0, prepared.clone())
            .unwrap();
        let owner =
            SessionVerificationToolOwnerV1::new(saved.clone(), Arc::new(Observed), Vec::new())
                .unwrap();
        assert_eq!(
            owner
                .checkpoints
                .lock()
                .unwrap()
                .get("checkpoint")
                .unwrap()
                .stage,
            VerificationCheckpointStageV1::Prepared
        );

        let mut running = prepared;
        running
            .transition(VerificationCheckpointStageV1::Running)
            .unwrap();
        saved.publish_verification_checkpoint(1, running).unwrap();
        let recovered =
            SessionVerificationToolOwnerV1::new(saved.clone(), Arc::new(Observed), Vec::new())
                .unwrap();
        assert_eq!(
            recovered
                .checkpoints
                .lock()
                .unwrap()
                .get("checkpoint")
                .unwrap()
                .stage,
            VerificationCheckpointStageV1::OutcomeUnknown
        );
        assert_eq!(
            saved
                .snapshot()
                .unwrap()
                .state
                .verification_checkpoint
                .unwrap()
                .stage,
            VerificationCheckpointStageV1::OutcomeUnknown
        );
    }
}
