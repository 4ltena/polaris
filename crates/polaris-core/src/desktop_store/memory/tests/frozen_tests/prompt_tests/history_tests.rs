//! Inherited request provenance uses real forks and exact fixed raw copies.
use super::*;

fn parent(inherited: u64) -> (PrototypeRoot, Writer) {
    let (root, mut parent, _unused_reader) = selected_after_inheritance(2, inherited);
    let saved = parent.snapshot().unwrap();
    let run = &saved.state.runs.last().unwrap().run;
    parent
        .finish(
            &RunTarget {
                run_id: run.run_id.clone(),
                attempt_id: run.attempt_id.clone(),
            },
            Observation::Succeeded,
            ResultId::new("prompt-finished").unwrap(),
        )
        .unwrap();
    (root, parent)
}

#[test]
fn request_history_proves_multifork_prompt_through_parents_without_local_runs() {
    for inherited in [0, 1, 10] {
        let (_root, parent) = parent(inherited);
        let child = parent
            .fork_memory(SessionId::new("history-child").unwrap(), "fixture")
            .unwrap();
        let mut grandchild = child
            .fork_memory(SessionId::new("history-grandchild").unwrap(), "fixture")
            .unwrap();
        assert!(child.snapshot().unwrap().state.runs.is_empty());
        drop(child);
        drop(parent);
        let target = accept_text(&mut grandchild, 99, "current real query");
        let before = grandchild.snapshot().unwrap();
        let history = grandchild.project_request_history(&target).unwrap();
        assert_eq!(history.query(), "current real query");
        assert_eq!(
            history.messages().last().unwrap().content,
            "current real query"
        );
        assert_eq!(
            history
                .messages()
                .iter()
                .filter(|m| m.content.contains("untrusted server supplement"))
                .count(),
            1
        );
        assert_eq!(before.marker, grandchild.snapshot().unwrap().marker);
        assert_eq!(
            serde_json::to_value(before.raw).unwrap(),
            serde_json::to_value(grandchild.snapshot().unwrap().raw).unwrap()
        );
    }
}

#[test]
fn request_history_rejects_deleted_or_busy_parent_without_changing_child() {
    let (_root, mut parent) = parent(0);
    let mut child = parent
        .fork_memory(SessionId::new("history-child").unwrap(), "fixture")
        .unwrap();
    let target = accept_text(&mut child, 99, "real query");
    let before = bytes(&child.test_directory());
    assert!(matches!(
        child.project_request_history(&target),
        Err(StoreError::Busy)
    ));
    parent.tombstone().unwrap();
    drop(parent);
    assert!(matches!(
        child.project_request_history(&target),
        Err(StoreError::Deleted)
    ));
    assert_eq!(bytes(&child.test_directory()), before);
}

#[test]
fn request_history_rejects_changed_inherited_message_after_restart() {
    let (root, parent) = parent(0);
    let child = parent
        .fork_memory(SessionId::new("history-child").unwrap(), "fixture")
        .unwrap();
    let mut saved = child.snapshot().unwrap();
    let path = child.test_directory();
    drop(child);
    drop(parent);
    saved.raw[0]
        .message
        .content
        .push_str(" changed inherited copy");
    write_raw(&path, saved.marker.clone(), &saved.raw);
    let mut child = root
        .open(&saved.marker.project_id, &saved.marker.session_id)
        .unwrap();
    let target = accept_text(&mut child, 99, "real query");
    let before = bytes(&path);
    assert!(matches!(
        child.project_request_history(&target),
        Err(StoreError::Corrupt("継承原文と固定親の不一致"))
    ));
    assert_eq!(bytes(&path), before);
}

#[test]
fn request_history_boundary_holds_parent_lock_through_final_projection() {
    let (root, parent) = parent(0);
    let parent_id = parent.snapshot().unwrap().marker.session_id;
    let mut child = parent
        .fork_memory(SessionId::new("history-child").unwrap(), "fixture")
        .unwrap();
    drop(parent);
    let target = accept_text(&mut child, 99, "real query");
    let saved = child.snapshot().unwrap();
    crate::desktop_store::memory::with_inherited_prompt_attachments(
        &child,
        &saved,
        |attachments| {
            assert!(!attachments.is_empty());
            assert!(matches!(
                root.open(&saved.marker.project_id, &parent_id),
                Err(StoreError::Busy)
            ));
            Ok(())
        },
    )
    .unwrap();
    root.open(&saved.marker.project_id, &parent_id).unwrap();
    child.project_request_history(&target).unwrap();
}

#[test]
fn request_history_boundary_accepts_legal_inherited_history_above_one_raw_record() {
    let (_root, mut parent) = parent(0);
    for turn in 3..=7 {
        let target = accept_text(&mut parent, turn, "large legal turn");
        parent
            .finish_with_messages(
                &target,
                &OperationId::new(format!("main-{turn}")).unwrap(),
                Observation::Succeeded,
                ResultId::new(format!("large-{turn}")).unwrap(),
                vec![Message::assistant("x".repeat(90_000)); 40],
            )
            .unwrap();
    }
    let mut child = parent
        .fork_memory(SessionId::new("large-history-child").unwrap(), "fixture")
        .unwrap();
    let saved = child.snapshot().unwrap();
    assert!(saved.marker.raw_offset.get() > 16 * 1024 * 1024);
    drop(parent);
    let target = accept_text(&mut child, 99, "real query");
    let before = child.snapshot().unwrap().marker;
    let history = child.project_request_history(&target).unwrap();
    assert_eq!(history.query(), "real query");
    assert_eq!(
        history
            .messages()
            .iter()
            .filter(|m| m.content.contains("untrusted server supplement"))
            .count(),
        1
    );
    assert_eq!(child.snapshot().unwrap().marker, before);
}

#[test]
fn request_history_preserves_the_direct_window_and_later_legacy_mode_after_fork() {
    for mode in [HistoryMode::Strict10, HistoryMode::Legacy] {
        let (_root, parent) = parent(10);
        let mut child = parent
            .fork_memory(SessionId::new("mode-child").unwrap(), "fixture")
            .unwrap();
        drop(parent);
        if mode == HistoryMode::Legacy {
            let saved = child.snapshot().unwrap();
            child
                .apply(
                    &request(
                        "legacy",
                        RequestBody::SessionConfigure(
                            saved.marker.session_id,
                            SessionConfigure {
                                expected_configuration_revision: saved
                                    .state
                                    .configuration
                                    .configuration_revision,
                                provider: saved.state.configuration.provider,
                                model: saved.state.configuration.model,
                                effort: saved.state.configuration.effort,
                                history_mode: mode,
                            },
                        ),
                    ),
                    None,
                )
                .unwrap();
        }
        for turn in [80, 81] {
            let target = accept_text(&mut child, turn, "local input");
            finish(&mut child, &target, turn);
        }
        let target = accept_text(&mut child, 82, "actual query");
        let history = child.project_request_history(&target).unwrap();
        assert_eq!(history.query(), "actual query");
        assert_eq!(
            history
                .messages()
                .iter()
                .filter(|m| m.role == Role::User)
                .count(),
            if mode == HistoryMode::Legacy { 13 } else { 10 }
        );
        assert_eq!(
            history
                .messages()
                .iter()
                .filter(|m| m.content.contains("untrusted server supplement"))
                .count(),
            1
        );
    }
}
