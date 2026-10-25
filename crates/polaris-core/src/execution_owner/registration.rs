//! Host-local MCP registration: one durable approval and intent per proposal.
//! The worker saves while holding the grant; dropping it records the result.

use super::{APPROVAL_TTL_MS, StartDeadline, now_ms};
use crate::{
    desktop_store::{IntentReceipt, Published, StoreError, Writer},
    mcp::operation::ExternalCeiling,
};
use polaris_desktop_protocol::{
    event::{ApprovalExpired, EventBody, ExpiryReason},
    ids::{ApprovalId, DecimalU64, OperationId, ResultId},
    request::RunTarget,
    snapshot::{ApprovalDecision, ApprovalDisplay, PendingApproval, PendingApprovalState},
};
use std::{
    future::Future,
    time::{Duration, Instant},
};
use tokio::sync::{mpsc, oneshot};

pub const MCP_REGISTRATION_OPERATION: &str = "mcp.registration";
const MAX_DESCRIPTION_BYTES: usize = 16 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum RegistrationError {
    #[error("mcp_registration_denied")]
    Denied,
    #[error("mcp_registration_cancelled")]
    Cancelled,
    #[error("mcp_registration_invalid")]
    Invalid,
    #[error("mcp_registration_unavailable")]
    Unavailable,
}

/// Untruncated host display and the proposal's canonical hash.
pub struct RegistrationApproval {
    pub description: String,
    pub scope: String,
    pub payload_hash: String,
}

/// Held while saving. Dropping it lets the owner record the operation result.
pub struct RegistrationGrant(#[allow(dead_code)] oneshot::Sender<()>);

struct Request {
    approval: RegistrationApproval,
    reply: oneshot::Sender<Result<RegistrationGrant, RegistrationError>>,
}

#[derive(Clone)]
pub struct RegistrationPort {
    tx: mpsc::Sender<Request>,
}

impl RegistrationPort {
    pub async fn authorize(
        &self,
        approval: RegistrationApproval,
        cancelled: impl Future<Output = ()>,
    ) -> Result<RegistrationGrant, RegistrationError> {
        let (reply, result) = oneshot::channel();
        self.tx
            .try_send(Request { approval, reply })
            .map_err(|_| RegistrationError::Unavailable)?;
        tokio::select! {
            biased;
            _ = cancelled => Err(RegistrationError::Cancelled),
            result = result => result.map_err(|_| RegistrationError::Unavailable)?,
        }
    }
}

struct Awaiting {
    request: Request,
    approval: PendingApproval,
    deadline: StartDeadline,
    decision: Option<ApprovalDecision>,
}

pub(super) struct RegistrationRelay {
    incoming: mpsc::Receiver<Request>,
    ceiling: ExternalCeiling,
    awaiting: Option<Awaiting>,
    running: Vec<(OperationId, oneshot::Receiver<()>)>,
    next_operation: u64,
}

fn valid_hash(hash: &str) -> bool {
    hash.len() == 64
        && hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

impl RegistrationRelay {
    pub(super) fn new(ceiling: ExternalCeiling) -> (Self, RegistrationPort) {
        let (tx, incoming) = mpsc::channel(1);
        (
            Self {
                incoming,
                ceiling,
                awaiting: None,
                running: Vec::new(),
                next_operation: 0,
            },
            RegistrationPort { tx },
        )
    }

    pub(super) fn cancel(&mut self) {
        self.incoming.close();
    }

    pub(super) fn approval_resolved(&mut self, id: &ApprovalId, decision: ApprovalDecision) {
        if let Some(waiting) = &mut self.awaiting
            && &waiting.approval.approval_id == id
        {
            waiting.decision = Some(decision);
        }
    }

    pub(super) fn awaiting_approval(&self) -> bool {
        self.awaiting.is_some()
    }

    pub(super) fn quiescent(&self) -> bool {
        self.awaiting.is_none() && self.running.is_empty() && self.incoming.is_empty()
    }

    /// Records results for grants already dropped. The run owner calls this
    /// before deciding that an operation without a result is unknown.
    pub(super) fn record_finished(
        &mut self,
        store: &mut Writer,
        target: &RunTarget,
    ) -> Result<(), StoreError> {
        let mut index = 0;
        while index < self.running.len() {
            if matches!(
                self.running[index].1.try_recv(),
                Err(oneshot::error::TryRecvError::Empty)
            ) {
                index += 1;
                continue;
            }
            let (operation, _) = self.running.remove(index);
            store.record_operation_result(
                target,
                &operation,
                ResultId::new(format!("result-{}", operation.as_str()))?,
            )?;
        }
        Ok(())
    }

    pub(super) fn step(
        &mut self,
        store: &mut Writer,
        target: &RunTarget,
        published: &Published,
        events: &mut Vec<EventBody>,
        stopped: bool,
    ) -> Result<(), StoreError> {
        let current = published.state.policy_revision;
        let run_matches = published.state.runs.iter().any(|run| {
            run.run.run_id == target.run_id
                && run.run.attempt_id == target.attempt_id
                && !run.run.state.is_terminal()
                && run.policy_revision == current
        });
        self.record_finished(store, target)?;
        self.record_finished(store, target)?;
        if let Some(waiting) = &self.awaiting {
            let cancelled = stopped || waiting.request.reply.is_closed();
            let revoked = !run_matches || waiting.approval.policy_revision != current;
            if cancelled || revoked || !waiting.deadline.valid() {
                let waiting = self.awaiting.take().expect("awaiting registration");
                store.invalidate_approval(&waiting.approval.approval_id)?;
                events.push(EventBody::ApprovalExpired(ApprovalExpired {
                    approval_id: waiting.approval.approval_id,
                    reason: if cancelled {
                        ExpiryReason::Cancelled
                    } else if revoked {
                        ExpiryReason::PolicyRevoked
                    } else {
                        ExpiryReason::Expired
                    },
                }));
                let _ = waiting
                    .request
                    .reply
                    .send(Err(RegistrationError::Cancelled));
            } else if let Some(decision) = waiting.decision {
                let waiting = self.awaiting.take().expect("awaiting registration");
                if decision == ApprovalDecision::Deny {
                    let _ = waiting.request.reply.send(Err(RegistrationError::Denied));
                    return Ok(());
                }
                let approval = &waiting.approval;
                let receipt = store.consume_approval_intent(
                    target,
                    &approval.approval_id,
                    &approval.operation_id,
                    MCP_REGISTRATION_OPERATION,
                    &approval.scope,
                    &approval.payload_hash,
                    now_ms()?,
                )?;
                if receipt != IntentReceipt::NewlyPublished {
                    return Err(StoreError::RunConflict);
                }
                let (done, finished) = oneshot::channel();
                self.running.push((approval.operation_id.clone(), finished));
                // A departed caller drops the grant, so the next step records it.
                let _ = waiting.request.reply.send(Ok(RegistrationGrant(done)));
            }
            return Ok(());
        }
        let Ok(request) = self.incoming.try_recv() else {
            return Ok(());
        };
        let reject = |request: Request, error| -> Result<(), StoreError> {
            let _ = request.reply.send(Err(error));
            Ok(())
        };
        if stopped || !run_matches || request.reply.is_closed() {
            return reject(request, RegistrationError::Cancelled);
        }
        if self.ceiling == ExternalCeiling::Denied {
            return reject(request, RegistrationError::Denied);
        }
        let display = &request.approval;
        if display.description.is_empty()
            || display.description.len() > MAX_DESCRIPTION_BYTES
            || display.scope.is_empty()
            || display.scope.len() > 128
            || !valid_hash(&display.payload_hash)
        {
            return reject(request, RegistrationError::Invalid);
        }
        self.next_operation = self
            .next_operation
            .checked_add(1)
            .ok_or(StoreError::Overflow)?;
        let key = crate::conversation_state::content_hash(&serde_json::to_vec(&(
            "mcp-registration-v1",
            target,
            self.next_operation,
        ))?);
        let operation_id = OperationId::new(format!("mcpreg-{key}"))?;
        let now = now_ms()?;
        let approval = PendingApproval {
            approval_id: ApprovalId::new(format!("approval-{}", operation_id.as_str()))?,
            run_id: target.run_id.clone(),
            attempt_id: target.attempt_id.clone(),
            operation_id,
            operation: MCP_REGISTRATION_OPERATION.into(),
            scope: display.scope.clone(),
            payload_hash: display.payload_hash.clone(),
            policy_revision: current,
            expires_at_unix_ms: DecimalU64::new(
                now.checked_add(APPROVAL_TTL_MS)
                    .ok_or(StoreError::Overflow)?,
            ),
            state: PendingApprovalState::Pending,
            display: ApprovalDisplay {
                title: "Register MCP server".into(),
                description: display.description.clone(),
                choices: vec![ApprovalDecision::Allow, ApprovalDecision::Deny],
            },
        };
        if let Err(error) = store.publish_approval(approval.clone(), now) {
            let _ = request.reply.send(Err(RegistrationError::Unavailable));
            return Err(error);
        }
        events.push(EventBody::ApprovalRequested(approval.clone()));
        self.awaiting = Some(Awaiting {
            request,
            deadline: StartDeadline {
                unix_ms: approval.expires_at_unix_ms.get(),
                monotonic: Instant::now() + Duration::from_millis(APPROVAL_TTL_MS),
            },
            approval,
            decision: None,
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests;
