//! Bounded approval mailbox. The controller owns the pending request and
//! answers asynchronously; this module never grants permission on disconnect.
use crate::approval::{Approver, Decision};
use crate::desktop_events::EventControl;
use polaris_desktop_protocol::ids::{ApprovalId, RunId};
use std::{future::Future, pin::Pin, time::Duration};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

const MAX_REASON_BYTES: usize = 4096;

pub struct DesktopApprover {
    run_id: RunId,
    policy_revision: u64,
    next_id: u64,
    timeout: Duration,
    control: EventControl,
    tx: mpsc::Sender<PendingApproval>,
}

pub struct PendingApproval {
    run_id: RunId,
    approval_id: ApprovalId,
    policy_revision: u64,
    reason: String,
    deadline: Instant,
    control: EventControl,
    answer: oneshot::Sender<Decision>,
}
impl PendingApproval {
    pub fn run_id(&self) -> &RunId {
        &self.run_id
    }
    pub fn approval_id(&self) -> &ApprovalId {
        &self.approval_id
    }
    pub fn policy_revision(&self) -> u64 {
        self.policy_revision
    }
    pub fn reason(&self) -> &str {
        &self.reason
    }

    /// Supply the engine's current policy revision, not a UI-provided value.
    /// Consuming the request makes answers one-shot and tied to its identity.
    pub fn resolve(self, current_policy_revision: u64, decision: Decision) -> bool {
        let allowed = current_policy_revision == self.policy_revision
            && Instant::now() < self.deadline
            && self.control.failure().is_none();
        self.answer
            .send(if allowed { decision } else { Decision::Deny })
            .is_ok()
            && allowed
    }
}
impl DesktopApprover {
    pub fn channel(
        run_id: RunId,
        policy_revision: u64,
        timeout: Duration,
        control: EventControl,
    ) -> (Self, mpsc::Receiver<PendingApproval>) {
        let (tx, rx) = mpsc::channel(1);
        (
            Self {
                run_id,
                policy_revision,
                timeout,
                control,
                tx,
                next_id: 0,
            },
            rx,
        )
    }
    async fn request(&mut self, reason: &str) -> Decision {
        if reason.len() > MAX_REASON_BYTES || self.control.failure().is_some() {
            return Decision::Deny;
        }
        let Some(next) = self.next_id.checked_add(1) else {
            return Decision::Deny;
        };
        self.next_id = next;
        let Some(deadline) = Instant::now().checked_add(self.timeout) else {
            return Decision::Deny;
        };
        let (answer, response) = oneshot::channel();
        let pending = PendingApproval {
            run_id: self.run_id.clone(),
            approval_id: ApprovalId::new(format!("approval-{next}")).unwrap(),
            policy_revision: self.policy_revision,
            reason: reason.into(),
            deadline,
            control: self.control.clone(),
            answer,
        };
        if self.tx.try_send(pending).is_err() {
            return Decision::Deny;
        }
        tokio::select! {
            biased;
            _ = self.control.cancelled() => Decision::Deny,
            _ = self.tx.closed() => Decision::Deny,
            _ = tokio::time::sleep_until(deadline) => Decision::Deny,
            answer = response => {
                if self.control.failure().is_some() || Instant::now() >= deadline { Decision::Deny }
                else { answer.unwrap_or(Decision::Deny) }
            }
        }
    }
}
impl Approver for DesktopApprover {
    // A synchronous/legacy caller must never accidentally approve or block.
    fn ask(&mut self, _: &str) -> Decision {
        Decision::Deny
    }
    fn ask_async<'a>(
        &'a mut self,
        reason: &'a str,
    ) -> Pin<Box<dyn Future<Output = Decision> + 'a>> {
        Box::pin(self.request(reason))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop_events::DesktopEventSink;

    #[tokio::test]
    async fn controller_answer_is_scoped_one_shot_and_policy_checked() {
        for current in [7, 8] {
            let id = RunId::new("test-run").unwrap();
            let (_sink, _events, control) = DesktopEventSink::channel(id.clone());
            let (mut approver, mut requests) =
                DesktopApprover::channel(id, 7, Duration::from_secs(2), control);
            let controller = async {
                let request = requests.recv().await.unwrap();
                assert_eq!(request.run_id.as_str(), "test-run");
                assert_eq!(request.approval_id.as_str(), "approval-1");
                assert_eq!(request.reason, "dummy command");
                request.resolve(current, Decision::Allow);
            };
            let (answer, ()) = tokio::join!(approver.ask_async("dummy command"), controller);
            assert_eq!(
                answer,
                if current == 7 {
                    Decision::Allow
                } else {
                    Decision::Deny
                }
            );
        }
    }
    #[tokio::test]
    async fn cancellation_and_late_answer_do_not_grant_permission() {
        let id = RunId::new("cancel-run").unwrap();
        let (_sink, _events, control) = DesktopEventSink::channel(id.clone());
        let (mut approver, mut requests) =
            DesktopApprover::channel(id, 1, Duration::from_secs(2), control.clone());
        let controller = async {
            let pending = requests.recv().await.unwrap();
            control.cancel();
            assert!(!pending.resolve(1, Decision::Allow));
        };
        let (answer, ()) = tokio::join!(approver.ask_async("dummy"), controller);
        assert_eq!(answer, Decision::Deny);
    }
    #[tokio::test]
    async fn expiry_disconnect_and_oversized_reason_fail_closed() {
        let id = RunId::new("timeout-run").unwrap();
        let (_sink, _events, control) = DesktopEventSink::channel(id.clone());
        let (mut approver, requests) = DesktopApprover::channel(id, 1, Duration::ZERO, control);
        assert_eq!(approver.ask_async("dummy").await, Decision::Deny);
        drop(requests);
        assert_eq!(approver.ask_async("dummy").await, Decision::Deny);
        assert_eq!(
            approver.ask_async(&"x".repeat(MAX_REASON_BYTES + 1)).await,
            Decision::Deny
        );
    }
}
