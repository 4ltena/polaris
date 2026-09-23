//! M1の要求を専用paramsで表し、methodとsession対象の組合せをwire境界で検査する。

pub use crate::local_models::LocalModels;
use crate::{
    ProtocolVersion,
    codec::StrictValue,
    ids::*,
    snapshot::{ApprovalDecision, Position},
};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Method {
    #[serde(rename = "local.models")]
    LocalModels,
    #[serde(rename = "workspace.read")]
    WorkspaceRead,
    #[serde(rename = "attachment.read")]
    AttachmentRead,
    #[serde(rename = "hello")]
    Hello,
    #[serde(rename = "session.open")]
    SessionOpen,
    #[serde(rename = "session.list")]
    SessionList,
    #[serde(rename = "session.select")]
    SessionSelect,
    #[serde(rename = "input.submit")]
    InputSubmit,
    #[serde(rename = "input.status")]
    InputStatus,
    #[serde(rename = "input.cancel")]
    InputCancel,
    #[serde(rename = "subagent.wait")]
    SubagentWait,
    #[serde(rename = "subagent.cancel")]
    SubagentCancel,
    #[serde(rename = "session.roles.configure")]
    SessionRolesConfigure,
    #[serde(rename = "session.snapshot")]
    SessionSnapshot,
    #[serde(rename = "session.subscribe")]
    SessionSubscribe,
    #[serde(rename = "history.page")]
    HistoryPage,
    #[serde(rename = "draft.update")]
    DraftUpdate,
    #[serde(rename = "session.configure")]
    SessionConfigure,
    #[serde(rename = "workflow.configure")]
    WorkflowConfigure,
    #[serde(rename = "goal.update")]
    GoalUpdate,
    #[serde(rename = "instruction.send")]
    InstructionSend,
    #[serde(rename = "run.start")]
    RunStart,
    #[serde(rename = "run.cancel")]
    RunCancel,
    #[serde(rename = "approval.resolve")]
    ApprovalResolve,
    #[serde(rename = "source_apply.list")]
    SourceApplyList,
    #[serde(rename = "source_apply.page")]
    SourceApplyPage,
    #[serde(rename = "source_apply.resolve")]
    SourceApplyResolve,
    #[serde(rename = "request.status")]
    RequestStatus,
    #[serde(rename = "shutdown.request")]
    ShutdownRequest,
    #[serde(rename = "mcp.registry.read")]
    McpRegistryRead,
    #[serde(rename = "mcp.registry.save")]
    McpRegistrySave,
    #[serde(rename = "mcp.secret.set")]
    McpSecretSet,
    #[serde(rename = "mcp.secret.delete")]
    McpSecretDelete,
    #[serde(rename = "mcp.catalog.check")]
    McpCatalogCheck,
    #[serde(rename = "mcp.catalog.cancel")]
    McpCatalogCancel,
    #[serde(rename = "mcp.setup.detect")]
    McpSetupDetect,
    #[serde(rename = "mcp.setup.register")]
    McpSetupRegister,
    #[serde(rename = "run.details")]
    RunDetails,
    #[serde(rename = "browse.list")]
    BrowseList,
    #[serde(rename = "browse.read")]
    BrowseRead,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Empty {}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Subscribe {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::present"
    )]
    pub resume: Option<Position>,
}
}

/// ページ上限は1..=256件。snapshot/cursorの失効・所属はengineが照合する。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct PageLimit(u16);
impl PageLimit {
    pub fn new(value: u16) -> Option<Self> {
        (1..=256).contains(&value).then_some(Self(value))
    }
    pub fn get(self) -> u16 {
        self.0
    }
}
impl<'de> Deserialize<'de> for PageLimit {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(u16::deserialize(d)?)
            .ok_or_else(|| de::Error::custom("page limit must be 1..=256"))
    }
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryPage {
    pub snapshot_id: SnapshotId,
    pub cursor: HistoryCursor,
    pub limit: PageLimit,
}
}

/// M6-3: what to do with the session's goal. A goal is only a reason to
/// consider another run; it never authorizes one and never occupies the input.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
pub enum GoalOperation {
    Set {
        text: String,
    },
    Clear,
    /// Suppresses automatic re-runs until an explicit resume.
    Stop,
    Resume,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GoalUpdate {
    #[serde(flatten)]
    pub operation: GoalOperation,
}
}

crate::object_wire! {
/// An instruction that arrived while a run was already going. Receiving it is
/// durable; applying it is a separate step the run owner records.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InstructionSend {
    pub text: String,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DraftUpdate {
    pub expected_draft_revision: DecimalU64,
    pub text: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub attachment_ids: Vec<AttachmentId>,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfigure {
    pub expected_configuration_revision: DecimalU64,
    pub provider: String,
    pub model: String,
    pub effort: String,
    #[serde(default, skip_serializing_if = "crate::snapshot::HistoryMode::is_legacy")]
    pub history_mode: crate::snapshot::HistoryMode,
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowMode {
    Manual,
    Adaptive,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkflowPhase {
    General,
    Brainstorm,
    Specify,
    Implement,
    Review,
    Verify,
    Deliver,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowConfigure {
    pub expected_session_revision: DecimalU64,
    pub enabled: bool,
    pub mode: WorkflowMode,
    pub initial_phase: WorkflowPhase,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunStart {
    pub expected_draft_revision: DecimalU64,
    pub expected_configuration_revision: DecimalU64,
    pub expected_policy_revision: DecimalU64,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub mcp_prompt: Option<McpPromptChoice>,
}
}

crate::object_wire! {
/// A host-selected next-run input, never a model tool or execution grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpPromptChoice {
    pub server: String,
    pub prompt: String,
    pub arguments: serde_json::Map<String, serde_json::Value>,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunTarget {
    pub run_id: RunId,
    pub attempt_id: AttemptId,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalResolve {
    pub approval_id: ApprovalId,
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub policy_revision: DecimalU64,
    pub decision: ApprovalDecision,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceApplyList {
    pub expected_session_revision: DecimalU64,
    pub offset: DecimalU64,
    pub limit: PageLimit,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceApplyPage {
    pub approval_id: ApprovalId,
    pub payload_hash: String,
    pub expected_session_revision: DecimalU64,
    pub offset: DecimalU64,
    pub limit: PageLimit,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceApplyResolve {
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub approval_id: ApprovalId,
    pub payload_hash: String,
    pub expected_session_revision: DecimalU64,
    pub expected_policy_revision: DecimalU64,
    pub decision: ApprovalDecision,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RequestStatus {
    pub client_id: ClientId,
    pub request_id: RequestId,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShutdownRequest {
    pub engine_epoch: EngineEpoch,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkspaceRead { pub selected_path: String }
}
crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AttachmentRead { pub path: String }
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum InputSubmit {
    Text {
        expected_manager_revision: DecimalU64,
        expected_session_revision: DecimalU64,
        expected_goal_revision: DecimalU64,
        route: InputRoute,
        #[serde(default, deserialize_with = "bounded_attachments")]
        attachment_ids: Vec<AttachmentId>,
        #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
        admission_id: Option<AdmissionId>,
        text: String,
    },
    Reference {
        expected_manager_revision: DecimalU64,
        expected_session_revision: DecimalU64,
        expected_goal_revision: DecimalU64,
        route: InputRoute,
        #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
        admission_id: Option<AdmissionId>,
        input_ref: ResultId,
    },
}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputRoute {
    Current,
    New,
    Auto,
}

impl InputSubmit {
    pub fn validate(&self) -> Result<(), &'static str> {
        if let Self::Text {
            text,
            attachment_ids,
            ..
        } = self
            && (text.is_empty() || text.len() > 32 * 1024 || attachment_ids.len() > 16)
        {
            return Err("input text/attachments exceed their bound");
        }
        Ok(())
    }
}

fn bounded_attachments<'de, D>(deserializer: D) -> Result<Vec<AttachmentId>, D::Error>
where
    D: Deserializer<'de>,
{
    let values = Vec::<AttachmentId>::deserialize(deserializer)?;
    if values.len() > 16 {
        return Err(de::Error::custom("attachment IDs exceed 16"));
    }
    Ok(values)
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputStatus { pub admission_id: AdmissionId }
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InputCancel {
    pub expected_manager_revision: DecimalU64,
    pub admission_id: AdmissionId,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentWait {
    pub root_run_id: RunId,
    pub root_attempt_id: AttemptId,
    pub expected_revision: DecimalU64,
    #[serde(default, skip_serializing_if = "Option::is_none", deserialize_with = "crate::present")]
    pub after_cursor: Option<DecimalU64>,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentCancel {
    pub root_run_id: RunId,
    pub root_attempt_id: AttemptId,
    pub logical_child_id: LogicalChildId,
    pub child_attempt_id: ChildAttemptId,
    pub expected_revision: DecimalU64,
}
}

/// sessionの要否をenumの形で固定し、無関係なsessionの黙殺を防ぐ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestBody {
    LocalModels(SessionId, LocalModels),
    WorkspaceRead(SessionId, WorkspaceRead),
    AttachmentRead(SessionId, AttachmentRead),
    Hello,
    SessionOpen(SessionId),
    SessionList,
    SessionSelect(SessionId),
    InputSubmit(SessionId, InputSubmit),
    InputStatus(SessionId, InputStatus),
    InputCancel(SessionId, InputCancel),
    SubagentWait(SessionId, SubagentWait),
    SubagentCancel(SessionId, SubagentCancel),
    SessionRolesConfigure(SessionId, crate::role_bindings::ConfigureRoleBindings),
    SessionSnapshot(SessionId),
    SessionSubscribe(SessionId, Subscribe),
    HistoryPage(SessionId, HistoryPage),
    DraftUpdate(SessionId, DraftUpdate),
    SessionConfigure(SessionId, SessionConfigure),
    WorkflowConfigure(SessionId, WorkflowConfigure),
    GoalUpdate(SessionId, GoalUpdate),
    InstructionSend(SessionId, InstructionSend),
    RunStart(SessionId, RunStart),
    RunCancel(SessionId, RunTarget),
    ApprovalResolve(SessionId, ApprovalResolve),
    SourceApplyList(SessionId, SourceApplyList),
    SourceApplyPage(SessionId, SourceApplyPage),
    SourceApplyResolve(SessionId, SourceApplyResolve),
    RequestStatus(SessionId, RequestStatus),
    ShutdownRequest(ShutdownRequest),
    McpRegistryRead(SessionId),
    McpRegistrySave(SessionId, crate::mcp_settings::McpRegistrySave),
    McpSecretSet(SessionId, crate::mcp_settings::McpSecretSet),
    McpSecretDelete(SessionId, crate::mcp_settings::McpSecretDelete),
    McpCatalogCheck(SessionId, crate::mcp_settings::McpCatalogTarget),
    McpCatalogCancel(SessionId, crate::mcp_settings::McpCatalogTarget),
    McpSetupDetect(SessionId),
    McpSetupRegister(SessionId, crate::mcp_settings::McpSetupTarget),
    RunDetails(SessionId, crate::run_details::RunDetailsRequest),
    BrowseList(crate::browse::BrowseList),
    BrowseRead(crate::browse::BrowseRead),
}

impl RequestBody {
    pub fn requires_v2(&self) -> bool {
        matches!(
            self,
            Self::SessionList
                | Self::SessionSelect(..)
                | Self::InputSubmit(..)
                | Self::InputStatus(..)
                | Self::InputCancel(..)
                | Self::SubagentWait(..)
                | Self::SubagentCancel(..)
        )
    }

    pub fn method(&self) -> Method {
        match self {
            Self::Hello => Method::Hello,
            Self::LocalModels(..) => Method::LocalModels,
            Self::WorkspaceRead(..) => Method::WorkspaceRead,
            Self::AttachmentRead(..) => Method::AttachmentRead,
            Self::SessionOpen(..) => Method::SessionOpen,
            Self::SessionList => Method::SessionList,
            Self::SessionSelect(..) => Method::SessionSelect,
            Self::InputSubmit(..) => Method::InputSubmit,
            Self::InputStatus(..) => Method::InputStatus,
            Self::InputCancel(..) => Method::InputCancel,
            Self::SubagentWait(..) => Method::SubagentWait,
            Self::SubagentCancel(..) => Method::SubagentCancel,
            Self::SessionRolesConfigure(..) => Method::SessionRolesConfigure,
            Self::SessionSnapshot(..) => Method::SessionSnapshot,
            Self::SessionSubscribe(..) => Method::SessionSubscribe,
            Self::HistoryPage(..) => Method::HistoryPage,
            Self::DraftUpdate(..) => Method::DraftUpdate,
            Self::SessionConfigure(..) => Method::SessionConfigure,
            Self::WorkflowConfigure(..) => Method::WorkflowConfigure,
            Self::GoalUpdate(..) => Method::GoalUpdate,
            Self::InstructionSend(..) => Method::InstructionSend,
            Self::RunStart(..) => Method::RunStart,
            Self::RunCancel(..) => Method::RunCancel,
            Self::ApprovalResolve(..) => Method::ApprovalResolve,
            Self::SourceApplyList(..) => Method::SourceApplyList,
            Self::SourceApplyPage(..) => Method::SourceApplyPage,
            Self::SourceApplyResolve(..) => Method::SourceApplyResolve,
            Self::RequestStatus(..) => Method::RequestStatus,
            Self::ShutdownRequest(..) => Method::ShutdownRequest,
            Self::McpRegistryRead(..) => Method::McpRegistryRead,
            Self::McpRegistrySave(..) => Method::McpRegistrySave,
            Self::McpSecretSet(..) => Method::McpSecretSet,
            Self::McpSecretDelete(..) => Method::McpSecretDelete,
            Self::McpCatalogCheck(..) => Method::McpCatalogCheck,
            Self::McpCatalogCancel(..) => Method::McpCatalogCancel,
            Self::McpSetupDetect(..) => Method::McpSetupDetect,
            Self::McpSetupRegister(..) => Method::McpSetupRegister,
            Self::RunDetails(..) => Method::RunDetails,
            Self::BrowseList(..) => Method::BrowseList,
            Self::BrowseRead(..) => Method::BrowseRead,
        }
    }
    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            Self::Hello
            | Self::SessionList
            | Self::ShutdownRequest(..)
            | Self::BrowseList(..)
            | Self::BrowseRead(..) => None,
            Self::WorkspaceRead(s, _)
            | Self::AttachmentRead(s, _)
            | Self::LocalModels(s, _)
            | Self::SessionOpen(s)
            | Self::SessionSelect(s)
            | Self::InputSubmit(s, _)
            | Self::InputStatus(s, _)
            | Self::InputCancel(s, _)
            | Self::SubagentWait(s, _)
            | Self::SubagentCancel(s, _)
            | Self::SessionRolesConfigure(s, _)
            | Self::SessionSnapshot(s)
            | Self::SessionSubscribe(s, _)
            | Self::HistoryPage(s, _)
            | Self::DraftUpdate(s, _)
            | Self::SessionConfigure(s, _)
            | Self::WorkflowConfigure(s, _)
            | Self::GoalUpdate(s, _)
            | Self::InstructionSend(s, _)
            | Self::RunStart(s, _)
            | Self::RunCancel(s, _)
            | Self::ApprovalResolve(s, _)
            | Self::SourceApplyList(s, _)
            | Self::SourceApplyPage(s, _)
            | Self::SourceApplyResolve(s, _)
            | Self::RequestStatus(s, _)
            | Self::McpRegistryRead(s)
            | Self::McpRegistrySave(s, _)
            | Self::McpSecretSet(s, _)
            | Self::McpSecretDelete(s, _)
            | Self::McpCatalogCheck(s, _)
            | Self::McpCatalogCancel(s, _)
            | Self::McpSetupDetect(s)
            | Self::McpSetupRegister(s, _)
            | Self::RunDetails(s, _) => Some(s),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    pub protocol_version: ProtocolVersion,
    pub client_id: ClientId,
    pub request_id: RequestId,
    pub body: RequestBody,
}

#[derive(Serialize, Deserialize)]
enum RequestKind {
    #[serde(rename = "request")]
    Request,
}

// Valueは重複検査後の非公開の一時表現だけで使い、公開APIには型付きparamsを渡す。
crate::object_wire! {
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    protocol_version: ProtocolVersion,
    kind: RequestKind,
    client_id: ClientId,
    request_id: RequestId,
    method: Method,
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "crate::present"
    )]
    session_id: Option<SessionId>,
    params: serde_json::Value,
}
}

impl<'de> Deserialize<'de> for Request {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let wire: Wire =
            serde_json::from_value(StrictValue::deserialize(d)?.0).map_err(de::Error::custom)?;
        let session = wire.session_id;
        let needs_session = !matches!(
            wire.method,
            Method::Hello
                | Method::SessionList
                | Method::ShutdownRequest
                | Method::BrowseList
                | Method::BrowseRead
        );
        if needs_session != session.is_some() {
            return Err(de::Error::custom("method/session target mismatch"));
        }
        macro_rules! params {
            () => {
                serde_json::from_value(wire.params).map_err(de::Error::custom)?
            };
        }
        macro_rules! scoped {
            ($variant:ident) => {
                RequestBody::$variant(
                    session.ok_or_else(|| de::Error::custom("missing session"))?,
                    params!(),
                )
            };
        }
        let body = match wire.method {
            Method::Hello => {
                let _: Empty = params!();
                RequestBody::Hello
            }
            Method::SessionOpen => {
                let _: Empty = params!();
                RequestBody::SessionOpen(session.unwrap())
            }
            Method::SessionList => {
                let _: Empty = params!();
                RequestBody::SessionList
            }
            Method::SessionSelect => {
                let _: Empty = params!();
                RequestBody::SessionSelect(session.unwrap())
            }
            Method::InputSubmit => {
                let params: InputSubmit = params!();
                params.validate().map_err(de::Error::custom)?;
                RequestBody::InputSubmit(
                    session.ok_or_else(|| de::Error::custom("missing session"))?,
                    params,
                )
            }
            Method::InputStatus => scoped!(InputStatus),
            Method::InputCancel => scoped!(InputCancel),
            Method::SubagentWait => scoped!(SubagentWait),
            Method::SubagentCancel => scoped!(SubagentCancel),
            Method::SessionRolesConfigure => scoped!(SessionRolesConfigure),
            Method::GoalUpdate => scoped!(GoalUpdate),
            Method::InstructionSend => scoped!(InstructionSend),
            Method::SessionSnapshot => {
                let _: Empty = params!();
                RequestBody::SessionSnapshot(session.unwrap())
            }
            Method::SessionSubscribe => scoped!(SessionSubscribe),
            Method::HistoryPage => scoped!(HistoryPage),
            Method::DraftUpdate => scoped!(DraftUpdate),
            Method::SessionConfigure => scoped!(SessionConfigure),
            Method::WorkflowConfigure => scoped!(WorkflowConfigure),
            Method::LocalModels => scoped!(LocalModels),
            Method::WorkspaceRead => scoped!(WorkspaceRead),
            Method::AttachmentRead => scoped!(AttachmentRead),
            Method::RunStart => scoped!(RunStart),
            Method::RunCancel => scoped!(RunCancel),
            Method::ApprovalResolve => scoped!(ApprovalResolve),
            Method::SourceApplyList => scoped!(SourceApplyList),
            Method::SourceApplyPage => scoped!(SourceApplyPage),
            Method::SourceApplyResolve => scoped!(SourceApplyResolve),
            Method::RequestStatus => scoped!(RequestStatus),
            Method::ShutdownRequest => RequestBody::ShutdownRequest(params!()),
            Method::BrowseList => RequestBody::BrowseList(params!()),
            Method::BrowseRead => RequestBody::BrowseRead(params!()),
            Method::McpRegistryRead => {
                let _: Empty = params!();
                RequestBody::McpRegistryRead(session.unwrap())
            }
            Method::McpRegistrySave => scoped!(McpRegistrySave),
            Method::McpSecretSet => scoped!(McpSecretSet),
            Method::McpSecretDelete => scoped!(McpSecretDelete),
            Method::McpCatalogCheck => scoped!(McpCatalogCheck),
            Method::McpCatalogCancel => scoped!(McpCatalogCancel),
            Method::McpSetupDetect => {
                let _: Empty = params!();
                RequestBody::McpSetupDetect(session.unwrap())
            }
            Method::McpSetupRegister => scoped!(McpSetupRegister),
            Method::RunDetails => {
                let params: crate::run_details::RunDetailsRequest = params!();
                // A later page names both its snapshot and cursor.
                if params.snapshot_id.is_some() != params.cursor.is_some() {
                    return Err(de::Error::custom("run details continuation"));
                }
                RequestBody::RunDetails(
                    session.ok_or_else(|| de::Error::custom("missing session"))?,
                    params,
                )
            }
        };
        if wire.protocol_version == ProtocolVersion::V1
            && matches!(
                wire.method,
                Method::SessionList
                    | Method::SessionSelect
                    | Method::InputSubmit
                    | Method::InputStatus
                    | Method::InputCancel
                    | Method::SubagentWait
                    | Method::SubagentCancel
            )
        {
            return Err(de::Error::custom("protocol v2 method on v1 frame"));
        }
        Ok(Self {
            protocol_version: wire.protocol_version,
            client_id: wire.client_id,
            request_id: wire.request_id,
            body,
        })
    }
}

impl Serialize for Request {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut wire = s.serialize_struct(
            "Request",
            if self.body.session_id().is_some() {
                7
            } else {
                6
            },
        )?;
        wire.serialize_field("protocol_version", &self.protocol_version)?;
        wire.serialize_field("kind", &RequestKind::Request)?;
        wire.serialize_field("client_id", &self.client_id)?;
        wire.serialize_field("request_id", &self.request_id)?;
        wire.serialize_field("method", &self.body.method())?;
        if let Some(session) = self.body.session_id() {
            wire.serialize_field("session_id", session)?;
        }
        match &self.body {
            RequestBody::Hello
            | RequestBody::SessionList
            | RequestBody::SessionOpen(_)
            | RequestBody::SessionSelect(_)
            | RequestBody::SessionSnapshot(_)
            | RequestBody::McpRegistryRead(_)
            | RequestBody::McpSetupDetect(_) => wire.serialize_field("params", &Empty {})?,
            RequestBody::SessionSubscribe(_, p) => wire.serialize_field("params", p)?,
            RequestBody::InputSubmit(_, p) => wire.serialize_field("params", p)?,
            RequestBody::InputStatus(_, p) => wire.serialize_field("params", p)?,
            RequestBody::InputCancel(_, p) => wire.serialize_field("params", p)?,
            RequestBody::SubagentWait(_, p) => wire.serialize_field("params", p)?,
            RequestBody::SubagentCancel(_, p) => wire.serialize_field("params", p)?,
            RequestBody::HistoryPage(_, p) => wire.serialize_field("params", p)?,
            RequestBody::DraftUpdate(_, p) => wire.serialize_field("params", p)?,
            RequestBody::SessionConfigure(_, p) => wire.serialize_field("params", p)?,
            RequestBody::WorkflowConfigure(_, p) => wire.serialize_field("params", p)?,
            RequestBody::SessionRolesConfigure(_, p) => wire.serialize_field("params", p)?,
            RequestBody::LocalModels(_, p) => wire.serialize_field("params", p)?,
            RequestBody::WorkspaceRead(_, p) => wire.serialize_field("params", p)?,
            RequestBody::AttachmentRead(_, p) => wire.serialize_field("params", p)?,
            RequestBody::GoalUpdate(_, p) => wire.serialize_field("params", p)?,
            RequestBody::InstructionSend(_, p) => wire.serialize_field("params", p)?,
            RequestBody::RunStart(_, p) => wire.serialize_field("params", p)?,
            RequestBody::RunCancel(_, p) => wire.serialize_field("params", p)?,
            RequestBody::ApprovalResolve(_, p) => wire.serialize_field("params", p)?,
            RequestBody::SourceApplyList(_, p) => wire.serialize_field("params", p)?,
            RequestBody::SourceApplyPage(_, p) => wire.serialize_field("params", p)?,
            RequestBody::SourceApplyResolve(_, p) => wire.serialize_field("params", p)?,
            RequestBody::RequestStatus(_, p) => wire.serialize_field("params", p)?,
            RequestBody::ShutdownRequest(p) => wire.serialize_field("params", p)?,
            RequestBody::McpRegistrySave(_, p) => wire.serialize_field("params", p)?,
            RequestBody::McpSecretSet(_, p) => wire.serialize_field("params", p)?,
            RequestBody::McpSecretDelete(_, p) => wire.serialize_field("params", p)?,
            RequestBody::McpCatalogCheck(_, p) => wire.serialize_field("params", p)?,
            RequestBody::McpCatalogCancel(_, p) => wire.serialize_field("params", p)?,
            RequestBody::McpSetupRegister(_, p) => wire.serialize_field("params", p)?,
            RequestBody::RunDetails(_, p) => wire.serialize_field("params", p)?,
            RequestBody::BrowseList(p) => wire.serialize_field("params", p)?,
            RequestBody::BrowseRead(p) => wire.serialize_field("params", p)?,
        }
        wire.end()
    }
}
