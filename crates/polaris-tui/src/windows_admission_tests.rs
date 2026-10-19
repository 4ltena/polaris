//! Windows TUI admission and slash command tests that preserve rejected work and state.
use super::*;
use polaris_core::session::Session;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::BTreeMap;
use std::fs;

fn enter() -> KeyEvent {
    KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)
}

fn contents(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(root: &Path, path: &Path, result: &mut BTreeMap<PathBuf, Option<Vec<u8>>>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            assert!(!kind.is_symlink());
            let relative = path.strip_prefix(root).unwrap().to_owned();
            if kind.is_dir() {
                result.insert(relative, None);
                visit(root, &path, result);
            } else {
                result.insert(relative, Some(fs::read(path).unwrap()));
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, &mut result);
    result
}

#[test]
fn windows_unavailable_owner_preserves_rejected_submission() {
    for text in [
        "実行してください",
        "/review",
        "/init",
        "/clear",
        "/export saved.md",
        "/resume",
        "/new",
        "/fork",
        "/model",
        "/models list",
        "/permissions",
        "/copy",
        "/logout",
        "/phase plan",
        "/workflow manual",
        "/account login",
        "/memory import source.md",
        "/mcp prompt select fixture",
        "/sync apply",
        "/session configure fixture",
        "/run settings max-turns 2",
        "/subagents cancel child attempt",
        "/setting git",
    ] {
        let mut buffer = text.to_owned();
        let mut cursor = text.chars().next().unwrap().len_utf8();
        let before = (buffer.clone(), cursor);
        assert_eq!(
            apply_admitted_key(&mut buffer, &mut cursor, enter()).err(),
            Some(WINDOWS_OWNER_UNAVAILABLE),
            "{text}"
        );
        assert_eq!((buffer, cursor), before, "{text}");
    }
    // Popup admission uses the resolved action, not the typed prefix.
    assert_eq!(
        require_action_owner(&slash::action_for("review")),
        Err(WINDOWS_OWNER_UNAVAILABLE)
    );
}

#[test]
fn windows_read_only_slash_submission_remains_available() {
    for text in [
        "/help",
        "/pwd",
        "/status",
        "/doctor",
        "/skills",
        "/diff",
        "/compare saved",
        "/account list",
        "/decided list",
        "/memory search fixture",
        "/memory get fixture",
        "/memory source fixture",
        "/mcp list",
        "/phase",
        "/workflow",
        "/session current",
        "/run details",
        "/sync status",
        "/web-search disabled",
        "/quit",
    ] {
        let mut buffer = text.to_owned();
        let mut cursor = buffer.len();
        assert!(matches!(
            apply_admitted_key(&mut buffer, &mut cursor, enter()),
            Ok(InputAction::Submit(submitted)) if submitted == text
        ));
        assert!(buffer.is_empty());
        assert_eq!(cursor, 0);
    }
}

#[test]
fn windows_unavailable_owner_preserves_inflight_history_and_queue() {
    let mut buffer = "next instruction".to_owned();
    let mut cursor = 4;
    let mut history = input::History::new();
    history.record("earlier input");
    let mut queue = VecDeque::from(["queued instruction".to_owned(), "/help".to_owned()]);
    let before = (buffer.clone(), cursor, queue.clone());
    assert_eq!(
        apply_inflight_key(&mut buffer, &mut cursor, &mut history, &mut queue, enter()),
        InflightInputAction::RejectedUnavailable
    );
    assert_eq!((&buffer, cursor, &queue), (&before.0, before.1, &before.2));
    assert_eq!(history.older(&buffer), Some("earlier input"));
    assert_eq!(history.older(&buffer), None);
    assert_eq!(
        promote_queued_input(&mut queue, false, &mut buffer, &mut cursor),
        Err(WINDOWS_OWNER_UNAVAILABLE)
    );
    assert_eq!((&buffer, cursor, &queue), (&before.0, before.1, &before.2));
    assert_eq!(
        promote_queued_input(&mut queue, true, &mut buffer, &mut cursor),
        Ok(None)
    );
    assert_eq!((buffer.clone(), cursor, queue), before);

    let mut queue = VecDeque::from(["/help".to_owned(), "/pwd".to_owned()]);
    let saved = promote_queued_input(&mut queue, false, &mut buffer, &mut cursor).unwrap();
    assert_eq!(saved, Some(("next instruction".to_owned(), 4)));
    assert_eq!(buffer, "/help");
    assert_eq!(cursor, 5);
    assert_eq!(queue, VecDeque::from(["/pwd".to_owned()]));
}

#[tokio::test]
async fn windows_unavailable_owner_blocks_turn_before_effect() {
    use std::sync::atomic::{AtomicBool, Ordering};

    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("project.rs"), b"project before").unwrap();
    fs::write(root.path().join("journal.json"), b"journal before").unwrap();
    let mut session = Session::new();
    session.push_user("existing conversation");
    let messages = serde_json::to_vec(&session.messages).unwrap();
    let before = contents(root.path());
    let mut session_path = root.path().join("session.json");
    let mut meta_path = root.path().join("meta.json");
    let paths = (session_path.clone(), meta_path.clone());
    let mut started = 17;
    let mut lines = vec![ratatui::text::Line::from("existing display")];
    let called = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&called);
    let authority: TurnAuthorityInstaller = Arc::new(move |_, _| {
        observed.store(true, Ordering::SeqCst);
        Err("authority must not run".into())
    });
    let result = admit_tui_input(
        &mut session,
        "attempted turn",
        None,
        &root.path().join("state"),
        &root.path().join("sessions"),
        &mut session_path,
        &mut meta_path,
        &mut started,
        &mut lines,
        Some(&authority),
        None,
        false,
        root.path(),
        false,
        None,
    )
    .await;
    let error = result
        .err()
        .expect("missing owner must refuse legacy admission");
    assert!(matches!(&error, TuiAdmissionFailure::Retryable(_)));
    assert_eq!(error.message(), WINDOWS_OWNER_UNAVAILABLE);
    assert!(!called.load(Ordering::SeqCst));
    assert_eq!(serde_json::to_vec(&session.messages).unwrap(), messages);
    assert!(session.persistence.is_none());
    assert_eq!((session_path, meta_path), paths);
    assert_eq!(started, 17);
    assert_eq!(lines, vec![ratatui::text::Line::from("existing display")]);
    assert_eq!(contents(root.path()), before);
}

#[test]
fn windows_unavailable_owner_blocks_direct_slash_mutations() {
    let root = tempfile::tempdir().unwrap();
    let session_path = root.path().join("session.json");
    fs::write(&session_path, b"saved conversation").unwrap();
    fs::write(root.path().join("journal.json"), b"journal before").unwrap();
    for action in [
        slash::Action::Init,
        slash::Action::Clear,
        slash::Action::Export(root.path().join("export.md").display().to_string()),
        slash::Action::Copy,
        slash::Action::Logout,
    ] {
        let mut session = Session::new();
        session.push_user("existing conversation");
        let messages = serde_json::to_vec(&session.messages).unwrap();
        let before = contents(root.path());
        let mut status = Status::Idle;
        let mut lines = vec![ratatui::text::Line::from("existing display")];
        assert!(matches!(
            apply_slash_action(
                action,
                &mut session,
                &session_path,
                &mut status,
                "fixture",
                "fixture",
                polaris_provider::UsageReport::default(),
                &[],
                root.path(),
                root.path(),
                root.path(),
                false,
                &mut lines,
            ),
            SlashOutcome::Continue
        ));
        assert!(matches!(status, Status::Error(message) if message == WINDOWS_OWNER_UNAVAILABLE));
        assert_eq!(serde_json::to_vec(&session.messages).unwrap(), messages);
        assert_eq!(lines, vec![ratatui::text::Line::from("existing display")]);
        assert_eq!(contents(root.path()), before);
    }
}

#[test]
fn windows_read_only_slash_dispatch_preserves_project_and_session() {
    let root = tempfile::tempdir().unwrap();
    fs::create_dir(root.path().join(".polaris")).unwrap();
    fs::write(root.path().join("project.rs"), b"project before").unwrap();
    let mut session = Session::new();
    session.push_user("existing conversation");
    let messages = serde_json::to_vec(&session.messages).unwrap();
    let before = contents(root.path());
    let mut lines = Vec::new();
    for action in [slash::Action::Help, slash::Action::Pwd] {
        let mut status = Status::Idle;
        assert!(matches!(
            apply_slash_action(
                action,
                &mut session,
                &root.path().join("session.json"),
                &mut status,
                "fixture",
                "fixture",
                polaris_provider::UsageReport::default(),
                &[],
                root.path(),
                root.path(),
                root.path(),
                false,
                &mut lines,
            ),
            SlashOutcome::Continue
        ));
        assert!(!matches!(status, Status::Error(_)));
    }
    assert!(!lines.is_empty());
    assert_eq!(serde_json::to_vec(&session.messages).unwrap(), messages);
    assert_eq!(contents(root.path()), before);
}
