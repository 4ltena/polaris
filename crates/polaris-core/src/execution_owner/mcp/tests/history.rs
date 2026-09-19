//! History uses the existing lane without an operation, grant or publication.
use super::*;
use std::future::pending;

#[tokio::test]
async fn history_lane_reads_fixed_owner_without_writes_or_operation_numbers() {
    let (_root, mut writer, target, mut owner, _dir) = super::super::super::tests::read_owner();
    let port = configured(&mut owner, &target);
    let before = writer.snapshot().unwrap();
    let mut request = Box::pin(port.history(pending()));
    assert!(request.as_mut().now_or_never().is_none());
    assert!(!owner.quiescent());
    owner.step(&mut writer, &target).unwrap();
    let history = request.as_mut().now_or_never().unwrap().unwrap();
    assert_eq!(history.target(), &target);
    assert_eq!(history.marker(), &before.marker);
    assert_eq!(history.query(), "dummy");
    assert_eq!(history.messages().len(), 1);
    assert_eq!(history.messages()[0].content, "dummy");
    assert_eq!(writer.snapshot().unwrap().marker, before.marker);
    assert_eq!(owner.mcp.as_ref().unwrap().next_operation, 0);
    assert!(owner.quiescent());
}

#[tokio::test]
async fn history_lane_wait_and_queued_cancellation_release_capacity_without_writes() {
    for queued in [false, true] {
        let (_root, mut writer, target, mut owner, _dir) = super::super::super::tests::read_owner();
        let port = configured(&mut owner, &target);
        let before = writer.snapshot().unwrap().marker;
        let held = (!queued).then(|| port.reserve_metadata().unwrap());
        let (cancel, cancelled) = oneshot::channel::<()>();
        let mut request = Box::pin(port.history(async {
            let _ = cancelled.await;
        }));
        assert!(request.as_mut().now_or_never().is_none());
        assert_eq!(
            owner.mcp.as_ref().unwrap().incoming.len(),
            usize::from(queued)
        );
        cancel.send(()).unwrap();
        assert_eq!(request.await.unwrap_err(), McpCallError::Cancelled);
        drop(held);
        owner.step(&mut writer, &target).unwrap();
        assert!(owner.quiescent());
        assert!(!port.stopped());
        assert_eq!(writer.snapshot().unwrap().marker, before);
        assert_eq!(owner.mcp.as_ref().unwrap().next_operation, 0);
    }
}

#[tokio::test]
async fn history_lane_dropped_reply_is_discarded_before_projecting() {
    // A selected but unconfirmed prompt would make the projector fail.
    let target = RunTarget {
        run_id: RunId::new("pending").unwrap(),
        attempt_id: AttemptId::new("attempt").unwrap(),
    };
    let (_root, mut writer, target, mut owner, _dir) =
        super::super::super::tests::read_owner_with_prompt(
            target,
            Some(polaris_desktop_protocol::request::McpPromptChoice {
                server: "fixture".into(),
                prompt: "prompt".into(),
                arguments: Default::default(),
            }),
        );
    let port = configured(&mut owner, &target);
    let before = writer.snapshot().unwrap().marker;
    let mut request = Box::pin(port.history(pending()));
    assert!(request.as_mut().now_or_never().is_none());
    drop(request);
    owner.step(&mut writer, &target).unwrap();
    assert!(!port.stopped());
    assert!(owner.quiescent());
    assert_eq!(writer.snapshot().unwrap().marker, before);
    assert_eq!(owner.mcp.as_ref().unwrap().next_operation, 0);
}

#[tokio::test]
async fn history_lane_owner_stop_wins_over_already_queued_success() {
    let (_root, mut writer, target, mut owner, _dir) = super::super::super::tests::read_owner();
    let port = configured(&mut owner, &target);
    let mut request = Box::pin(port.history(pending()));
    assert!(request.as_mut().now_or_never().is_none());
    owner.step(&mut writer, &target).unwrap();
    owner.mcp.as_mut().unwrap().cancel();
    assert_eq!(request.await.unwrap_err(), McpCallError::Cancelled);
}
