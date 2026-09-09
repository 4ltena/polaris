//! providerの借用deltaをdesktopの有界queueへ渡す表示adapter。
//! FinalTextは正本応答に任せ、全文の複製・二重連結を行わない。
use crate::desktop_events::{DesktopEventSink, EventFailure, EventSink, MAX_EVENT_BYTES};
use crate::events::AgentEvent;
use polaris_provider::{ObserverError, ResponseEvent, ResponseObserver};

/// AgentEventの固定領域を含めた既存queue上限に収まるdelta本文長。
pub const MAX_DELTA_BYTES: usize = MAX_EVENT_BYTES - std::mem::size_of::<AgentEvent>();

pub struct DesktopResponseObserver {
    sink: DesktopEventSink,
    request_id: u64,
}
impl DesktopResponseObserver {
    pub fn request_id(&self) -> u64 {
        self.request_id
    }
}
impl EventSink {
    /// provider要求ごとに作成する。子も同じBounded sinkのcloneを用いる。
    pub fn response_observer(&self) -> Result<Option<DesktopResponseObserver>, EventFailure> {
        match self {
            Self::Legacy(_) => Ok(None),
            Self::Bounded(sink) => Ok(Some(DesktopResponseObserver {
                request_id: sink.next_request_id()?,
                sink: sink.clone(),
            })),
        }
    }
}

fn observer_error(reason: EventFailure) -> ObserverError {
    match reason {
        EventFailure::Full => ObserverError::Full,
        EventFailure::ReceiverDropped => ObserverError::Closed,
        EventFailure::Oversize
        | EventFailure::SequenceExhausted
        | EventFailure::RequestIdExhausted
        | EventFailure::Cancelled => ObserverError::Cancelled,
    }
}
impl ResponseObserver for DesktopResponseObserver {
    fn try_emit(&self, event: ResponseEvent<'_>) -> Result<(), ObserverError> {
        // 正本はAgentOutcome/Session側。完了通知で既に得た応答をerror化しない。
        if matches!(event, ResponseEvent::FinalText { .. }) {
            return Ok(());
        }
        if let Some(reason) = self.sink.control().failure() {
            return Err(observer_error(reason));
        }
        match event {
            ResponseEvent::TextDelta {
                output_index,
                content_index,
                text,
            } => {
                // 借用strの長さだけを調べ、過大な本文をclone/serialize/debugしない。
                if text.len() > MAX_DELTA_BYTES {
                    return Err(observer_error(self.sink.fail(EventFailure::Oversize)));
                }
                EventSink::from(self.sink.clone())
                    .send(AgentEvent::TextDelta {
                        request_id: self.request_id,
                        output_index,
                        content_index,
                        text: text.to_owned(),
                    })
                    .map_err(observer_error)
            }
            ResponseEvent::FinalText { .. } => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::desktop_events::{DesktopEventReceiver, EVENT_CAPACITY, EventControl};
    use polaris_desktop_protocol::ids::RunId;

    fn channel() -> (EventSink, DesktopEventReceiver, EventControl) {
        EventSink::desktop(RunId::new("response-run").unwrap())
    }
    fn delta(text: &str) -> ResponseEvent<'_> {
        ResponseEvent::TextDelta {
            output_index: 2,
            content_index: 3,
            text,
        }
    }

    #[test]
    fn legacy_has_no_observer_and_keeps_queue_empty() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        assert!(EventSink::Legacy(tx).response_observer().unwrap().is_none());
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn indexed_delta_is_provisional_and_final_text_is_not_queued() {
        let (sink, mut rx, _) = channel();
        let observer = sink.response_observer().unwrap().unwrap();
        observer.try_emit(delta("仮本文")).unwrap();
        let envelope = rx.try_recv().unwrap();
        assert_eq!(envelope.run_id.as_str(), "response-run");
        assert_eq!(envelope.sequence, 1);
        match envelope.event {
            AgentEvent::TextDelta {
                request_id,
                output_index,
                content_index,
                text,
            } => {
                assert_eq!(request_id, observer.request_id());
                assert_eq!((output_index, content_index), (2, 3));
                assert_eq!(text, "仮本文");
            }
            other => panic!("unexpected event: {other:?}"),
        }
        let final_text = "x".repeat(MAX_EVENT_BYTES + 1);
        for delta_matches in [None, Some(true), Some(false)] {
            observer
                .try_emit(ResponseEvent::FinalText {
                    text: &final_text,
                    delta_matches,
                })
                .unwrap();
        }
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn request_ids_are_shared_across_concurrent_clones_and_never_reused() {
        let (sink, _rx, _) = channel();
        let ids = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..4)
                .map(|_| {
                    let child = sink.clone();
                    scope.spawn(move || {
                        (0..8)
                            .map(|_| child.response_observer().unwrap().unwrap().request_id())
                            .collect::<Vec<_>>()
                    })
                })
                .collect();
            handles
                .into_iter()
                .flat_map(|h| h.join().unwrap())
                .collect::<Vec<_>>()
        });
        let mut ids = ids;
        ids.sort_unstable();
        assert_eq!(ids, (1..=32).collect::<Vec<_>>());
        assert_eq!(sink.response_observer().unwrap().unwrap().request_id(), 33);
    }

    #[tokio::test]
    async fn size_boundary_and_oversize_keep_first_failure_and_cancel() {
        let (sink, mut rx, control) = channel();
        let observer = sink.response_observer().unwrap().unwrap();
        let accepted = "x".repeat(MAX_DELTA_BYTES);
        observer.try_emit(delta(&accepted)).unwrap();
        assert!(rx.try_recv().is_ok());
        let oversized = "x".repeat(MAX_DELTA_BYTES + 1);
        assert_eq!(
            observer.try_emit(delta(&oversized)),
            Err(ObserverError::Cancelled)
        );
        assert_eq!(control.failure(), Some(EventFailure::Oversize));
        assert!(rx.try_recv().is_err());
        control.cancel();
        drop(rx);
        assert_eq!(control.failure(), Some(EventFailure::Oversize));
        assert!(matches!(
            sink.response_observer(),
            Err(EventFailure::Oversize)
        ));
        assert_eq!(
            observer.try_emit(ResponseEvent::FinalText {
                text: "final",
                delta_matches: None
            }),
            Ok(())
        );
        tokio::time::timeout(std::time::Duration::from_secs(1), control.cancelled())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn full_drop_and_cancel_reject_deltas_but_preserve_completed_response() {
        for reason in [
            EventFailure::Full,
            EventFailure::ReceiverDropped,
            EventFailure::Cancelled,
        ] {
            let (sink, rx, control) = channel();
            let observer = sink.response_observer().unwrap().unwrap();
            match reason {
                EventFailure::Full => {
                    for _ in 0..EVENT_CAPACITY {
                        observer.try_emit(delta("x")).unwrap();
                    }
                    assert_eq!(observer.try_emit(delta("x")), Err(ObserverError::Full));
                }
                EventFailure::ReceiverDropped => drop(rx),
                EventFailure::Cancelled => control.cancel(),
                _ => unreachable!(),
            }
            assert_eq!(control.failure(), Some(reason));
            assert_eq!(
                observer.try_emit(delta("later")),
                Err(observer_error(reason))
            );
            assert_eq!(
                observer.try_emit(ResponseEvent::FinalText {
                    text: "final",
                    delta_matches: Some(true)
                }),
                Ok(())
            );
            assert!(matches!(sink.response_observer(), Err(failure) if failure == reason));
            tokio::time::timeout(std::time::Duration::from_secs(1), control.cancelled())
                .await
                .unwrap();
        }
    }
}
