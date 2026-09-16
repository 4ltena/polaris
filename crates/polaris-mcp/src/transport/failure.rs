//! Closed metadata error classes survive SDK channel closure without raw errors.
use crate::Error;
use rmcp::{ServiceError, service::ClientInitializeError};
use std::sync::{Arc, Mutex};

/// Read errors can become a generic closed channel in the SDK. Preserve the
/// first non-transient boundary failure for this connection before that happens.
#[derive(Clone, Default)]
pub(super) struct Failures(Arc<Mutex<Option<Error>>>);
impl Failures {
    pub(super) fn record(&self, error: Error) {
        if !matches!(error, Error::Transport | Error::Timeout | Error::Cancelled)
            && let Ok(mut failure) = self.0.lock()
        {
            failure.get_or_insert(error);
        }
    }

    pub(super) fn resolve(&self, error: Error) -> Error {
        if error == Error::Cancelled {
            return error;
        }
        self.0
            .lock()
            .map_or(Error::Protection, |saved| saved.unwrap_or(error))
    }
}

pub(super) fn http(
    error: &rmcp::transport::streamable_http_client::StreamableHttpError<Error>,
) -> Error {
    use rmcp::transport::streamable_http_client::StreamableHttpError as E;
    match error {
        E::Client(error) => *error,
        E::AuthRequired(_) | E::InsufficientScope(_) => Error::Auth,
        E::UnexpectedEndOfStream | E::TransportChannelClosed | E::SessionExpired => {
            Error::Transport
        }
        E::Io(error) => error
            .get_ref()
            .and_then(|e| e.downcast_ref::<Error>())
            .copied()
            .unwrap_or(Error::Transport),
        E::ControlRequestTimeout | E::SessionRecoveryTimeout => Error::Timeout,
        _ => Error::Protocol,
    }
}

fn transport(error: &(dyn std::error::Error + 'static)) -> Error {
    let mut source = Some(error);
    for _ in 0..16 {
        let Some(error) = source else {
            break;
        };
        if let Some(error) = error.downcast_ref::<Error>() {
            return *error;
        }
        if let Some(error) = error
            .downcast_ref::<rmcp::transport::streamable_http_client::StreamableHttpError<Error>>()
        {
            return http(error);
        }
        source = error.source();
    }
    Error::Transport
}

pub(super) fn initialize(error: ClientInitializeError) -> Error {
    match error {
        ClientInitializeError::Cancelled => Error::Cancelled,
        ClientInitializeError::ConnectionClosed(_) => Error::Transport,
        ClientInitializeError::TransportError { error, .. } => transport(error.error.as_ref()),
        ClientInitializeError::LegacyFallbackFailed { fallback, .. } => initialize(*fallback),
        _ => Error::Protocol,
    }
}

pub(super) fn service(error: ServiceError) -> Error {
    match error {
        ServiceError::Cancelled { .. } => Error::Cancelled,
        ServiceError::Timeout { .. } => Error::Timeout,
        ServiceError::TransportClosed => Error::Transport,
        ServiceError::TransportSend(error) => transport(error.error.as_ref()),
        _ => Error::Protocol,
    }
}
