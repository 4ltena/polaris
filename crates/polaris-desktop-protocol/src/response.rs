//! 成功resultと9種のエラーを排他的に表し、要求IDとmethodへの対応を検査する。

pub use crate::source_apply::{
    SourceApplyEntry, SourceApplyListResult, SourceApplyPageResult, SourceApplySummary,
    SourceApplyVersion,
};
use crate::{
    ProtocolVersion,
    codec::StrictValue,
    ids::*,
    request::{Method, Request, RunTarget},
    snapshot::*,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    UnsupportedVersion,
    InvalidRequest,
    NotFound,
    PermissionDenied,
    RevisionConflict,
    SessionBusy,
    CapabilityUnavailable,
    ReferenceUnavailable,
    StorageFailed,
    RecoveryRequired,
}

crate::object_wire! {
/// OS/providerの生エラーからの自動変換は提供しない。messageは呼出側が用意する表示文。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolError {
    pub code: ErrorCode,
    pub message: String,
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    LocalModels,
    WorkspaceRead,
    AttachmentRead,
    RoleConfigure,
    SessionRead,
    SessionManage,
    HistoryRead,
    DraftUpdate,
    SessionConfigure,
    WorkflowConfigure,
    RunStart,
    RunCancel,
    InputCancel,
    SubagentControl,
    ApprovalResolve,
    SourceApplyRead,
    SourceApplyResolve,
    RequestStatus,
    Shutdown,
    McpSettings,
    RunDetailsRead,
    /// Only the read-only home helper offers this, never an owner.
    HomeBrowse,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    pub frame_bytes: DecimalU64,
    pub subscription_events: DecimalU64,
    pub subscription_bytes: DecimalU64,
    pub text_batch_bytes: DecimalU64,
    pub text_batch_ms: DecimalU64,
}
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            frame_bytes: DecimalU64::new(1_048_576),
            subscription_events: DecimalU64::new(256),
            subscription_bytes: DecimalU64::new(4_194_304),
            text_batch_bytes: DecimalU64::new(32_768),
            text_batch_ms: DecimalU64::new(50),
        }
    }
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Hello {
    pub protocol_version: ProtocolVersion,
    pub engine_epoch: EngineEpoch,
    pub capabilities: Vec<Capability>,
    pub limits: Limits,
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    Tool,
    System,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryMessage {
    pub message_id: MessageId,
    pub role: MessageRole,
    pub text: String,
    pub saved_byte_offset: DecimalU64,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryPageResult {
    pub snapshot_id: SnapshotId,
    pub session_revision: DecimalU64,
    pub messages: Vec<HistoryMessage>,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::present"
    )]
    pub next_cursor: Option<HistoryCursor>,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftUpdated {
    pub session_revision: DecimalU64,
    pub draft_revision: DecimalU64,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Configured {
    pub session_revision: DecimalU64,
    pub configuration: Configuration,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowConfigured {
    pub session_revision: DecimalU64,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub workflow: Option<WorkflowStatus>,
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Queued {
    Queued,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalUpdated {
    pub session_revision: DecimalU64,
    pub goal_revision: DecimalU64,
}
}

crate::object_wire! {
/// Durable on receipt only. The run owner records separately when a run
/// actually applied it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionReceived {
    pub session_revision: DecimalU64,
    pub instruction_id: String,
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputAdmissionState {
    Reserved,
    Classifying,
    Decided,
    DestinationReady,
    InputAppended,
    RunQueued,
    Queued,
    Running,
    Completed,
    Failed,
    Cancelled,
    OutcomeUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputRouteReason {
    ExplicitCurrent,
    ExplicitNew,
    SameContext,
    DistinctContext,
    Ambiguous,
    ClassifierUnavailable,
    ClassifierInvalid,
    Cancelled,
    ApprovalReply,
    ContextChanged,
    CrashRecovery,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputSubmitted {
    pub manager_revision: DecimalU64,
    pub session_revision: DecimalU64,
    pub admission_id: AdmissionId,
    pub input_hash: String,
    pub route: crate::request::InputRoute,
    pub state: InputAdmissionState,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub route_reason: Option<InputRouteReason>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub destination_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub run: Option<RunTarget>,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputStatusResult {
    pub manager_revision: DecimalU64,
    pub admission_id: AdmissionId,
    pub source_session_id: SessionId,
    pub state: InputAdmissionState,
    pub session_revision: DecimalU64,
    pub route: crate::request::InputRoute,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub route_reason: Option<InputRouteReason>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub destination_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub run: Option<RunTarget>,
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentWaitStateV2 {
    Unchanged,
    Updated,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentWaitResult {
    pub root_run_id: RunId,
    pub root_attempt_id: AttemptId,
    pub revision: DecimalU64,
    pub cursor: DecimalU64,
    pub state: SubagentWaitStateV2,
    pub child_total: DecimalU64,
    pub child_omitted: bool,
    #[serde(deserialize_with = "bounded_subagent_attempts")]
    pub children: Vec<crate::orchestration::SubagentAttemptSummaryV2>,
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentCancelDispositionV2 {
    Requested,
    AlreadyRequested,
    AlreadyTerminal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubagentRuntimeEffectV2 {
    Unavailable,
    Requested,
    OutcomeUnknown,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentCancelResult {
    pub root_run_id: RunId,
    pub root_attempt_id: AttemptId,
    pub logical_child_id: LogicalChildId,
    pub child_attempt_id: ChildAttemptId,
    pub revision: DecimalU64,
    pub cursor: DecimalU64,
    pub disposition: SubagentCancelDispositionV2,
    pub state: crate::orchestration::ChildControlStateV2,
    pub runtime_effect: SubagentRuntimeEffectV2,
}
}

fn bounded_subagent_attempts<'de, D>(
    deserializer: D,
) -> Result<Vec<crate::orchestration::SubagentAttemptSummaryV2>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<crate::orchestration::SubagentAttemptSummaryV2>::deserialize(deserializer)?;
    if values.len() > 32
        || values.iter().any(|value| {
            value.logical_child_id.is_empty()
                || value.logical_child_id.len() > 128
                || value.attempt_id.is_empty()
                || value.attempt_id.len() > 128
                || value.display_name.is_empty()
                || value.display_name.len() > 128
                || value.role.is_empty()
                || value.role.len() > 128
                || value.activity.as_ref().is_some_and(|text| text.len() > 512)
        })
    {
        return Err(de::Error::custom(
            "subagent wait projection exceeds its bound",
        ));
    }
    Ok(values)
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionListEntry {
    pub session_id: SessionId,
    pub kind: crate::orchestration::ManagedSessionKindV2,
    pub session_revision: DecimalU64,
    pub goal_revision: DecimalU64,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub source_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub active_run: Option<RunTarget>,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionListResult {
    pub manager_revision: DecimalU64,
    pub session_total: DecimalU64,
    pub session_omitted: bool,
    #[serde(deserialize_with = "bounded_session_entries")]
    pub sessions: Vec<SessionListEntry>,
}
}

fn bounded_session_entries<'de, D>(deserializer: D) -> Result<Vec<SessionListEntry>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<SessionListEntry>::deserialize(deserializer)?;
    if values.len() > 2 {
        return Err(de::Error::custom("session entries exceed 2"));
    }
    Ok(values)
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunAccepted {
    pub session_revision: DecimalU64,
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub state: Queued,
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelStatus {
    CancelRequested,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelAccepted {
    pub status: CancelStatus,
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolved {
    Resolved,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalResolved {
    pub approval_id: ApprovalId,
    pub state: Resolved,
    pub decision: ApprovalDecision,
}
}

crate::object_wire! {
/// 確定結果は保存先の参照で表す。不存在や結果不明を再実行の許可には変換しない。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum RequestStatusResult {
    NotFound {},
    Accepted {
        session_revision: DecimalU64,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "crate::present"
        )]
        run: Option<RunTarget>,
    },
    Completed {
        session_revision: DecimalU64,
        result_id: ResultId,
    },
    OutcomeUnknown {
        session_revision: DecimalU64,
        run_id: RunId,
        attempt_id: AttemptId,
    },
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ShutdownState {
    Draining,
    Ready,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownResult {
    pub engine_epoch: EngineEpoch,
    pub state: ShutdownState,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", deny_unknown_fields)]
pub enum SuccessResult {
    #[serde(rename = "local.models")]
    LocalModels(crate::local_models::LocalModelsResult),
    #[serde(rename = "workspace.read")]
    WorkspaceRead(crate::workspace_view::WorkspaceView),
    #[serde(rename = "attachment.read")]
    AttachmentRead(crate::workspace_view::AttachmentView),
    #[serde(rename = "session.roles.configure")]
    SessionRolesConfigure(crate::role_bindings::RoleBindingsConfigured),
    #[serde(rename = "hello")]
    Hello(Hello),
    #[serde(rename = "session.open")]
    SessionOpen(Box<Snapshot>),
    #[serde(rename = "session.list")]
    SessionList(SessionListResult),
    #[serde(rename = "session.select")]
    SessionSelect(Box<Snapshot>),
    #[serde(rename = "input.submit")]
    InputSubmit(InputSubmitted),
    #[serde(rename = "input.status")]
    InputStatus(InputStatusResult),
    #[serde(rename = "input.cancel")]
    InputCancel(InputStatusResult),
    #[serde(rename = "subagent.wait")]
    SubagentWait(SubagentWaitResult),
    #[serde(rename = "subagent.cancel")]
    SubagentCancel(SubagentCancelResult),
    #[serde(rename = "session.snapshot")]
    SessionSnapshot(Box<Snapshot>),
    #[serde(rename = "session.subscribe")]
    SessionSubscribe(Box<Snapshot>),
    #[serde(rename = "history.page")]
    HistoryPage(HistoryPageResult),
    #[serde(rename = "draft.update")]
    DraftUpdate(DraftUpdated),
    #[serde(rename = "session.configure")]
    SessionConfigure(Configured),
    #[serde(rename = "workflow.configure")]
    WorkflowConfigure(WorkflowConfigured),
    #[serde(rename = "goal.update")]
    GoalUpdate(GoalUpdated),
    #[serde(rename = "instruction.send")]
    InstructionSend(InstructionReceived),
    #[serde(rename = "run.start")]
    RunStart(RunAccepted),
    #[serde(rename = "run.cancel")]
    RunCancel(CancelAccepted),
    #[serde(rename = "approval.resolve")]
    ApprovalResolve(ApprovalResolved),
    #[serde(rename = "source_apply.list")]
    SourceApplyList(SourceApplyListResult),
    #[serde(rename = "source_apply.page")]
    SourceApplyPage(SourceApplyPageResult),
    #[serde(rename = "source_apply.resolve")]
    SourceApplyResolve(ApprovalResolved),
    #[serde(rename = "request.status")]
    RequestStatus(RequestStatusResult),
    #[serde(rename = "shutdown.request")]
    ShutdownRequest(ShutdownResult),
    #[serde(rename = "mcp.registry.read")]
    McpRegistryRead(crate::mcp_settings::McpRegistryState),
    #[serde(rename = "mcp.registry.save")]
    McpRegistrySave(crate::mcp_settings::McpRegistryState),
    #[serde(rename = "mcp.secret.set")]
    McpSecretSet(crate::mcp_settings::McpRegistryState),
    #[serde(rename = "mcp.secret.delete")]
    McpSecretDelete(crate::mcp_settings::McpRegistryState),
    #[serde(rename = "mcp.catalog.check")]
    McpCatalogCheck(crate::mcp_settings::McpCatalogChecked),
    #[serde(rename = "mcp.catalog.cancel")]
    McpCatalogCancel(crate::mcp_settings::McpCatalogCancelled),
    #[serde(rename = "mcp.setup.detect")]
    McpSetupDetect(crate::mcp_settings::McpSetupCandidates),
    #[serde(rename = "mcp.setup.register")]
    McpSetupRegister(crate::mcp_settings::McpRegistryState),
    #[serde(rename = "run.details")]
    RunDetails(crate::run_details::RunDetailsPage),
    #[serde(rename = "browse.list")]
    BrowseList(crate::browse::BrowseListPage),
    #[serde(rename = "browse.read")]
    BrowseRead(crate::browse::BrowseReadResult),
}
}

impl SuccessResult {
    pub fn method(&self) -> Method {
        match self {
            Self::Hello(_) => Method::Hello,
            Self::LocalModels(_) => Method::LocalModels,
            Self::WorkspaceRead(_) => Method::WorkspaceRead,
            Self::AttachmentRead(_) => Method::AttachmentRead,
            Self::SessionRolesConfigure(_) => Method::SessionRolesConfigure,
            Self::SessionOpen(_) => Method::SessionOpen,
            Self::SessionList(_) => Method::SessionList,
            Self::SessionSelect(_) => Method::SessionSelect,
            Self::InputSubmit(_) => Method::InputSubmit,
            Self::InputStatus(_) => Method::InputStatus,
            Self::InputCancel(_) => Method::InputCancel,
            Self::SubagentWait(_) => Method::SubagentWait,
            Self::SubagentCancel(_) => Method::SubagentCancel,
            Self::SessionSnapshot(_) => Method::SessionSnapshot,
            Self::SessionSubscribe(_) => Method::SessionSubscribe,
            Self::HistoryPage(_) => Method::HistoryPage,
            Self::DraftUpdate(_) => Method::DraftUpdate,
            Self::SessionConfigure(_) => Method::SessionConfigure,
            Self::WorkflowConfigure(_) => Method::WorkflowConfigure,
            Self::GoalUpdate(_) => Method::GoalUpdate,
            Self::InstructionSend(_) => Method::InstructionSend,
            Self::RunStart(_) => Method::RunStart,
            Self::RunCancel(_) => Method::RunCancel,
            Self::ApprovalResolve(_) => Method::ApprovalResolve,
            Self::SourceApplyList(_) => Method::SourceApplyList,
            Self::SourceApplyPage(_) => Method::SourceApplyPage,
            Self::SourceApplyResolve(_) => Method::SourceApplyResolve,
            Self::RequestStatus(_) => Method::RequestStatus,
            Self::ShutdownRequest(_) => Method::ShutdownRequest,
            Self::McpRegistryRead(_) => Method::McpRegistryRead,
            Self::McpRegistrySave(_) => Method::McpRegistrySave,
            Self::McpSecretSet(_) => Method::McpSecretSet,
            Self::McpSecretDelete(_) => Method::McpSecretDelete,
            Self::McpCatalogCheck(_) => Method::McpCatalogCheck,
            Self::McpCatalogCancel(_) => Method::McpCatalogCancel,
            Self::McpSetupDetect(_) => Method::McpSetupDetect,
            Self::McpSetupRegister(_) => Method::McpSetupRegister,
            Self::RunDetails(_) => Method::RunDetails,
            Self::BrowseList(_) => Method::BrowseList,
            Self::BrowseRead(_) => Method::BrowseRead,
        }
    }

    fn requires_v2(&self) -> bool {
        match self {
            Self::SessionList(_)
            | Self::SessionSelect(_)
            | Self::InputSubmit(_)
            | Self::InputStatus(_)
            | Self::InputCancel(_) => true,
            Self::SubagentWait(_) | Self::SubagentCancel(_) => true,
            Self::SessionOpen(snapshot)
            | Self::SessionSnapshot(snapshot)
            | Self::SessionSubscribe(snapshot) => snapshot.orchestration.is_some(),
            _ => false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response {
    pub protocol_version: ProtocolVersion,
    pub client_id: ClientId,
    pub request_id: RequestId,
    pub outcome: Result<SuccessResult, ProtocolError>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrelationError {
    ProtocolVersion,
    ClientId,
    RequestId,
    Method,
}

impl Response {
    /// resultのtypeは元要求のmethodと一致する。失敗応答にも相関IDを要求する。
    pub fn validate_for(&self, request: &Request) -> Result<(), CorrelationError> {
        if self.protocol_version != request.protocol_version {
            return Err(CorrelationError::ProtocolVersion);
        }
        if self.client_id != request.client_id {
            return Err(CorrelationError::ClientId);
        }
        if self.request_id != request.request_id {
            return Err(CorrelationError::RequestId);
        }
        if request.protocol_version == ProtocolVersion::V1
            && self.outcome.as_ref().is_ok_and(SuccessResult::requires_v2)
        {
            return Err(CorrelationError::ProtocolVersion);
        }
        if let Ok(result) = &self.outcome
            && result.method() != request.body.method()
        {
            return Err(CorrelationError::Method);
        }
        if let (
            Ok(SuccessResult::LocalModels(result)),
            crate::request::RequestBody::LocalModels(_, params),
        ) = (&self.outcome, &request.body)
            && (result.provider != params.provider || result.endpoint != params.endpoint)
        {
            return Err(CorrelationError::Method);
        }
        if let (
            Ok(SuccessResult::BrowseList(page)),
            crate::request::RequestBody::BrowseList(params),
        ) = (&self.outcome, &request.body)
            && (page.directory != params.path
                || page.entries.iter().any(|entry| {
                    entry.name.is_empty()
                        || entry.name.contains('/')
                        || entry.path
                            != if page.directory.is_empty() {
                                entry.name.clone()
                            } else {
                                format!("{}/{}", page.directory, entry.name)
                            }
                }))
        {
            return Err(CorrelationError::Method);
        }
        if let (
            Ok(SuccessResult::BrowseRead(result)),
            crate::request::RequestBody::BrowseRead(params),
        ) = (&self.outcome, &request.body)
            && (result.path != params.path
                || result.device != params.device
                || result.inode != params.inode)
        {
            return Err(CorrelationError::Method);
        }
        if let (
            Ok(SuccessResult::RunDetails(page)),
            crate::request::RequestBody::RunDetails(_, params),
        ) = (&self.outcome, &request.body)
            && (page.run_id != params.run_id
                || page.attempt_id != params.attempt_id
                || params
                    .snapshot_id
                    .as_ref()
                    .is_some_and(|id| id != &page.snapshot_id))
        {
            return Err(CorrelationError::Method);
        }
        if let (
            Ok(SuccessResult::SubagentWait(result)),
            crate::request::RequestBody::SubagentWait(_, params),
        ) = (&self.outcome, &request.body)
            && (result.root_run_id != params.root_run_id
                || result.root_attempt_id != params.root_attempt_id
                || result.revision != params.expected_revision
                || result.child_total.get() < result.children.len() as u64
                || result.child_omitted
                    != (result.child_total.get() != result.children.len() as u64)
                || matches!(result.state, SubagentWaitStateV2::Unchanged)
                    && !result.children.is_empty()
                || result.children.iter().any(|child| {
                    child.root_run_id != result.root_run_id
                        || child.root_attempt_id != result.root_attempt_id
                        || child.control_revision != result.revision
                }))
        {
            return Err(CorrelationError::Method);
        }
        if let (
            Ok(SuccessResult::SubagentCancel(result)),
            crate::request::RequestBody::SubagentCancel(_, params),
        ) = (&self.outcome, &request.body)
            && (result.root_run_id != params.root_run_id
                || result.root_attempt_id != params.root_attempt_id
                || result.logical_child_id != params.logical_child_id
                || result.child_attempt_id != params.child_attempt_id
                || match result.disposition {
                    SubagentCancelDispositionV2::Requested => {
                        match params.expected_revision.checked_add(1) {
                            Ok(expected) => result.revision != expected,
                            Err(_) => true,
                        }
                    }
                    SubagentCancelDispositionV2::AlreadyRequested
                    | SubagentCancelDispositionV2::AlreadyTerminal => {
                        result.revision != params.expected_revision
                    }
                })
        {
            return Err(CorrelationError::Method);
        }
        if let (
            Ok(
                SuccessResult::McpCatalogCheck(crate::mcp_settings::McpCatalogChecked {
                    server_id,
                    ..
                })
                | SuccessResult::McpCatalogCancel(crate::mcp_settings::McpCatalogCancelled {
                    server_id,
                    ..
                }),
            ),
            crate::request::RequestBody::McpCatalogCheck(_, params)
            | crate::request::RequestBody::McpCatalogCancel(_, params),
        ) = (&self.outcome, &request.body)
            && server_id != &params.server_id
        {
            return Err(CorrelationError::Method);
        }
        Ok(())
    }
    pub fn for_request(
        request: &Request,
        outcome: Result<SuccessResult, ProtocolError>,
    ) -> Result<Self, CorrelationError> {
        let response = Self {
            protocol_version: request.protocol_version,
            client_id: request.client_id.clone(),
            request_id: request.request_id.clone(),
            outcome,
        };
        response.validate_for(request)?;
        Ok(response)
    }
}

#[derive(Serialize, Deserialize)]
enum ResponseKind {
    #[serde(rename = "response")]
    Response,
}

crate::object_wire! {
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    protocol_version: ProtocolVersion,
    kind: ResponseKind,
    client_id: ClientId,
    request_id: RequestId,
    #[serde(default, deserialize_with = "crate::present")]
    result: Option<SuccessResult>,
    #[serde(default, deserialize_with = "crate::present")]
    error: Option<ProtocolError>,
}
}

impl<'de> Deserialize<'de> for Response {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let wire: Wire =
            serde_json::from_value(StrictValue::deserialize(d)?.0).map_err(de::Error::custom)?;
        let ResponseKind::Response = wire.kind;
        let outcome = match (wire.result, wire.error) {
            (Some(result), None) => {
                if wire.protocol_version == ProtocolVersion::V1 && result.requires_v2() {
                    return Err(de::Error::custom("protocol v2 result on v1 frame"));
                }
                Ok(result)
            }
            (None, Some(error)) => Err(error),
            _ => return Err(de::Error::custom("exactly one of result/error is required")),
        };
        Ok(Self {
            protocol_version: wire.protocol_version,
            client_id: wire.client_id,
            request_id: wire.request_id,
            outcome,
        })
    }
}

impl Serialize for Response {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut wire = s.serialize_struct("Response", 5)?;
        wire.serialize_field("protocol_version", &self.protocol_version)?;
        wire.serialize_field("kind", &ResponseKind::Response)?;
        wire.serialize_field("client_id", &self.client_id)?;
        wire.serialize_field("request_id", &self.request_id)?;
        match &self.outcome {
            Ok(result) => wire.serialize_field("result", result)?,
            Err(error) => wire.serialize_field("error", error)?,
        }
        wire.end()
    }
}
