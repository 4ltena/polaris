//! Desktopキューだけを有界にする。provider/tool内のbufferは対象外。
use crate::events::AgentEvent;
use polaris_desktop_protocol::ids::{AttemptId, MAX_ID_BYTES, RunId};
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, watch};

pub const EVENT_CAPACITY: usize = 64;
pub const MAX_EVENT_BYTES: usize = 256 * 1024;

#[derive(Debug)]
pub struct EventEnvelope {
    pub run_id: RunId,
    pub sequence: u64,
    pub child: Option<ChildIdentity>,
    pub event: AgentEvent,
}

/// 表示上の子identity。envelopeのroot run・execution所有とは別に扱う。
/// SpawnStartedから同じIDのSpawnFinishedまで保持する。Futureのdropは終端ではない。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildIdentity {
    pub run_id: RunId,
    pub attempt_id: AttemptId,
    pub parent_run_id: RunId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventFailure {
    Full,
    ReceiverDropped,
    Oversize,
    SequenceExhausted,
    RequestIdExhausted,
    Cancelled,
}
impl std::fmt::Display for EventFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{self:?}")
    }
}
impl std::error::Error for EventFailure {}

struct State {
    sequence: u64,
    request_id: u64,
    failure: Option<EventFailure>,
}
struct Shared {
    state: Mutex<State>,
    cancelled: watch::Sender<bool>,
}
impl Shared {
    fn fail(&self, state: &mut State, reason: EventFailure) -> EventFailure {
        let reason = *state.failure.get_or_insert(reason);
        self.cancelled.send_replace(true);
        reason
    }
}

#[derive(Clone)]
pub struct EventControl(Arc<Shared>);
impl EventControl {
    pub fn failure(&self) -> Option<EventFailure> {
        self.0.state.lock().unwrap().failure
    }
    pub fn cancel(&self) {
        let mut state = self.0.state.lock().unwrap();
        self.0.fail(&mut state, EventFailure::Cancelled);
    }
    /// 既に取消済みの場合も即座に返る。複数waiterに対応する。
    pub async fn cancelled(&self) {
        let mut rx = self.0.cancelled.subscribe();
        let _ = rx.wait_for(|cancelled| *cancelled).await;
    }
}

#[derive(Clone)]
pub struct DesktopEventSink {
    run_id: RunId,
    child: Option<ChildIdentity>,
    tx: mpsc::Sender<EventEnvelope>,
    control: EventControl,
    execution: Option<crate::desktop_execution::ExecutionPort>,
}
#[derive(Clone)]
pub enum EventSink {
    Legacy(mpsc::UnboundedSender<AgentEvent>),
    Bounded(DesktopEventSink),
}
impl From<mpsc::UnboundedSender<AgentEvent>> for EventSink {
    fn from(tx: mpsc::UnboundedSender<AgentEvent>) -> Self {
        Self::Legacy(tx)
    }
}
impl DesktopEventSink {
    /// rootごとの一意counterを使い、task本文や型名には依存しない。
    /// rootはserviceのepoch付きID。hashで128 byteのrootも固定長に収める。
    pub(crate) fn child(&self) -> Result<Self, EventFailure> {
        let id = self.next_request_id()?;
        let namespace = crate::conversation_state::content_hash(self.run_id.as_str().as_bytes());
        let mut sink = self.clone();
        sink.child = Some(ChildIdentity {
            run_id: RunId::new(format!("child-{namespace}-{id}")).expect("bounded child ID"),
            attempt_id: AttemptId::new(format!("child-attempt-{namespace}-{id}"))
                .expect("bounded attempt ID"),
            parent_run_id: self
                .child
                .as_ref()
                .map_or(&self.run_id, |child| &child.run_id)
                .clone(),
        });
        Ok(sink)
    }

    pub fn with_execution(
        mut self,
        port: crate::desktop_execution::ExecutionPort,
    ) -> Result<Self, EventFailure> {
        if port.run_id() != &self.run_id {
            return Err(EventFailure::Cancelled);
        }
        self.execution = Some(port);
        Ok(self)
    }
    pub(crate) fn execution(&self) -> Option<&crate::desktop_execution::ExecutionPort> {
        self.execution.as_ref()
    }
    /// 同じrunのclone・子・再試行で共有する。失敗時にも番号を再利用しない。
    pub(crate) fn next_request_id(&self) -> Result<u64, EventFailure> {
        let shared = &self.control.0;
        let mut state = shared.state.lock().unwrap();
        if let Some(reason) = state.failure {
            return Err(reason);
        }
        let Some(id) = state.request_id.checked_add(1) else {
            return Err(shared.fail(&mut state, EventFailure::RequestIdExhausted));
        };
        state.request_id = id;
        Ok(id)
    }

    pub(crate) fn fail(&self, reason: EventFailure) -> EventFailure {
        let shared = &self.control.0;
        let mut state = shared.state.lock().unwrap();
        shared.fail(&mut state, reason)
    }

    pub fn control(&self) -> EventControl {
        self.control.clone()
    }

    pub fn channel(run_id: RunId) -> (Self, DesktopEventReceiver, EventControl) {
        let (tx, rx) = mpsc::channel(EVENT_CAPACITY);
        let (cancelled, _) = watch::channel(false);
        let control = EventControl(Arc::new(Shared {
            state: Mutex::new(State {
                sequence: 0,
                request_id: 0,
                failure: None,
            }),
            cancelled,
        }));
        // IDの元Stringの余剰capacityを持ち込まない。
        let run_id = RunId::new(run_id.as_str()).expect("validated RunId");
        (
            Self {
                run_id,
                child: None,
                tx,
                control: control.clone(),
                execution: None,
            },
            DesktopEventReceiver {
                rx,
                control: control.clone(),
            },
            control,
        )
    }
}
impl From<DesktopEventSink> for EventSink {
    fn from(sink: DesktopEventSink) -> Self {
        Self::Bounded(sink)
    }
}
impl EventSink {
    pub fn desktop(run_id: RunId) -> (Self, DesktopEventReceiver, EventControl) {
        let (sink, receiver, control) = DesktopEventSink::channel(run_id);
        (Self::Bounded(sink), receiver, control)
    }
    /// キュー空きを待たない。最初の失敗以後はenqueueしない。
    pub fn send(&self, event: AgentEvent) -> Result<(), EventFailure> {
        let Self::Bounded(sink) = self else {
            let Self::Legacy(tx) = self else {
                unreachable!()
            };
            return tx.send(event).map_err(|_| EventFailure::ReceiverDropped);
        };
        let shared = &sink.control.0;
        let mut state = shared.state.lock().unwrap();
        if let Some(failure) = state.failure {
            return Err(failure);
        }
        // 既存rootのpayload予算を維持し、子metadata分だけ追加で差し引く。
        let overhead = if sink.child.is_some() {
            std::mem::size_of::<ChildIdentity>() + 3 * MAX_ID_BYTES
        } else {
            0
        };
        if !event.fits_queue_budget(MAX_EVENT_BYTES.saturating_sub(overhead)) {
            return Err(shared.fail(&mut state, EventFailure::Oversize));
        }
        let Some(sequence) = state.sequence.checked_add(1) else {
            return Err(shared.fail(&mut state, EventFailure::SequenceExhausted));
        };
        let envelope = EventEnvelope {
            run_id: sink.run_id.clone(),
            sequence,
            child: sink.child.clone(),
            event,
        };
        match sink.tx.try_send(envelope) {
            Ok(()) => {
                state.sequence = sequence;
                Ok(())
            }
            Err(mpsc::error::TrySendError::Full(_)) => {
                Err(shared.fail(&mut state, EventFailure::Full))
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                Err(shared.fail(&mut state, EventFailure::ReceiverDropped))
            }
        }
    }
}

pub struct DesktopEventReceiver {
    rx: mpsc::Receiver<EventEnvelope>,
    control: EventControl,
}
impl DesktopEventReceiver {
    pub async fn recv(&mut self) -> Option<EventEnvelope> {
        self.rx.recv().await
    }
    pub fn try_recv(&mut self) -> Result<EventEnvelope, mpsc::error::TryRecvError> {
        self.rx.try_recv()
    }
}
impl Drop for DesktopEventReceiver {
    fn drop(&mut self) {
        let mut state = self.control.0.state.lock().unwrap();
        self.control
            .0
            .fail(&mut state, EventFailure::ReceiverDropped);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn child_identity_shares_order_requests_and_root_execution_scope() {
        let root = RunId::new("r".repeat(MAX_ID_BYTES)).unwrap();
        let (sink, mut rx, control) = DesktopEventSink::channel(root.clone());
        let (port, _requests) = crate::desktop_execution::ExecutionPort::channel(root.clone());
        let sink = sink.with_execution(port).unwrap();
        let first = sink.child().unwrap();
        let second = sink.clone().child().unwrap();
        assert_ne!(first.child, second.child);
        assert_eq!(first.child.as_ref().unwrap().parent_run_id, root);
        assert_eq!(first.execution().unwrap().run_id(), &root);
        let nested = first.child().unwrap();
        assert_eq!(
            nested.child.as_ref().unwrap().parent_run_id,
            first.child.as_ref().unwrap().run_id
        );
        assert_eq!(first.next_request_id(), Ok(4));
        assert_eq!(second.next_request_id(), Ok(5));
        for source in [&sink, &first, &second, &nested] {
            EventSink::from(source.clone()).send(event()).unwrap();
        }
        for (index, source) in [&sink, &first, &second, &nested].into_iter().enumerate() {
            let envelope = rx.try_recv().unwrap();
            assert_eq!(envelope.run_id, root);
            assert_eq!(envelope.sequence, index as u64 + 1);
            assert_eq!(envelope.child, source.child);
        }
        let (wrong, _requests) = crate::desktop_execution::ExecutionPort::channel(
            first.child.as_ref().unwrap().run_id.clone(),
        );
        assert!(first.clone().with_execution(wrong).is_err());
        let (reopened, _rx, _) = DesktopEventSink::channel(RunId::new("new-epoch-run").unwrap());
        assert_ne!(first.child, reopened.child().unwrap().child);
        control.cancel();
        assert!(matches!(first.child(), Err(EventFailure::Cancelled)));
        assert_eq!(
            EventSink::from(second).send(event()),
            Err(EventFailure::Cancelled)
        );
    }

    #[test]
    fn child_counter_overflow_is_sticky() {
        let (sink, _rx, control) = DesktopEventSink::channel(RunId::new("root").unwrap());
        control.0.state.lock().unwrap().request_id = u64::MAX - 1;
        assert!(sink.child().is_ok());
        assert!(matches!(
            sink.clone().child(),
            Err(EventFailure::RequestIdExhausted)
        ));
        assert_eq!(
            EventSink::from(sink).send(event()),
            Err(EventFailure::RequestIdExhausted)
        );
    }

    #[test]
    fn concurrent_child_allocation_and_full_queue_share_one_boundary() {
        let (sink, mut rx, control) = DesktopEventSink::channel(RunId::new("parallel").unwrap());
        std::thread::scope(|scope| {
            for _ in 0..8 {
                let sink = sink.clone();
                scope.spawn(move || {
                    for _ in 0..8 {
                        EventSink::from(sink.child().unwrap())
                            .send(event())
                            .unwrap();
                    }
                });
            }
        });
        let extra = sink.child().unwrap();
        assert_eq!(
            EventSink::from(extra).send(event()),
            Err(EventFailure::Full)
        );
        let mut identities = std::collections::HashSet::new();
        for sequence in 1..=EVENT_CAPACITY as u64 {
            let envelope = rx.try_recv().unwrap();
            assert_eq!(envelope.sequence, sequence);
            assert!(identities.insert(envelope.child.unwrap().run_id));
        }
        assert_eq!(control.failure(), Some(EventFailure::Full));
        assert!(matches!(sink.child(), Err(EventFailure::Full)));
    }

    #[test]
    fn child_metadata_counts_toward_queue_budget() {
        let (sink, mut rx, control) = DesktopEventSink::channel(RunId::new("root").unwrap());
        let sink = EventSink::from(sink.child().unwrap());
        let event = AgentEvent::SpawnFinished {
            agent_type: "x".repeat(MAX_EVENT_BYTES - std::mem::size_of::<AgentEvent>()),
            ok: true,
        };
        assert!(event.fits_queue_budget(MAX_EVENT_BYTES));
        assert_eq!(sink.send(event), Err(EventFailure::Oversize));
        assert_eq!(control.failure(), Some(EventFailure::Oversize));
        assert!(rx.try_recv().is_err());
    }

    fn event() -> AgentEvent {
        AgentEvent::SpawnFinished {
            agent_type: "test".into(),
            ok: true,
        }
    }
    fn channel() -> (EventSink, DesktopEventReceiver, EventControl) {
        EventSink::desktop(RunId::new("run-test").unwrap())
    }
    #[tokio::test]
    async fn desktop_entry_control_is_the_same_cancellation_boundary() {
        let (sink, _receiver, control) = DesktopEventSink::channel(RunId::new("run").unwrap());
        let from_sink = sink.control();
        from_sink.cancel();
        assert_eq!(control.failure(), Some(EventFailure::Cancelled));
        assert_eq!(
            EventSink::from(sink).send(event()),
            Err(EventFailure::Cancelled)
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), control.cancelled())
            .await
            .unwrap();
    }

    #[test]
    fn request_id_overflow_cancels_without_reusing_ids() {
        let (sink, _receiver, control) = DesktopEventSink::channel(RunId::new("run").unwrap());
        control.0.state.lock().unwrap().request_id = u64::MAX - 1;
        assert_eq!(sink.next_request_id(), Ok(u64::MAX));
        assert_eq!(
            sink.clone().next_request_id(),
            Err(EventFailure::RequestIdExhausted)
        );
        control.cancel();
        assert_eq!(control.failure(), Some(EventFailure::RequestIdExhausted));
    }

    #[test]
    fn full_is_sticky_and_queue_is_bounded() {
        let (tx, mut rx, control) = channel();
        for _ in 0..EVENT_CAPACITY {
            tx.send(event()).unwrap();
        }
        assert_eq!(tx.send(event()), Err(EventFailure::Full));
        assert_eq!(control.failure(), Some(EventFailure::Full));
        for sequence in 1..=EVENT_CAPACITY as u64 {
            let envelope = rx.try_recv().unwrap();
            assert_eq!(envelope.sequence, sequence);
            assert_eq!(envelope.run_id.as_str(), "run-test");
        }
        assert_eq!(tx.clone().send(event()), Err(EventFailure::Full));
        assert!(rx.try_recv().is_err());
    }
    #[tokio::test]
    async fn receiver_drop_notifies_existing_and_late_waiters() {
        let (tx, rx, control) = channel();
        let waiter_control = control.clone();
        let waiter = tokio::spawn(async move { waiter_control.cancelled().await });
        tokio::task::yield_now().await;
        drop(rx);
        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .unwrap()
            .unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(1), control.cancelled())
            .await
            .unwrap();
        assert_eq!(tx.send(event()), Err(EventFailure::ReceiverDropped));
    }
    #[tokio::test]
    async fn full_and_oversize_notify_cancellation() {
        for oversized in [false, true] {
            let (tx, _rx, control) = channel();
            let failure = if oversized {
                tx.send(AgentEvent::SpawnFinished {
                    agent_type: "x".repeat(MAX_EVENT_BYTES + 1),
                    ok: true,
                })
            } else {
                for _ in 0..EVENT_CAPACITY {
                    tx.send(event()).unwrap();
                }
                tx.send(event())
            };
            assert_eq!(
                failure,
                Err(if oversized {
                    EventFailure::Oversize
                } else {
                    EventFailure::Full
                })
            );
            tokio::time::timeout(std::time::Duration::from_secs(1), control.cancelled())
                .await
                .unwrap();
        }
    }

    #[test]
    fn oversized_spare_capacity_rejected_without_enqueue() {
        let (tx, mut rx, control) = channel();
        let huge = AgentEvent::SpawnFinished {
            agent_type: String::with_capacity(MAX_EVENT_BYTES + 1),
            ok: true,
        };
        assert_eq!(tx.send(huge), Err(EventFailure::Oversize));
        assert_eq!(control.failure(), Some(EventFailure::Oversize));
        assert!(rx.try_recv().is_err());
    }
    #[test]
    fn clones_preserve_delivery_sequence() {
        let (tx, mut rx, _) = channel();
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let tx = tx.clone();
                scope.spawn(move || {
                    for _ in 0..8 {
                        tx.send(event()).unwrap();
                    }
                });
            }
        });
        for expected in 1..=32 {
            assert_eq!(rx.try_recv().unwrap().sequence, expected);
        }
    }
    #[test]
    fn explicit_cancel_and_overflow_fail_closed() {
        let (tx, _, control) = channel();
        // receiverを保持する別channelで取消を検証。
        assert_eq!(control.failure(), Some(EventFailure::ReceiverDropped));
        assert!(tx.send(event()).is_err());
        let (tx, _rx, control) = channel();
        control.cancel();
        assert_eq!(tx.send(event()), Err(EventFailure::Cancelled));
        let (tx, _rx, control) = channel();
        control.0.state.lock().unwrap().sequence = u64::MAX;
        assert_eq!(tx.send(event()), Err(EventFailure::SequenceExhausted));
    }
    #[test]
    fn legacy_preserves_unbounded_delivery_and_reports_disconnect() {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tx = EventSink::Legacy(tx);
        for _ in 0..EVENT_CAPACITY + 1 {
            tx.send(event()).unwrap();
        }
        for _ in 0..EVENT_CAPACITY + 1 {
            assert!(rx.try_recv().is_ok());
        }
        drop(rx);
        assert_eq!(tx.send(event()), Err(EventFailure::ReceiverDropped));
    }
}
