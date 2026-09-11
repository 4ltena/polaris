//! 保存世代、下書き、設定、作業・親子run、回答可能な未解決承認のsnapshot値を保持する。

use crate::{ids::*, run_state::RunState};
use serde::{Deserialize, Serialize};

/// 原文保存期間とは独立した、要求履歴の構成方式。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryMode {
    #[default]
    Legacy,
    Strict10,
}
impl HistoryMode {
    pub fn is_legacy(&self) -> bool {
        *self == Self::Legacy
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryPhase {
    Preparing,
    Ready,
    Failed,
    OutcomeUnknown,
}

crate::object_wire! {
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryUsage {
    pub input_tokens: DecimalU64,
    pub output_tokens: DecimalU64,
    pub cached_tokens: DecimalU64,
    pub reported_responses: DecimalU64,
    pub missing_responses: DecimalU64,
    pub failed_requests: DecimalU64,
}
}

crate::object_wire! {
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EmbeddingUsage {
    pub requests: DecimalU64,
    pub completed: DecimalU64,
    pub failed: DecimalU64,
    pub unknown: DecimalU64,
    pub input_tokens: DecimalU64,
}
}

crate::object_wire! {
/// Last strict10 run only. Tokens with missing coverage are known subtotals.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MemoryStatus {
    pub run_id: RunId,
    pub phase: MemoryPhase,
    pub detail: String,
    pub recent_raw_turns: DecimalU64,
    pub retrieval_sources: Vec<String>,
    pub reference_tokens: DecimalU64,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub main_usage: Option<MemoryUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub summary_usage: Option<MemoryUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub embedding_usage: Option<EmbeddingUsage>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub total_usage: Option<MemoryUsage>,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Position {
    pub engine_epoch: EngineEpoch,
    pub subscription_id: SubscriptionId,
    pub event_seq: DecimalU64,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Draft {
    pub draft_revision: DecimalU64,
    pub text: String,
    pub attachment_ids: Vec<AttachmentId>,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configuration {
    pub configuration_revision: DecimalU64,
    pub provider: String,
    pub model: String,
    pub effort: String,
    #[serde(default, skip_serializing_if = "HistoryMode::is_legacy")]
    pub history_mode: HistoryMode,
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Pending,
    Implementing,
    ReviewPending,
    RetryPending,
    Completed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceState {
    Pending,
    Passed,
    RecheckRequired,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Acceptance {
    pub criterion: String,
    pub state: AcceptanceState,
    pub evidence: String,
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BlockerKind {
    Cancelled,
    Failed,
    Interrupted,
    OutcomeUnknown,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TaskBlocker {
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub kind: BlockerKind,
    pub detail: String,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub task_id: TaskId,
    pub title: String,
    pub state: TaskState,
    pub acceptance: Vec<Acceptance>,
    pub blockers: Vec<TaskBlocker>,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Run {
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub state: RunState,
    pub task_ids: Vec<TaskId>,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Child {
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub parent_run_id: RunId,
    pub state: RunState,
    pub task_ids: Vec<TaskId>,
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalDecision {
    Allow,
    Deny,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PendingApprovalState {
    Pending,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalDisplay {
    pub title: String,
    pub description: String,
    pub choices: Vec<ApprovalDecision>,
}
}

crate::object_wire! {
/// hashとscopeは上位層で照合する表示用値で、任意toolをdispatchする入力ではない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PendingApproval {
    pub approval_id: ApprovalId,
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub operation_id: OperationId,
    pub operation: String,
    pub scope: String,
    pub payload_hash: String,
    pub policy_revision: DecimalU64,
    pub expires_at_unix_ms: DecimalU64,
    pub state: PendingApprovalState,
    pub display: ApprovalDisplay,
}
}

crate::object_wire! {
/// 全一覧が必須。空一覧だけが「このsnapshotでは該当なし」を表す。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    pub snapshot_id: SnapshotId,
    pub history_start_cursor: HistoryCursor,
    pub session_id: SessionId,
    pub summary: String,
    pub session_revision: DecimalU64,
    pub content_revision: DecimalU64,
    pub plan_revision: DecimalU64,
    pub policy_revision: DecimalU64,
    pub position: Position,
    pub draft: Draft,
    pub configuration: Configuration,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub memory: Option<MemoryStatus>,
    pub role_bindings: Vec<crate::role_bindings::RoleBinding>,
    pub role_catalog: Vec<crate::role_bindings::RoleDescriptor>,
    pub tasks: Vec<Task>,
    pub runs: Vec<Run>,
    pub children: Vec<Child>,
    pub child_attempt_count: DecimalU64,
    pub unresolved_approvals: Vec<PendingApproval>,
}
}
