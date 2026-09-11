//! 配信位置付きeventと本文byte offset・durability、承認更新を型付きpayloadで保持する。

use crate::{ProtocolVersion, codec::StrictValue, ids::*, response::ApprovalResolved, snapshot::*};
use serde::{Deserialize, Deserializer, Serialize, Serializer, de};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Durability {
    Tentative,
    Saved,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextDelta {
    pub message_id: MessageId,
    pub byte_offset: DecimalU64,
    pub text: String,
    pub durability: Durability,
}
}

impl TextDelta {
    /// UTF-8 byte単位。文字数への読み替えもu64 wrapも行わない。
    pub fn end_offset(&self) -> Result<DecimalU64, ValueError> {
        self.byte_offset.checked_add(self.text.len() as u64)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExpiryReason {
    Cancelled,
    Shutdown,
    Expired,
    PolicyRevoked,
    EngineRestarted,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ApprovalExpired {
    pub approval_id: ApprovalId,
    pub reason: ExpiryReason,
}
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", content = "payload", deny_unknown_fields)]
pub enum EventBody {
    #[serde(rename = "message.delta")]
    MessageDelta(TextDelta),
    #[serde(rename = "run.state")]
    RunState(Run),
    #[serde(rename = "task.updated")]
    TaskUpdated(Task),
    #[serde(rename = "child.updated")]
    ChildUpdated(Child),
    #[serde(rename = "draft.updated")]
    DraftUpdated(Draft),
    #[serde(rename = "configuration.updated")]
    ConfigurationUpdated(Configuration),
    #[serde(rename = "memory.updated")]
    MemoryUpdated(MemoryStatus),
    #[serde(rename = "role_bindings.updated")]
    RoleBindingsUpdated(crate::role_bindings::RoleBindingsConfigured),
    #[serde(rename = "approval.requested")]
    ApprovalRequested(PendingApproval),
    #[serde(rename = "approval.resolved")]
    ApprovalResolved(ApprovalResolved),
    #[serde(rename = "approval.expired")]
    ApprovalExpired(ApprovalExpired),
}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub protocol_version: ProtocolVersion,
    pub engine_epoch: EngineEpoch,
    pub subscription_id: SubscriptionId,
    pub event_seq: DecimalU64,
    pub session_id: SessionId,
    pub session_revision: DecimalU64,
    pub body: EventBody,
}

#[derive(Serialize, Deserialize)]
enum EventKind {
    #[serde(rename = "event")]
    Event,
}

crate::object_wire! {
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Wire {
    protocol_version: ProtocolVersion,
    kind: EventKind,
    engine_epoch: EngineEpoch,
    subscription_id: SubscriptionId,
    event_seq: DecimalU64,
    session_id: SessionId,
    session_revision: DecimalU64,
    #[serde(rename = "type")]
    event_type: String,
    payload: serde_json::Value,
}
}

impl<'de> Deserialize<'de> for Event {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let wire: Wire =
            serde_json::from_value(StrictValue::deserialize(d)?.0).map_err(de::Error::custom)?;
        let EventKind::Event = wire.kind;
        if !matches!(
            wire.event_type.as_str(),
            "message.delta"
                | "run.state"
                | "task.updated"
                | "child.updated"
                | "draft.updated"
                | "configuration.updated"
                | "memory.updated"
                | "role_bindings.updated"
                | "approval.requested"
                | "approval.resolved"
                | "approval.expired"
        ) {
            return Err(de::Error::custom("unknown state event; resync required"));
        }
        let body = serde_json::from_value(
            serde_json::json!({"type": wire.event_type, "payload": wire.payload}),
        )
        .map_err(de::Error::custom)?;
        Ok(Self {
            protocol_version: wire.protocol_version,
            engine_epoch: wire.engine_epoch,
            subscription_id: wire.subscription_id,
            event_seq: wire.event_seq,
            session_id: wire.session_id,
            session_revision: wire.session_revision,
            body,
        })
    }
}

impl Serialize for Event {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut wire = s.serialize_struct("Event", 9)?;
        wire.serialize_field("protocol_version", &self.protocol_version)?;
        wire.serialize_field("kind", &EventKind::Event)?;
        wire.serialize_field("engine_epoch", &self.engine_epoch)?;
        wire.serialize_field("subscription_id", &self.subscription_id)?;
        wire.serialize_field("event_seq", &self.event_seq)?;
        wire.serialize_field("session_id", &self.session_id)?;
        wire.serialize_field("session_revision", &self.session_revision)?;
        macro_rules! payload {
            ($name:literal, $p:expr) => {{
                wire.serialize_field("type", $name)?;
                wire.serialize_field("payload", $p)?;
            }};
        }
        match &self.body {
            EventBody::MessageDelta(p) => payload!("message.delta", p),
            EventBody::RunState(p) => payload!("run.state", p),
            EventBody::TaskUpdated(p) => payload!("task.updated", p),
            EventBody::ChildUpdated(p) => payload!("child.updated", p),
            EventBody::DraftUpdated(p) => payload!("draft.updated", p),
            EventBody::ConfigurationUpdated(p) => payload!("configuration.updated", p),
            EventBody::MemoryUpdated(p) => payload!("memory.updated", p),
            EventBody::RoleBindingsUpdated(p) => payload!("role_bindings.updated", p),
            EventBody::ApprovalRequested(p) => payload!("approval.requested", p),
            EventBody::ApprovalResolved(p) => payload!("approval.resolved", p),
            EventBody::ApprovalExpired(p) => payload!("approval.expired", p),
        }
        wire.end()
    }
}
