
use super::*;
use polaris_core::{
    management_effect::ManagementOperationIdV1 as Operation,
    sync::{
        push::{SyncPushHostV1, SyncPushOutcomeV1},
        remote_intent::SyncDigestV1,
    },
};

#[test]
fn c10_parser_preserves_explicit_recovery_and_rejects_extra_arguments() {
    let operation = Operation::random().unwrap();
    assert!(is_c10_sync_request(&format!("recover {operation}")));
    assert!(is_c10_sync_request(&format!("abandon {operation}")));
    assert!(!is_c10_sync_request(&format!("recover {}", "a".repeat(64))));
    assert!(matches!(
        c10_tui_invocation("push").unwrap(),
        C10TuiInvocation::Push
    ));
    assert!(
        matches!(c10_tui_invocation(&format!("recover {operation}")).unwrap(), C10TuiInvocation::Recover(value) if value == operation)
    );
    assert!(
        matches!(c10_tui_invocation(&format!("abandon {operation}")).unwrap(), C10TuiInvocation::Abandon(value) if value == operation)
    );
    for value in [
        "push unknown",
        "recover invalid",
        "push yes",
        "push recover",
    ] {
        assert!(c10_tui_invocation(value).is_err());
    }
    for value in [
        "/sync push",
        "/sync recover invalid",
        "/sync abandon invalid",
    ] {
        assert!(!slash::parse(value).unwrap().needs_management_clearance());
    }
}

#[test]
fn c10_owned_worker_recovery_binds_invocation_without_confirmation() {
    let main_thread = std::thread::current().id();
    let operation = Operation::random().unwrap();
    let result = c10_tui_worker(
        C10TuiInvocation::Recover(operation),
        |_| panic!("recover must not ask consent"),
        move |host| {
            assert_ne!(std::thread::current().id(), main_thread);
            host.require_recovery_invocation(operation).unwrap();
            assert!(
                host.require_recovery_invocation(Operation::random().unwrap())
                    .is_err()
            );
            assert!(
                host.require_abandon_invocation(operation, SyncDigestV1::digest(b"request"))
                    .is_err()
            );
            Ok("reconciled without send".into())
        },
    )
    .unwrap();
    assert_eq!(result, "reconciled without send");
}

#[test]
fn c10_owned_abandon_waits_for_exact_fresh_consent_and_retains_worker() {
    let main_thread = std::thread::current().id();
    let operation = Operation::random().unwrap();
    let request = SyncDigestV1::digest(b"durable request");
    let result = c10_tui_worker(
        C10TuiInvocation::Abandon(operation),
        |view| {
            assert_eq!(std::thread::current().id(), main_thread);
            assert!(
                view.details
                    .iter()
                    .any(|value| value.contains(&operation.as_hex()))
            );
            assert!(
                view.details
                    .iter()
                    .any(|value| value.contains(&request.to_string()))
            );
            true
        },
        move |host| {
            assert_ne!(std::thread::current().id(), main_thread);
            assert!(host.require_abandon_invocation(operation, request).is_err());
            host.confirm_abandon(operation, request).unwrap();
            host.require_abandon_invocation(operation, request).unwrap();
            assert!(
                host.require_abandon_invocation(operation, SyncDigestV1::digest(b"other request"))
                    .is_err()
            );
            assert!(host.require_recovery_invocation(operation).is_err());
            Ok("owned worker completed".into())
        },
    )
    .unwrap();
    assert_eq!(result, "owned worker completed");
}

#[test]
fn c10_worker_decline_or_failure_never_detaches_or_grants_consent() {
    let operation = Operation::random().unwrap();
    let error = c10_tui_worker(
        C10TuiInvocation::Abandon(operation),
        |_| false,
        move |host| {
            host.confirm_abandon(operation, SyncDigestV1::digest(b"request"))
                .map_err(|e| e.to_string())?;
            panic!("declined request continued")
        },
    )
    .unwrap_err();
    assert_eq!(error, "c10_permission_unavailable");
    assert!(
        c10_tui_worker(
            C10TuiInvocation::Push,
            |_| panic!("worker failed before prompt"),
            |_| { panic!("owned synthetic worker failure") }
        )
        .unwrap_err()
        .contains("worker failed")
    );
}

struct Keys(std::collections::VecDeque<ratatui::crossterm::event::KeyCode>);
impl approver::KeyReader for Keys {
    fn read_key(&mut self) -> std::io::Result<ratatui::crossterm::event::KeyCode> {
        self.0
            .pop_front()
            .ok_or_else(|| std::io::ErrorKind::UnexpectedEof.into())
    }
}

#[test]
fn c10_narrow_confirmation_requires_complete_wrapped_details() {
    use ratatui::crossterm::event::KeyCode;
    let terminal =
        RefCell::new(ratatui::Terminal::new(ratatui::backend::TestBackend::new(20, 10)).unwrap());
    let view = render::C10ConfirmView {
        title: "C10".into(),
        details: vec!["a".repeat(64), "b".repeat(64), "request end".into()],
    };
    let mut premature = Keys([KeyCode::Char('y')].into());
    assert!(!confirm_c10_sync(&terminal, &mut premature, &view));
    let mut keys = std::collections::VecDeque::from([KeyCode::Char('y')]);
    keys.extend(std::iter::repeat_n(KeyCode::Down, 12));
    keys.push_back(KeyCode::Char('y'));
    assert!(confirm_c10_sync(&terminal, &mut Keys(keys), &view));
    let mut cancel = Keys([KeyCode::Esc].into());
    assert!(!confirm_c10_sync(&terminal, &mut cancel, &view));
}

#[test]
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn c10_actual_gateway_rejects_queued_and_malformed_input_before_writes() {
    let root = tempfile::tempdir().unwrap();
    #[cfg(target_os = "macos")]
    {
        assert!(start_c10_sync_action(root.path(), root.path(), "push", true).is_err());
        assert_eq!(
            start_c10_sync_action(root.path(), root.path(), "recover invalid", false)
                .err()
                .unwrap(),
            "Invalid C10 operation ID"
        );
    }
    #[cfg(target_os = "linux")]
    {
        assert!(c10_unavailable_action("push", true).is_err());
        assert_eq!(
            c10_unavailable_action("recover invalid", false).unwrap_err(),
            "Invalid C10 operation ID"
        );
    }
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    let terminal =
        RefCell::new(ratatui::Terminal::new(ratatui::backend::TestBackend::new(80, 24)).unwrap());
    let session = polaris_core::session::Session::default();
    let mut status = Status::Notice(String::new());
    let mut lines = Vec::new();
    handle_sync_action(
        &terminal,
        &mut Keys([].into()),
        root.path(),
        root.path(),
        "recover invalid",
        false,
        &session,
        &mut status,
        &mut lines,
    );
    assert!(matches!(status, Status::Error(_)));
    assert!(session.messages.is_empty());
    assert!(lines.is_empty());
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}

#[test]
fn c10_unknown_and_partial_are_not_terminal_successes() {
    let operation_id = Operation::random().unwrap();
    assert!(c10_tui_outcome(SyncPushOutcomeV1::NoChanges).is_ok());
    for outcome in [
        SyncPushOutcomeV1::OutcomeUnknown { operation_id },
        SyncPushOutcomeV1::PartialComplete { operation_id },
    ] {
        let error = c10_tui_outcome(outcome).unwrap_err();
        assert!(error.contains(&format!("/sync recover {operation_id}")));
    }
}

#[test]
fn c10_pending_network_job_returns_to_main_loop_without_waiting() {
    let (release, blocked) = std::sync::mpsc::channel();
    let mut job = C10TuiJob::start(C10TuiInvocation::Push, move |_| {
        blocked.recv().unwrap();
        Ok("terminal".into())
    });
    let mut redraws = 0;
    for _ in 0..50 {
        assert!(
            job.poll(
                |_| panic!("unexpected preview"),
                |_| panic!("unexpected confirmation")
            )
            .is_none()
        );
        redraws += 1;
    }
    assert_eq!(redraws, 50);
    assert!(!job.worker.as_ref().unwrap().is_finished());
    release.send(()).unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if let Some(result) = job.poll(|_| true, |_| false) {
            assert_eq!(result.unwrap(), "terminal");
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert!(job.worker.is_none());
}

#[test]
fn c10_preview_ack_is_separate_from_prepared_confirmation_and_drop_reaps() {
    let order = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let worker_order = order.clone();
    let mut job = C10TuiJob::start(C10TuiInvocation::Push, move |host| {
        host.show_preview(render::C10ConfirmView {
            title: String::new(),
            details: vec!["preview".into()],
        })
        .unwrap();
        worker_order.lock().unwrap().push("prepared");
        assert!(
            host.ask(render::C10ConfirmView {
                title: "prepared".into(),
                details: Vec::new()
            })
            .unwrap()
        );
        Ok("terminal".into())
    });
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        if let Some(result) = job.poll(
            |_| {
                order.lock().unwrap().push("preview displayed");
                true
            },
            |_| {
                order.lock().unwrap().push("confirmed");
                true
            },
        ) {
            assert_eq!(result.unwrap(), "terminal");
            break;
        }
        assert!(std::time::Instant::now() < deadline);
        std::thread::yield_now();
    }
    assert_eq!(
        *order.lock().unwrap(),
        ["preview displayed", "prepared", "confirmed"]
    );
    let done = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let worker_done = done.clone();
    let abandoned = C10TuiJob::start(C10TuiInvocation::Push, move |host| {
        assert!(
            host.ask(render::C10ConfirmView {
                title: "pending".into(),
                details: Vec::new()
            })
            .is_err()
        );
        worker_done.store(true, std::sync::atomic::Ordering::Release);
        Err("UI closed".into())
    });
    drop(abandoned);
    assert!(done.load(std::sync::atomic::Ordering::Acquire));
}
