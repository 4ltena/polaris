//! P1値を共用する保存marker、付随状態、要求結果と明示的な保存エラー。

use crate::conversation_state::RawEventV2;
use polaris_desktop_protocol::{
    ids::*,
    snapshot::{ApprovalDecision, Configuration, Draft, PendingApproval, Run, Task},
};
use serde::{Deserialize, Serialize};

pub type StoreResult<T> = Result<T, StoreError>;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("保存I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("保存形式: {0}")]
    Json(#[from] serde_json::Error),
    #[error("不正な公開状態: {0}")]
    Corrupt(&'static str),
    #[error("未対応の保存版: {0}")]
    UnsupportedVersion(u32),
    #[error("このOSの原子的置換・directory sync・私用領域は未対応")]
    UnsupportedPlatform,
    #[error("別writerが所有しています")]
    Busy,
    #[error("公開状態の再照合が必要です")]
    RecoveryRequired,
    #[error("削除済みsessionです")]
    Deleted,
    #[error("対象session/projectが一致しません")]
    TargetMismatch,
    #[error("未対応の変更methodです")]
    UnsupportedMethod,
    #[error("期待版が一致しません: {0}")]
    CasConflict(&'static str),
    #[error("要求IDが異なる内容に再利用されました")]
    RequestConflict,
    #[error("run/attempt/operation/結果が競合しています")]
    RunConflict,
    #[error("対象が存在しません")]
    NotFound,
    #[error("値の範囲超過")]
    Overflow,
}

impl From<polaris_desktop_protocol::ids::ValueError> for StoreError {
    fn from(_: polaris_desktop_protocol::ids::ValueError) -> Self {
        Self::Overflow
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Marker {
    pub schema_version: u32,
    pub session_id: SessionId,
    pub project_id: ProjectId,
    pub epoch: DecimalU64,
    pub session_revision: DecimalU64,
    pub content_revision: DecimalU64,
    pub raw_offset: DecimalU64,
    pub raw_hash: String,
    pub sidecar_offset: DecimalU64,
    pub sidecar_hash: String,
    pub deleted: bool,
}

#[derive(Debug, Clone)]
pub struct InitialState {
    pub draft: Draft,
    pub configuration: Configuration,
    pub policy_revision: DecimalU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Operation {
    pub operation_id: OperationId,
    pub result_id: Option<ResultId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunRecord {
    pub run: Run,
    pub configuration: Configuration,
    /// Accepted choices and past observations only; never runtime authorization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_bindings: Option<super::SavedRoleBindings>,
    pub policy_revision: DecimalU64,
    /// 送信時の添付IDも原文と同じ世代に保持する。実ファイル解決は行わない。
    pub input: Draft,
    pub operations: Vec<Operation>,
    pub result_id: Option<ResultId>,
    /// None means no usage observation has been published, not zero consumption.
    #[serde(default)]
    pub usage: Option<polaris_provider::UsageReport>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory: Option<polaris_desktop_protocol::snapshot::MemoryStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_resources: Option<super::MemoryResources>,
    /// Frozen at acceptance, replaced atomically with the terminal checkpoint.
    #[serde(default)]
    pub workflow: Option<super::SavedWorkflow>,
    /// Some received output could not be retained; never report a complete transcript.
    #[serde(default)]
    pub history_gap: Option<HistoryGap>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HistoryGap {
    OutputRejected,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestResult {
    SourceApplyResolved {
        approval_id: ApprovalId,
    },
    ApprovalResolved {
        approval_id: ApprovalId,
    },
    DraftUpdated {
        draft_revision: DecimalU64,
    },
    Configured {
        configuration_revision: DecimalU64,
        configuration: Configuration,
    },
    CancelRequested {
        run_id: RunId,
        attempt_id: AttemptId,
    },
    RunAccepted {
        run_id: RunId,
        attempt_id: AttemptId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestRecord {
    pub client_id: ClientId,
    pub request_id: RequestId,
    pub request_hash: String,
    pub accepted_revision: DecimalU64,
    pub result: RequestResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sidecar {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub conversation_memory: Option<super::SavedMemory>,
    /// Controller-only evidence; never an RPC/model execution capability.
    #[serde(default)]
    pub source_applies: Vec<super::SavedSourceApply>,
    #[serde(default)]
    pub children: Vec<super::SavedChild>,
    pub session_revision: DecimalU64,
    pub draft: Draft,
    pub configuration: Configuration,
    /// Accepted choices and past observations only; never runtime authorization.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role_bindings: Option<super::SavedRoleBindings>,
    pub policy_revision: DecimalU64,
    pub tasks: Vec<Task>,
    pub runs: Vec<RunRecord>,
    pub requests: Vec<RequestRecord>,
    pub unresolved_approvals: Vec<PendingApproval>,
    #[serde(default)]
    pub approval_records: Vec<ApprovalRecord>,
    #[serde(default)]
    pub workflow: Option<super::SavedWorkflow>,
}

#[derive(Debug, Clone)]
pub struct Published {
    pub marker: Marker,
    pub state: Sidecar,
    pub raw: Vec<RawEventV2>,
}

/// 既存受理でも最新run状態を返し、不存在とoutcome_unknownを区別する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Acceptance {
    pub record: RequestRecord,
    pub run: Option<RunRecord>,
}

/// AlreadyRecordedは実行許可ではない。上位層は同じ操作を再dispatchしない。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntentReceipt {
    NewlyPublished,
    AlreadyRecorded,
}

/// IDと束縛を失効後も保持する。回答の履歴と実行権を分離する。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalRecord {
    pub pending: PendingApproval,
    pub decision: Option<ApprovalDecision>,
    pub consumed: bool,
    pub invalidated: bool,
}
