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
    SessionRead,
    HistoryRead,
    DraftUpdate,
    SessionConfigure,
    RunStart,
    RunCancel,
    ApprovalResolve,
    SourceApplyRead,
    SourceApplyResolve,
    RequestStatus,
    Shutdown,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Queued {
    Queued,
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
    #[serde(rename = "hello")]
    Hello(Hello),
    #[serde(rename = "session.open")]
    SessionOpen(Box<Snapshot>),
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
}
}

impl SuccessResult {
    pub fn method(&self) -> Method {
        match self {
            Self::Hello(_) => Method::Hello,
            Self::LocalModels(_) => Method::LocalModels,
            Self::SessionOpen(_) => Method::SessionOpen,
            Self::SessionSnapshot(_) => Method::SessionSnapshot,
            Self::SessionSubscribe(_) => Method::SessionSubscribe,
            Self::HistoryPage(_) => Method::HistoryPage,
            Self::DraftUpdate(_) => Method::DraftUpdate,
            Self::SessionConfigure(_) => Method::SessionConfigure,
            Self::RunStart(_) => Method::RunStart,
            Self::RunCancel(_) => Method::RunCancel,
            Self::ApprovalResolve(_) => Method::ApprovalResolve,
            Self::SourceApplyList(_) => Method::SourceApplyList,
            Self::SourceApplyPage(_) => Method::SourceApplyPage,
            Self::SourceApplyResolve(_) => Method::SourceApplyResolve,
            Self::RequestStatus(_) => Method::RequestStatus,
            Self::ShutdownRequest(_) => Method::ShutdownRequest,
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
    ClientId,
    RequestId,
    Method,
}

impl Response {
    /// resultのtypeは元要求のmethodと一致する。失敗応答にも相関IDを要求する。
    pub fn validate_for(&self, request: &Request) -> Result<(), CorrelationError> {
        if self.client_id != request.client_id {
            return Err(CorrelationError::ClientId);
        }
        if self.request_id != request.request_id {
            return Err(CorrelationError::RequestId);
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
        {
            if result.provider != params.provider || result.endpoint != params.endpoint {
                return Err(CorrelationError::Method);
            }
        }
        Ok(())
    }
    pub fn for_request(
        request: &Request,
        outcome: Result<SuccessResult, ProtocolError>,
    ) -> Result<Self, CorrelationError> {
        let response = Self {
            protocol_version: ProtocolVersion,
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
            (Some(result), None) => Ok(result),
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
