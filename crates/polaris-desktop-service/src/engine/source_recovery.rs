//! Trusted result-only ingress. The reactor holds a sender, never the Writer.
//! Dropping a response future does not cancel an accepted owner operation.
use super::*;
use polaris_desktop_protocol::source_recovery as wire;
use tokio::sync::{mpsc, oneshot};

pub struct SourceRecoveryHandle {
    sender: mpsc::Sender<Envelope>,
}
struct Envelope {
    request: wire::Request,
    response: oneshot::Sender<wire::Response>,
}
pub(super) struct Ingress {
    receiver: mpsc::Receiver<Envelope>,
}
impl SourceRecoveryHandle {
    /// Capacity one. Never waits for the owner; caller owns its reply deadline.
    pub fn try_request(
        &self,
        request: wire::Request,
    ) -> Result<oneshot::Receiver<wire::Response>, wire::ErrorCode> {
        wire::encode(&request).map_err(|_| wire::ErrorCode::TargetMismatch)?;
        let (response, receiver) = oneshot::channel();
        self.sender
            .try_send(Envelope { request, response })
            .map_err(|_| wire::ErrorCode::Busy)?;
        Ok(receiver)
    }
}
impl DesktopService {
    /// Attach once, before serve, only to an explicitly configured source owner.
    /// No socket or ordinary binary activation is performed here.
    pub fn attach_source_recovery(&mut self) -> Result<SourceRecoveryHandle, ServiceError> {
        let engine = self.engine.as_mut().ok_or(ServiceError::Options)?;
        if engine.served || engine.source.is_none() || engine.source_recovery.is_some() {
            return Err(ServiceError::Options);
        }
        let (sender, receiver) = mpsc::channel(1);
        engine.source_recovery = Some(Ingress { receiver });
        Ok(SourceRecoveryHandle { sender })
    }
}
impl Engine {
    pub(super) fn close_source_recovery(&mut self) {
        // The engine can remain in DesktopService after its owner loop ends.
        // Release queued reply senders now, not when that outer object is dropped.
        self.source_recovery = None;
    }
    pub(super) fn poll_source_recovery(&mut self) {
        let Some(envelope) = self
            .source_recovery
            .as_mut()
            .and_then(|ingress| ingress.receiver.try_recv().ok())
        else {
            return;
        };
        let response = self.answer_source_recovery(&envelope.request);
        // Save already completed. A lost ACK never makes it eligible to reapply.
        let _ = envelope.response.send(response);
    }
    fn answer_source_recovery(&mut self, request: &wire::Request) -> wire::Response {
        let mut result = None;
        let mut failure = None;
        if matches!(request, wire::Request::RetryResultSave { .. }) {
            if self.active.is_some() || !self.cleanups.is_empty() {
                failure = Some(wire::ErrorCode::Busy);
            } else if let Some(source) = &mut self.source {
                match source.retry_saved_result(&mut self.store, &self.epoch, request) {
                    Ok(proof) => {
                        result = Some(proof);
                        // Only a confirmed result reconciliation clears the failed
                        // store gate. Draining/disconnected remain set permanently.
                        self.failed = false;
                    }
                    Err(error) => failure = Some(error),
                }
            } else {
                failure = Some(wire::ErrorCode::NoRetainedReport);
            }
        }
        let mut target = self.source.as_ref().and_then(|s| s.recovery_target());
        let state = if failure.is_some() {
            wire::State::RecoveryRequired
        } else if self.active.is_some()
            || !self.cleanups.is_empty()
            || self.source.as_ref().is_some_and(|s| s.io_active())
        {
            wire::State::Working
        } else if target.is_some() {
            wire::State::ReportPending
        } else if self.failed || self.source_unsettled() {
            wire::State::RecoveryRequired
        } else if self.draining || self.disconnected.load(Ordering::Acquire) {
            wire::State::ReadyToExit
        } else {
            wire::State::Working
        };
        if state != wire::State::ReportPending {
            target = None;
        }
        let (project, session) = self.store.coordinates();
        wire::Response {
            version: ProtocolVersion,
            request_id: request.request_id().clone(),
            engine_epoch: self.epoch.clone(),
            project_id: project.clone(),
            session_id: session.clone(),
            state,
            result,
            retry_target: target,
            error: failure,
        }
    }
}
