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
    #[serde(rename = "hello")]
    Hello,
    #[serde(rename = "session.open")]
    SessionOpen,
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
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RunStart {
    pub expected_draft_revision: DecimalU64,
    pub expected_configuration_revision: DecimalU64,
    pub expected_policy_revision: DecimalU64,
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

/// sessionの要否をenumの形で固定し、無関係なsessionの黙殺を防ぐ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RequestBody {
    LocalModels(SessionId, LocalModels),
    Hello,
    SessionOpen(SessionId),
    SessionSnapshot(SessionId),
    SessionSubscribe(SessionId, Subscribe),
    HistoryPage(SessionId, HistoryPage),
    DraftUpdate(SessionId, DraftUpdate),
    SessionConfigure(SessionId, SessionConfigure),
    RunStart(SessionId, RunStart),
    RunCancel(SessionId, RunTarget),
    ApprovalResolve(SessionId, ApprovalResolve),
    SourceApplyList(SessionId, SourceApplyList),
    SourceApplyPage(SessionId, SourceApplyPage),
    SourceApplyResolve(SessionId, SourceApplyResolve),
    RequestStatus(SessionId, RequestStatus),
    ShutdownRequest(ShutdownRequest),
}

impl RequestBody {
    pub fn method(&self) -> Method {
        match self {
            Self::Hello => Method::Hello,
            Self::LocalModels(..) => Method::LocalModels,
            Self::SessionOpen(..) => Method::SessionOpen,
            Self::SessionSnapshot(..) => Method::SessionSnapshot,
            Self::SessionSubscribe(..) => Method::SessionSubscribe,
            Self::HistoryPage(..) => Method::HistoryPage,
            Self::DraftUpdate(..) => Method::DraftUpdate,
            Self::SessionConfigure(..) => Method::SessionConfigure,
            Self::RunStart(..) => Method::RunStart,
            Self::RunCancel(..) => Method::RunCancel,
            Self::ApprovalResolve(..) => Method::ApprovalResolve,
            Self::SourceApplyList(..) => Method::SourceApplyList,
            Self::SourceApplyPage(..) => Method::SourceApplyPage,
            Self::SourceApplyResolve(..) => Method::SourceApplyResolve,
            Self::RequestStatus(..) => Method::RequestStatus,
            Self::ShutdownRequest(..) => Method::ShutdownRequest,
        }
    }
    pub fn session_id(&self) -> Option<&SessionId> {
        match self {
            Self::Hello | Self::ShutdownRequest(..) => None,
            Self::LocalModels(s, _)
            | Self::SessionOpen(s)
            | Self::SessionSnapshot(s)
            | Self::SessionSubscribe(s, _)
            | Self::HistoryPage(s, _)
            | Self::DraftUpdate(s, _)
            | Self::SessionConfigure(s, _)
            | Self::RunStart(s, _)
            | Self::RunCancel(s, _)
            | Self::ApprovalResolve(s, _)
            | Self::SourceApplyList(s, _)
            | Self::SourceApplyPage(s, _)
            | Self::SourceApplyResolve(s, _)
            | Self::RequestStatus(s, _) => Some(s),
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
        let needs_session = !matches!(wire.method, Method::Hello | Method::ShutdownRequest);
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
            Method::SessionSnapshot => {
                let _: Empty = params!();
                RequestBody::SessionSnapshot(session.unwrap())
            }
            Method::SessionSubscribe => scoped!(SessionSubscribe),
            Method::HistoryPage => scoped!(HistoryPage),
            Method::DraftUpdate => scoped!(DraftUpdate),
            Method::SessionConfigure => scoped!(SessionConfigure),
            Method::LocalModels => scoped!(LocalModels),
            Method::RunStart => scoped!(RunStart),
            Method::RunCancel => scoped!(RunCancel),
            Method::ApprovalResolve => scoped!(ApprovalResolve),
            Method::SourceApplyList => scoped!(SourceApplyList),
            Method::SourceApplyPage => scoped!(SourceApplyPage),
            Method::SourceApplyResolve => scoped!(SourceApplyResolve),
            Method::RequestStatus => scoped!(RequestStatus),
            Method::ShutdownRequest => RequestBody::ShutdownRequest(params!()),
        };
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
            RequestBody::Hello | RequestBody::SessionOpen(_) | RequestBody::SessionSnapshot(_) => {
                wire.serialize_field("params", &Empty {})?
            }
            RequestBody::SessionSubscribe(_, p) => wire.serialize_field("params", p)?,
            RequestBody::HistoryPage(_, p) => wire.serialize_field("params", p)?,
            RequestBody::DraftUpdate(_, p) => wire.serialize_field("params", p)?,
            RequestBody::SessionConfigure(_, p) => wire.serialize_field("params", p)?,
            RequestBody::LocalModels(_, p) => wire.serialize_field("params", p)?,
            RequestBody::RunStart(_, p) => wire.serialize_field("params", p)?,
            RequestBody::RunCancel(_, p) => wire.serialize_field("params", p)?,
            RequestBody::ApprovalResolve(_, p) => wire.serialize_field("params", p)?,
            RequestBody::SourceApplyList(_, p) => wire.serialize_field("params", p)?,
            RequestBody::SourceApplyPage(_, p) => wire.serialize_field("params", p)?,
            RequestBody::SourceApplyResolve(_, p) => wire.serialize_field("params", p)?,
            RequestBody::RequestStatus(_, p) => wire.serialize_field("params", p)?,
            RequestBody::ShutdownRequest(p) => wire.serialize_field("params", p)?,
        }
        wire.end()
    }
}
