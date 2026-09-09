//! hello前後の受付を純粋に判定する。構造の受理は認可・保存・実行開始を意味しない。

use crate::request::{Request, RequestBody};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ConnectionState {
    #[default]
    AwaitingHello,
    Ready,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateError {
    HelloRequired,
    AlreadyReady,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedRequest<'a> {
    pub request: &'a Request,
    pub next_state: ConnectionState,
}

/// 呼出側はhello成功時だけnext_stateを採用する。拒否は状態を変更しない。
pub fn validate_request(
    state: ConnectionState,
    request: &Request,
) -> Result<ValidatedRequest<'_>, GateError> {
    let next_state = match (state, &request.body) {
        (ConnectionState::AwaitingHello, RequestBody::Hello) => ConnectionState::Ready,
        (ConnectionState::AwaitingHello, _) => return Err(GateError::HelloRequired),
        (ConnectionState::Ready, RequestBody::Hello) => return Err(GateError::AlreadyReady),
        (ConnectionState::Ready, _) => ConnectionState::Ready,
    };
    Ok(ValidatedRequest {
        request,
        next_state,
    })
}
