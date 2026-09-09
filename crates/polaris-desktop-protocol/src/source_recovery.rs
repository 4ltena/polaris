//! Bounded result-save recovery wire values. No I/O, authority or apply replay.
//! A saved result is persistence evidence, never evidence of successful apply.
pub use crate::codec::{CodecError, Decode};
use crate::{ProtocolVersion, codec, ids::*};
use serde::{
    Deserialize, Deserializer, Serialize,
    de::{self, DeserializeOwned},
};
use std::io;

/// Maximum JSON body, excluding the four-byte big-endian length prefix.
pub const MAX_FRAME_BYTES: usize = 8 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct PayloadHash(String);
impl PayloadHash {
    pub fn new(value: impl Into<String>) -> Result<Self, CodecError> {
        let value = value.into();
        if value.len() != 64
            || !value
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err(CodecError::Schema);
        }
        Ok(Self(value))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}
impl<'de> Deserialize<'de> for PayloadHash {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Self::new(String::deserialize(d)?).map_err(de::Error::custom)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "method", rename_all = "snake_case", deny_unknown_fields)]
pub enum Request {
    Hello {
        version: ProtocolVersion,
        request_id: RequestId,
    },
    RetryResultSave {
        version: ProtocolVersion,
        request_id: RequestId,
        engine_epoch: EngineEpoch,
        project_id: ProjectId,
        session_id: SessionId,
        run_id: RunId,
        attempt_id: AttemptId,
        approval_id: ApprovalId,
        operation_id: OperationId,
        payload_hash: PayloadHash,
    },
}
impl Request {
    pub fn request_id(&self) -> &RequestId {
        match self {
            Self::Hello { request_id, .. } | Self::RetryResultSave { request_id, .. } => request_id,
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Working,
    ReportPending,
    RecoveryRequired,
    ReadyToExit,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    Busy,
    TargetMismatch,
    NoRetainedReport,
    RequestConflict,
    StorageFailed,
}

crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedResult {
    pub operation_id:OperationId,
    pub result_id:ResultId,
    pub saved_revision:DecimalU64,
}
}
crate::object_wire! {
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetryTarget {
    pub run_id:RunId,
    pub attempt_id:AttemptId,
    pub approval_id:ApprovalId,
    pub operation_id:OperationId,
    pub payload_hash:PayloadHash,
}
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Response {
    pub version: ProtocolVersion,
    pub request_id: RequestId,
    pub engine_epoch: EngineEpoch,
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub state: State,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<SavedResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub retry_target: Option<RetryTarget>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ErrorCode>,
}
crate::object_wire! {
#[derive(Serialize,Deserialize)]
#[serde(deny_unknown_fields)]
struct ResponseWire {
    version:ProtocolVersion,
    request_id:RequestId,
    engine_epoch:EngineEpoch,
    project_id:ProjectId,
    session_id:SessionId,
    state:State,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="crate::present")]
    result:Option<SavedResult>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="crate::present")]
    retry_target:Option<RetryTarget>,
    #[serde(default,skip_serializing_if="Option::is_none",deserialize_with="crate::present")]
    error:Option<ErrorCode>,
}
}
impl Response {
    pub fn validate(&self) -> Result<(), CodecError> {
        if (self.state == State::ReportPending) != self.retry_target.is_some()
            || (self.result.is_some() && self.error.is_some())
        {
            return Err(CodecError::Schema);
        }
        Ok(())
    }

    /// Correlates a response with its request. A saved operation may coexist
    /// with the next pending target; neither is evidence that apply succeeded.
    pub fn validate_for_request(&self, request: &Request) -> Result<(), CodecError> {
        self.validate()?;
        if &self.request_id != request.request_id() {
            return Err(CodecError::Schema);
        }
        match request {
            Request::Hello { .. } => {
                if self.result.is_some() {
                    return Err(CodecError::Schema);
                }
            }
            Request::RetryResultSave {
                engine_epoch,
                project_id,
                session_id,
                operation_id,
                ..
            } => {
                if self.error.is_none() {
                    let result = self.result.as_ref().ok_or(CodecError::Schema)?;
                    if &result.operation_id != operation_id
                        || &self.engine_epoch != engine_epoch
                        || &self.project_id != project_id
                        || &self.session_id != session_id
                    {
                        return Err(CodecError::Schema);
                    }
                }
            }
        }
        Ok(())
    }
}
impl<'de> Deserialize<'de> for Response {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let w = ResponseWire::deserialize(d)?;
        let result = Self {
            version: w.version,
            request_id: w.request_id,
            engine_epoch: w.engine_epoch,
            project_id: w.project_id,
            session_id: w.session_id,
            state: w.state,
            result: w.result,
            retry_target: w.retry_target,
            error: w.error,
        };
        result.validate().map_err(de::Error::custom)?;
        Ok(result)
    }
}

/// Finite byte parsing only. Caller owns buffering, fixed deadlines and EOF.
pub fn decode<T: DeserializeOwned>(input: &[u8]) -> Decode<T> {
    if input.len() < 4 {
        return Decode::NeedMore;
    }
    let length = u32::from_be_bytes(input[..4].try_into().expect("four bytes")) as usize;
    if length > MAX_FRAME_BYTES {
        return Decode::Invalid(CodecError::FrameTooLarge);
    }
    codec::decode(input)
}
pub fn finish<T: DeserializeOwned>(input: &[u8]) -> Result<Option<(T, usize)>, CodecError> {
    if input.is_empty() {
        return Ok(None);
    }
    match decode(input) {
        Decode::NeedMore => Err(CodecError::UnexpectedEof),
        Decode::Invalid(e) => Err(e),
        Decode::Decoded { value, consumed } => Ok(Some((value, consumed))),
    }
}
struct Buffer {
    bytes: Vec<u8>,
    overflow: bool,
}
impl io::Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.len() > MAX_FRAME_BYTES - self.bytes.len() {
            self.overflow = true;
            return Err(io::Error::other("recovery frame limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
/// Body never grows beyond 8KiB; strict decode validates schema before sending.
pub fn encode<T: Serialize + DeserializeOwned>(value: &T) -> Result<Vec<u8>, CodecError> {
    let mut body = Buffer {
        bytes: Vec::new(),
        overflow: false,
    };
    if serde_json::to_writer(&mut body, value).is_err() {
        return Err(if body.overflow {
            CodecError::FrameTooLarge
        } else {
            CodecError::Schema
        });
    }
    let _: T = codec::from_json(&body.bytes)?;
    let mut frame = Vec::with_capacity(body.bytes.len() + 4);
    frame.extend_from_slice(&(body.bytes.len() as u32).to_be_bytes());
    frame.extend(body.bytes);
    Ok(frame)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn raw(bytes: &[u8]) -> Vec<u8> {
        let mut frame = (bytes.len() as u32).to_be_bytes().to_vec();
        frame.extend(bytes);
        frame
    }
    fn retry() -> Request {
        Request::RetryResultSave {
            version: Default::default(),
            request_id: RequestId::new("request").unwrap(),
            engine_epoch: EngineEpoch::new("epoch").unwrap(),
            project_id: ProjectId::new("project").unwrap(),
            session_id: SessionId::new("session").unwrap(),
            run_id: RunId::new("run").unwrap(),
            attempt_id: AttemptId::new("attempt").unwrap(),
            approval_id: ApprovalId::new("approval").unwrap(),
            operation_id: OperationId::new("operation").unwrap(),
            payload_hash: PayloadHash::new("a".repeat(64)).unwrap(),
        }
    }
    fn response() -> Response {
        Response {
            version: Default::default(),
            request_id: RequestId::new("request").unwrap(),
            engine_epoch: EngineEpoch::new("epoch").unwrap(),
            project_id: ProjectId::new("project").unwrap(),
            session_id: SessionId::new("session").unwrap(),
            state: State::ReadyToExit,
            result: None,
            retry_target: None,
            error: None,
        }
    }
    fn target() -> RetryTarget {
        RetryTarget {
            run_id: RunId::new("run").unwrap(),
            attempt_id: AttemptId::new("attempt").unwrap(),
            approval_id: ApprovalId::new("approval").unwrap(),
            operation_id: OperationId::new("operation").unwrap(),
            payload_hash: PayloadHash::new("a".repeat(64)).unwrap(),
        }
    }
    #[test]
    fn recovery_round_trip_fragmentation_and_eof() {
        for request in [
            Request::Hello {
                version: Default::default(),
                request_id: RequestId::new("hello").unwrap(),
            },
            retry(),
        ] {
            let frame = encode(&request).unwrap();
            for n in 0..frame.len() {
                assert_eq!(decode::<Request>(&frame[..n]), Decode::NeedMore);
                if n > 0 {
                    assert_eq!(
                        finish::<Request>(&frame[..n]),
                        Err(CodecError::UnexpectedEof)
                    );
                }
            }
            let mut pair = frame.clone();
            pair.extend(&frame);
            assert_eq!(
                decode::<Request>(&pair),
                Decode::Decoded {
                    value: request,
                    consumed: frame.len()
                }
            );
        }
        assert_eq!(finish::<Request>(&[]), Ok(None));
        let value = response();
        assert_eq!(
            finish::<Response>(&encode(&value).unwrap())
                .unwrap()
                .unwrap()
                .0,
            value
        );
    }
    #[test]
    fn recovery_rejects_oversize_and_malformed() {
        assert_eq!(
            decode::<Request>(&((MAX_FRAME_BYTES + 1) as u32).to_be_bytes()),
            Decode::Invalid(CodecError::FrameTooLarge)
        );
        assert!(encode(&"x".repeat(MAX_FRAME_BYTES)).is_err());
        for bytes in [&b"\xff"[..], &b"{"[..], &b"[]"[..], &b"null"[..]] {
            assert!(matches!(decode::<Request>(&raw(bytes)), Decode::Invalid(_)));
        }
    }
    #[test]
    fn recovery_rejects_duplicate_unknown_null_and_bad_targets() {
        let original = serde_json::to_string(&retry()).unwrap();
        for extra in [
            "\"path\":\"/tmp\"",
            "\"run_id\":\"other\"",
            "\"r\\u0075n_id\":\"other\"",
        ] {
            let text = format!("{},{} }}", &original[..original.len() - 1], extra);
            assert!(matches!(
                decode::<Request>(&raw(text.as_bytes())),
                Decode::Invalid(_)
            ));
        }
        for (key, value) in [
            ("version", serde_json::json!(2)),
            ("run_id", serde_json::json!("")),
            ("request_id", serde_json::json!("x".repeat(129))),
            ("payload_hash", serde_json::json!("A".repeat(64))),
            ("payload_hash", serde_json::json!("a".repeat(63))),
            ("session_id", serde_json::Value::Null),
        ] {
            let mut v = serde_json::to_value(retry()).unwrap();
            v[key] = value;
            assert!(matches!(
                decode::<Request>(&raw(v.to_string().as_bytes())),
                Decode::Invalid(_)
            ));
        }
    }
    #[test]
    fn recovery_response_consistency_and_nested_schema() {
        let mut r = response();
        r.retry_target = Some(target());
        assert!(encode(&r).is_err());
        r.state = State::ReportPending;
        assert!(encode(&r).is_ok());
        r.retry_target = None;
        assert!(encode(&r).is_err());
        r.state = State::ReadyToExit;
        r.result = Some(SavedResult {
            operation_id: OperationId::new("operation").unwrap(),
            result_id: ResultId::new("result").unwrap(),
            saved_revision: DecimalU64::new(9),
        });
        assert!(encode(&r).is_ok());
        r.error = Some(ErrorCode::StorageFailed);
        assert!(encode(&r).is_err());
        r.error = None;
        let mut v = serde_json::to_value(&r).unwrap();
        v["result"]["saved_revision"] = serde_json::json!(9);
        assert!(matches!(
            decode::<Response>(&raw(v.to_string().as_bytes())),
            Decode::Invalid(_)
        ));
        let mut v = serde_json::to_value(response()).unwrap();
        v["error"] = serde_json::Value::Null;
        assert!(matches!(
            decode::<Response>(&raw(v.to_string().as_bytes())),
            Decode::Invalid(_)
        ));
        let nested = format!(
            r#"{{"version":1,"request_id":"r","engine_epoch":"e","project_id":"p","session_id":"s","state":"report_pending","retry_target":{{"run_id":"r","run_id":"x","attempt_id":"a","approval_id":"a","operation_id":"o","payload_hash":"{}"}}}}"#,
            "a".repeat(64)
        );
        assert_eq!(
            decode::<Response>(&raw(nested.as_bytes())),
            Decode::Invalid(CodecError::DuplicateKey)
        );
    }
    #[test]
    fn recovery_saved_result_and_next_pending_target_are_valid() {
        let request = retry();
        let mut r = response();
        r.state = State::ReportPending;
        let mut next = target();
        next.operation_id = OperationId::new("next-operation").unwrap();
        r.retry_target = Some(next);
        r.result = Some(SavedResult {
            operation_id: OperationId::new("operation").unwrap(),
            result_id: ResultId::new("saved-result").unwrap(),
            saved_revision: DecimalU64::new(11),
        });
        let frame = encode(&r).unwrap();
        let decoded = finish::<Response>(&frame).unwrap().unwrap().0;
        assert_eq!(decoded, r);
        assert!(decoded.validate_for_request(&request).is_ok());
        let hello = Request::Hello {
            version: Default::default(),
            request_id: request.request_id().clone(),
        };
        assert!(decoded.validate_for_request(&hello).is_err());
        r.result.as_mut().unwrap().operation_id = OperationId::new("wrong-operation").unwrap();
        assert!(r.validate_for_request(&request).is_err());
        r.result = None;
        assert!(r.validate_for_request(&request).is_err());
        r.error = Some(ErrorCode::StorageFailed);
        assert!(r.validate_for_request(&request).is_ok());
        r.request_id = RequestId::new("wrong-request").unwrap();
        assert!(r.validate_for_request(&request).is_err());
    }
}
