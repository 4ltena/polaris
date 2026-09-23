//! Bounded, secret-free V2 desktop projection of orchestration state.
//!
//! These are display values only.  They neither grant a lease nor carry an
//! executable command, MCP payload, credential, file path, or artifact body.

use crate::{ids::*, request::WorkflowPhase};
use serde::{Deserialize, Deserializer, Serialize, de};

const MAX_SUMMARY_BYTES: usize = 512;
const MAX_ITEMS: usize = 32;
const MAX_ACCEPTANCE: usize = 64;
const MAX_EVIDENCE: usize = 64;
const MAX_LEASES: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowStepV2 {
    Idle,
    Context,
    Planning,
    Executing,
    Reviewing,
    Converging,
    Blocked,
    Complete,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceSummaryStateV2 {
    Pending,
    Passed,
    Failed,
    Stale,
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConvergenceStateV2 {
    Unavailable,
    Pending,
    Blocked,
    Allowed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedSessionKindV2 {
    Current,
    Sidechat,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SidechatStateV2 {
    None,
    Active,
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedSessionStateV2 {
    Idle,
    Queued,
    Running,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildControlStateV2 {
    Queued,
    Running,
    Waiting,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
    OutcomeUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpLeaseStateV2 {
    Reserved,
    Active,
    Releasing,
    Released,
    Expired,
    OutcomeUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommonRunnerStateV2 {
    Unavailable,
    Idle,
    Preparing,
    Running,
    Completed,
    Failed,
    CleanupPending,
    OutcomeUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ClarificationStateV2 {
    Unavailable,
    None,
    Pending,
    Resolved,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClarificationSummaryV2 {
    pub state: ClarificationStateV2,
    pub questions: Vec<ClarificationQuestionV2>,
    pub unanswered_count: DecimalU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClarificationQuestionV2 {
    pub question_id: String,
    pub text: String,
    pub options: Vec<ClarificationOptionV2>,
    pub free_text: ClarificationFreeTextV2,
    pub answered: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClarificationOptionV2 {
    pub option_id: String,
    pub label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ClarificationFreeTextV2 {
    pub allowed: bool,
    pub required: bool,
    pub max_bytes: DecimalU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArtifactRefV2 {
    pub artifact_id: String,
    pub content_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceSummaryV2 {
    pub criterion_id: String,
    pub state: AcceptanceSummaryStateV2,
    pub evidence_count: DecimalU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceSummaryV2 {
    pub passed: DecimalU64,
    pub failed: DecimalU64,
    pub stale: DecimalU64,
    pub unavailable: DecimalU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConvergenceSummaryV2 {
    pub state: ConvergenceStateV2,
    pub required_criteria: DecimalU64,
    pub unresolved_gaps: DecimalU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSummaryV2 {
    pub phase: WorkflowPhase,
    pub step: WorkflowStepV2,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_feature: Option<ArtifactRefV2>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_plan: Option<ArtifactRefV2>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub active_task: Option<ArtifactRefV2>,
    pub acceptance: Vec<AcceptanceSummaryV2>,
    pub evidence: EvidenceSummaryV2,
    pub convergence: ConvergenceSummaryV2,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionManagerSummaryV2 {
    pub manager_revision: DecimalU64,
    pub session_count: DecimalU64,
    pub session_total: DecimalU64,
    pub session_omitted: bool,
    pub sessions: Vec<ManagedSessionSummaryV2>,
    pub sidechat: SidechatStateV2,
    pub queued_admissions: DecimalU64,
    pub active_runs: DecimalU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedSessionSummaryV2 {
    pub session_id: SessionId,
    pub kind: ManagedSessionKindV2,
    pub state: ManagedSessionStateV2,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_session_id: Option<SessionId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentAttemptSummaryV2 {
    pub root_run_id: RunId,
    pub root_attempt_id: AttemptId,
    pub control_revision: DecimalU64,
    pub logical_child_id: String,
    pub attempt_id: String,
    pub display_name: String,
    pub role: String,
    pub state: ChildControlStateV2,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub activity: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpLeaseSummaryV2 {
    pub lease_id: String,
    pub server_id: String,
    pub holder_run_id: RunId,
    pub holder_attempt_id: AttemptId,
    pub state: McpLeaseStateV2,
    pub expires_at_unix_ms: DecimalU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommonRunnerSummaryV2 {
    pub state: CommonRunnerStateV2,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub case_id: Option<String>,
    pub completed_cases: DecimalU64,
    pub failed_cases: DecimalU64,
    pub cleanup_pending: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OrchestrationSummaryV2 {
    pub workflow: WorkflowSummaryV2,
    pub session_manager: SessionManagerSummaryV2,
    pub subagents: Vec<SubagentAttemptSummaryV2>,
    pub subagent_total: DecimalU64,
    pub subagent_omitted: bool,
    pub mcp_leases: Vec<McpLeaseSummaryV2>,
    pub mcp_lease_total: DecimalU64,
    pub mcp_lease_omitted: bool,
    pub common_runner: CommonRunnerSummaryV2,
    pub clarification: ClarificationSummaryV2,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RawOrchestrationSummaryV2 {
    workflow: WorkflowSummaryV2,
    session_manager: SessionManagerSummaryV2,
    subagents: Vec<SubagentAttemptSummaryV2>,
    subagent_total: DecimalU64,
    subagent_omitted: bool,
    mcp_leases: Vec<McpLeaseSummaryV2>,
    mcp_lease_total: DecimalU64,
    mcp_lease_omitted: bool,
    common_runner: CommonRunnerSummaryV2,
    clarification: ClarificationSummaryV2,
}

impl<'de> Deserialize<'de> for OrchestrationSummaryV2 {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = RawOrchestrationSummaryV2::deserialize(deserializer)?;
        let value = Self {
            workflow: raw.workflow,
            session_manager: raw.session_manager,
            subagents: raw.subagents,
            subagent_total: raw.subagent_total,
            subagent_omitted: raw.subagent_omitted,
            mcp_leases: raw.mcp_leases,
            mcp_lease_total: raw.mcp_lease_total,
            mcp_lease_omitted: raw.mcp_lease_omitted,
            common_runner: raw.common_runner,
            clarification: raw.clarification,
        };
        value.validate().map_err(de::Error::custom)?;
        Ok(value)
    }
}

impl OrchestrationSummaryV2 {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.workflow.acceptance.len() > MAX_ACCEPTANCE
            || self.subagents.len() > MAX_ITEMS
            || self.mcp_leases.len() > MAX_LEASES
        {
            return Err("orchestration list exceeds its bound");
        }
        if self.session_manager.session_count.get() > 2
            || self.session_manager.session_total.get() > 2
            || self.session_manager.sessions.len() > 2
            || self.session_manager.queued_admissions.get() > 32
        {
            return Err("session manager summary exceeds its bound");
        }
        if self.session_manager.session_count != self.session_manager.session_total
            || self.session_manager.session_omitted
            || self.session_manager.session_total.get()
                != self.session_manager.sessions.len() as u64
        {
            return Err("session summary is not a complete bounded projection");
        }
        validate_total(
            self.subagent_total,
            self.subagent_omitted,
            self.subagents.len(),
            MAX_ITEMS,
        )?;
        validate_total(
            self.mcp_lease_total,
            self.mcp_lease_omitted,
            self.mcp_leases.len(),
            MAX_LEASES,
        )?;
        validate_workflow(&self.workflow)?;
        validate_runner(&self.common_runner)?;
        if self.clarification.questions.len() > 3
            || self.clarification.unanswered_count.get() > self.clarification.questions.len() as u64
        {
            return Err("clarification summary exceeds its bound");
        }
        let unanswered = self
            .clarification
            .questions
            .iter()
            .filter(|question| !question.answered)
            .count() as u64;
        if self.clarification.unanswered_count.get() != unanswered
            || match self.clarification.state {
                ClarificationStateV2::Unavailable | ClarificationStateV2::None => {
                    !self.clarification.questions.is_empty() || unanswered != 0
                }
                ClarificationStateV2::Pending => {
                    self.clarification.questions.is_empty() || unanswered == 0
                }
                ClarificationStateV2::Resolved => {
                    self.clarification.questions.is_empty() || unanswered != 0
                }
            }
        {
            return Err("clarification state does not match its questions");
        }
        for question in &self.clarification.questions {
            validate_id(&question.question_id)?;
            if question.text.is_empty() || question.text.len() > 32 * 1024 {
                return Err("invalid clarification question");
            }
            if question.options.len() > 8
                || question.free_text.max_bytes.get() > 32 * 1024
                || (!question.free_text.allowed && question.free_text.required)
                || (question.options.is_empty() && !question.free_text.allowed)
            {
                return Err("invalid clarification answer contract");
            }
            for option in &question.options {
                validate_id(&option.option_id)?;
                validate_text(&option.label)?;
            }
        }
        for child in &self.subagents {
            validate_id(&child.logical_child_id)?;
            validate_id(&child.attempt_id)?;
            validate_text(&child.display_name)?;
            validate_text(&child.role)?;
            if let Some(activity) = &child.activity {
                validate_text(activity)?;
            }
        }
        for lease in &self.mcp_leases {
            validate_id(&lease.lease_id)?;
            validate_id(&lease.server_id)?;
        }
        Ok(())
    }
}

fn validate_total(
    total: DecimalU64,
    omitted: bool,
    visible: usize,
    limit: usize,
) -> Result<(), &'static str> {
    if total.get() < visible as u64 || (!omitted && total.get() != visible as u64) {
        return Err("orchestration total does not match the projection");
    }
    if omitted && visible != limit {
        return Err("an omitted orchestration projection must fill its bounded list");
    }
    Ok(())
}

fn validate_workflow(workflow: &WorkflowSummaryV2) -> Result<(), &'static str> {
    for reference in [
        &workflow.active_feature,
        &workflow.active_plan,
        &workflow.active_task,
    ]
    .into_iter()
    .flatten()
    {
        validate_id(&reference.artifact_id)?;
        validate_hash(&reference.content_hash)?;
    }
    for acceptance in &workflow.acceptance {
        validate_id(&acceptance.criterion_id)?;
    }
    let evidence = &workflow.evidence;
    if evidence.passed.get()
        + evidence.failed.get()
        + evidence.stale.get()
        + evidence.unavailable.get()
        > MAX_EVIDENCE as u64
    {
        return Err("evidence count exceeds its bound");
    }
    if workflow.convergence.required_criteria.get() > MAX_ACCEPTANCE as u64
        || workflow.convergence.unresolved_gaps.get() > MAX_EVIDENCE as u64
    {
        return Err("convergence summary exceeds its bound");
    }
    Ok(())
}

fn validate_runner(runner: &CommonRunnerSummaryV2) -> Result<(), &'static str> {
    if let Some(case_id) = &runner.case_id {
        validate_id(case_id)?;
    }
    if runner.completed_cases.get() > MAX_ITEMS as u64
        || runner.failed_cases.get() > MAX_ITEMS as u64
    {
        return Err("common runner count exceeds its bound");
    }
    Ok(())
}

fn validate_id(value: &str) -> Result<(), &'static str> {
    if value.is_empty() || value.len() > MAX_ID_BYTES {
        return Err("invalid orchestration ID");
    }
    Ok(())
}

fn validate_hash(value: &str) -> Result<(), &'static str> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err("invalid orchestration hash");
    }
    Ok(())
}

fn validate_text(value: &str) -> Result<(), &'static str> {
    if value.is_empty() || value.len() > MAX_SUMMARY_BYTES {
        return Err("invalid orchestration text");
    }
    Ok(())
}
