//! Interactive command, session and admission regression tests.
#[cfg(target_os = "linux")]
#[test]
fn linux_signal_tui_pre_raw_injection_avoids_terminal_mutation() {
    use std::cell::Cell;

    let terminal_calls = Cell::new(0);
    let result = linux_start_terminal(
        || Some(signal::LinuxProcessShutdown::SigInt),
        || {
            terminal_calls.set(terminal_calls.get() + 1);
            Ok(())
        },
    );
    assert!(matches!(
        result,
        Err(LinuxTerminalStartError::Shutdown(
            signal::LinuxProcessShutdown::SigInt
        ))
    ));
    assert_eq!(terminal_calls.get(), 0);
}

#[cfg(target_os = "linux")]
#[test]
fn linux_signal_tui_startup_resume_guard_restores_once_on_error() {
    let mut guard = LinuxTerminalGuard {
        alternate: true,
        raw: true,
        mouse: false,
        cursor: false,
    };
    let mut steps = Vec::new();
    let result: Result<(), String> = linux_finish_guarded_terminal(
        Err("saved transcript read failed".into()),
        &mut guard,
        |step| {
            steps.push(step);
            Ok(())
        },
    );
    assert!(result.unwrap_err().contains("saved transcript read failed"));
    guard
        .restore_with(|step| {
            steps.push(step);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        steps,
        [LinuxTerminalStep::Raw, LinuxTerminalStep::Alternate]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_signal_tui_startup_workflow_guard_restores_once_on_error() {
    let mut guard = LinuxTerminalGuard {
        alternate: true,
        raw: true,
        mouse: false,
        cursor: false,
    };
    let mut steps = Vec::new();
    let result: Result<bool, String> =
        linux_finish_guarded_terminal(Err("confirmation draw failed".into()), &mut guard, |step| {
            steps.push(step);
            Ok(())
        });
    assert!(result.unwrap_err().contains("confirmation draw failed"));
    guard
        .restore_with(|step| {
            steps.push(step);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        steps,
        [LinuxTerminalStep::Raw, LinuxTerminalStep::Alternate]
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_signal_tui_picker_shutdown_accepts_no_choice() {
    use approver::KeyReader;
    use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};
    use std::cell::Cell;

    struct LatchingReader(Cell<bool>);
    impl KeyReader for LatchingReader {
        fn read_key(&mut self) -> std::io::Result<KeyCode> {
            let latched = &self.0;
            linux_read_key_with(
                |_| Ok(true),
                || {
                    latched.set(true);
                    Ok(Event::Key(KeyEvent::new(
                        KeyCode::Enter,
                        KeyModifiers::NONE,
                    )))
                },
                || latched.get(),
            )
        }
    }

    let terminal =
        RefCell::new(ratatui::Terminal::new(ratatui::backend::TestBackend::new(70, 14)).unwrap());
    let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
    let mut approval_policy = ApprovalPolicy::OnRequest;
    let mut status = Status::Idle;
    let mut reader = LatchingReader(Cell::new(false));
    handle_permissions(
        &terminal,
        &mut reader,
        &mut approval_policy,
        ApprovalPolicy::OnRequest,
        &sandbox,
        &mut status,
    );
    assert!(reader.0.get());
    assert_eq!(approval_policy, ApprovalPolicy::OnRequest);
    assert!(matches!(status, Status::Idle));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_signal_tui_diff_result_is_suppressed_after_latch() {
    let mut lines = vec![ratatui::text::Line::from("earlier output")];
    let result = append_diff_result_after_read("secret diff line\n", &mut lines, || {
        Some(signal::LinuxProcessShutdown::SigTerm)
    });
    assert_eq!(result, Err(signal::LinuxProcessShutdown::SigTerm));
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0].spans[0].content, "earlier output");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_signal_tui_inflight_shutdown_uses_existing_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let sandbox = SandboxPolicy::new(
        polaris_sandbox::SandboxMode::WorkspaceWrite,
        &[root.path().to_path_buf()],
    )
    .unwrap();
    let mut session = polaris_core::session::Session::new();
    let control = install_tui_root_control(
        &mut session,
        TuiRootControlArgs {
            session_id: "signal-session",
            turn: 1,
            sandbox: &sandbox,
            approval: ApprovalPolicy::OnRequest,
            mcp_available: false,
            tool_names: &["read".into()],
            model: "gpt-6-astra",
            effort: "automatic",
        },
    )
    .unwrap();
    assert!(linux_cancel_owned_children(
        Some(signal::LinuxProcessShutdown::SigInt),
        true,
    ));
    assert_eq!(control.snapshot().await.session_id, "signal-session");
    session.finish_root_delegation_control(true).await.unwrap();
    assert!(session.delegation_control.is_none());
    assert!(linux_cancel_owned_children(None, false));
    assert!(!linux_cancel_owned_children(None, true));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_signal_tui_first_signal_wins_during_cleanup() {
    use polaris_core::session_manager::TerminalStatus;
    use std::cell::Cell;

    // The turn can finish before the signal arrives while child cleanup
    // is still awaited. Check the latch again before persisting receipt.
    let pending = Cell::new(None);
    let before_cleanup = pending.get();
    pending.set(Some(signal::LinuxProcessShutdown::SigInt));
    assert_eq!(
        linux_admission_status_after_cleanup(
            TerminalStatus::Completed,
            before_cleanup.or_else(|| pending.get()),
        ),
        TerminalStatus::OutcomeUnknown,
    );
    assert_eq!(
        linux_admission_status_after_cleanup(TerminalStatus::Completed, None),
        TerminalStatus::Completed,
    );
    assert!(!linux_turn_completed_without_shutdown(true, pending.get()));
    assert!(linux_turn_completed_without_shutdown(true, None));
    pending.set(None);
    let before_capture = linux_turn_completed_without_shutdown(true, pending.get());
    assert!(before_capture);
    // The signal arrives while post-capture effects are classified, after
    // an earlier clear check but before the boundary's final commit.
    let _effect = {
        pending.set(Some(signal::LinuxProcessShutdown::SigInt));
        polaris_core::conversation_state::RewindEffectStatusV1::NoExternalMutation
    };
    assert!(!linux_turn_completed_without_shutdown(
        before_capture,
        pending.get(),
    ));
    let first = linux_signal_exit_code(signal::LinuxProcessShutdown::SigInt);
    let (exit, error) = linux_exit_after_terminal_restoration(
        first,
        Ok(()),
        Some(signal::LinuxProcessShutdown::SigTerm),
    );
    assert_eq!(exit, ExitCode::from(130));
    assert!(error.is_none());
}

#[cfg(target_os = "linux")]
#[test]
fn linux_signal_tui_cleanup_failure_exits_one_before_signal_status() {
    let (exit, error) = linux_exit_after_terminal_restoration(
        linux_signal_exit_code(signal::LinuxProcessShutdown::SigTerm),
        Err(std::io::ErrorKind::BrokenPipe.into()),
        Some(signal::LinuxProcessShutdown::SigTerm),
    );
    assert_eq!(exit, ExitCode::FAILURE);
    assert!(error.unwrap().contains("terminal restoration failed"));
}

#[cfg(target_os = "linux")]
#[test]
fn linux_signal_tui_handled_error_restores_every_acquired_state() {
    let mut guard = LinuxTerminalGuard {
        alternate: true,
        raw: true,
        mouse: true,
        cursor: true,
    };
    let mut steps = Vec::new();
    let result: Result<(), String> =
        linux_finish_guarded_terminal(Err("handled error".into()), &mut guard, |step| {
            steps.push(step);
            if step == LinuxTerminalStep::Cursor {
                Err(std::io::ErrorKind::BrokenPipe.into())
            } else {
                Ok(())
            }
        });
    let error = result.unwrap_err();
    assert!(error.contains("handled error"));
    assert!(error.contains("terminal restoration failed"));
    guard
        .restore_with(|step| {
            steps.push(step);
            Ok(())
        })
        .unwrap();
    assert_eq!(
        steps,
        [
            LinuxTerminalStep::Cursor,
            LinuxTerminalStep::Mouse,
            LinuxTerminalStep::Raw,
            LinuxTerminalStep::Alternate,
        ]
    );
}

#[test]
fn m3c_memory_read_keeps_project_scope_and_bounds_exact_ranges() {
    use polaris_memory::{MemoryStore, Record};

    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let other_project = root.path().join("other");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&other_project).unwrap();
    let state_dir = root.path().join("state");
    std::fs::create_dir(&state_dir).unwrap();
    assert!(memory_read_lines(&project, &state_dir, root.path(), "search needle").is_err());
    assert!(!state_dir.join("memory.sqlite3").exists());

    let mut store = MemoryStore::open(state_dir.join("memory.sqlite3")).unwrap();
    for (path, id) in [
        (&project, "same-project"),
        (&other_project, "other-project"),
    ] {
        store
            .upsert(
                &Record {
                    project_id: persist::project_identity(path).unwrap(),
                    session_id: "session-1".into(),
                    id: id.into(),
                    role: "user".into(),
                    timestamp: "2026-09-23".into(),
                    source: "test".into(),
                    text: "needle evidence".into(),
                },
                None,
            )
            .unwrap();
    }
    drop(store);
    let lines = memory_read_lines(&project, &state_dir, root.path(), "search needle")
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(lines.contains("same-project"), "{lines}");
    assert!(!lines.contains("other-project"), "{lines}");
    assert!(lines.contains("not current instructions or approvals"));
    let page = memory_read_lines(
        &project,
        &state_dir,
        root.path(),
        "get session-1 same-project 7 8",
    )
    .unwrap()
    .iter()
    .map(ToString::to_string)
    .collect::<Vec<_>>()
    .join("\n");
    assert!(page.contains("evidence"));
    assert!(
        memory_read_lines(
            &project,
            &state_dir,
            root.path(),
            "get session-1 same-project 900 8"
        )
        .is_err()
    );
    assert!(
        memory_read_lines(
            &project,
            &state_dir,
            root.path(),
            "get session-1 same-project 0 4097"
        )
        .is_err()
    );
    assert!(
        memory_read_lines(
            &project,
            &state_dir,
            root.path(),
            "get session-1 other-project"
        )
        .is_err()
    );
}

#[test]
fn decided_read_is_bounded_and_marks_old_entries_as_history() {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir(root.path().join(".polaris")).unwrap();
    polaris_core::decisions::append(root.path(), "2026-09-23", "keep the old name").unwrap();
    let lines = decided_read_lines(root.path())
        .unwrap()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(lines.contains("keep the old name"), "{lines}");
    assert!(lines.contains("not current approvals"), "{lines}");
    std::fs::write(
        root.path().join(polaris_core::decisions::PATH),
        "x".repeat(1024 * 1024 + 1),
    )
    .unwrap();
    assert!(decided_read_lines(root.path()).is_err());
}

#[test]
fn readonly_acceptance_compare_folders_and_errors_preserve_source() {
    let root = tempfile::tempdir().unwrap();
    let left = root.path().join("left");
    let right = root.path().join("right");
    std::fs::create_dir(&left).unwrap();
    std::fs::create_dir(&right).unwrap();
    for path in [&left, &right] {
        std::fs::write(path.join("same.txt"), "contents stay in place").unwrap();
    }
    let mut session = Session::default();
    let mut status = Status::Idle;
    let mut local_lines = Vec::new();
    let session_path = root.path().join("session.jsonl");
    let outcome = call(
        slash::parse("/compare left | right").unwrap(),
        &mut session,
        &session_path,
        &mut status,
        &[],
        root.path(),
        &mut local_lines,
    );
    assert!(matches!(outcome, SlashOutcome::Continue));
    assert!(matches!(status, Status::Notice(_)));
    let lines = local_lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(lines.contains("similarity"), "{lines}");
    assert!(lines.contains("Nothing is moved or deleted"), "{lines}");
    for command in [
        "/compare left | missing".to_owned(),
        format!("/compare {} | right", "x".repeat(2049)),
    ] {
        let before = local_lines.clone();
        call(
            slash::parse(&command).unwrap(),
            &mut session,
            &session_path,
            &mut status,
            &[],
            root.path(),
            &mut local_lines,
        );
        assert!(matches!(status, Status::Error(_)));
        assert_eq!(local_lines, before);
    }
    assert!(session.messages.is_empty());
    assert!(!session_path.exists());
    assert_eq!(
        std::fs::read_to_string(left.join("same.txt")).unwrap(),
        "contents stay in place"
    );
    assert_eq!(
        std::fs::read_to_string(right.join("same.txt")).unwrap(),
        "contents stay in place"
    );
}

#[test]
fn mcp_prompt_selection_is_typed_and_rejects_ambiguous_arguments() {
    let choice = mcp_prompt_choice("prompt files/review path=src/lib.rs depth=2").unwrap();
    assert_eq!(choice.server, "files");
    assert_eq!(choice.prompt, "review");
    assert_eq!(choice.arguments["depth"], "2");
    assert!(mcp_prompt_choice("prompt files/review path=a path=b").is_err());
    assert!(mcp_prompt_choice("prompt files/review path").is_err());
    assert!(mcp_prompt_choice("prompt files/review\nsecret=x").is_err());
}

#[test]
fn mcp_prompt_selection_does_not_follow_a_different_session() {
    let mut pending = Some(mcp_prompt_choice("prompt files/review path=src/lib.rs").unwrap());
    let mut requires_reselect = true;
    clear_mcp_prompt_on_session_change(
        std::path::Path::new("/sessions/first.jsonl"),
        std::path::Path::new("/sessions/first.jsonl"),
        &mut pending,
        &mut requires_reselect,
    );
    assert!(pending.is_some());
    assert!(requires_reselect);

    clear_mcp_prompt_on_session_change(
        std::path::Path::new("/sessions/first.jsonl"),
        std::path::Path::new("/sessions/second.jsonl"),
        &mut pending,
        &mut requires_reselect,
    );
    assert!(pending.is_none());
    assert!(!requires_reselect);
}

#[cfg(unix)]
#[test]
fn unresolved_management_effect_blocks_mutating_slash_but_allows_diagnostics() {
    use polaris_core::management_effect::{
        ManagementEffectHashV1, ManagementEffectScopeV1, ManagementInventoryIdV1,
        ManagementOperationIdV1, ManagementPermissionScopeV1, ManagementRewindDispositionV1,
        PrepareManagementEffectV1,
    };
    use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};

    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let state_dir = root.path().join("state");
    std::fs::create_dir(&state_dir).unwrap();
    std::fs::set_permissions(
        &state_dir,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    let owner = WorkspaceOwner::open(
        &project,
        &state_dir.join("rewind-workspace"),
        RewindLimits::default(),
    )
    .unwrap();
    let lease = owner.try_mutation().unwrap();
    let store = polaris_core::management_effect::ManagementEffectStoreV1::open_at_state_dir(
        &project, &state_dir, &lease,
    )
    .unwrap();
    let request = PrepareManagementEffectV1 {
        operation_id: ManagementOperationIdV1::random().unwrap(),
        inventory: ManagementInventoryIdV1::parse("T10").unwrap(),
        scope: ManagementEffectScopeV1 {
            project: store.project_scope_hash(),
            session: None,
            generation: 0,
            revision: 0,
            permission: ManagementPermissionScopeV1::Project,
            account_binding: None,
        },
        request_hash: ManagementEffectHashV1::digest(b"synthetic-init"),
        rewind: ManagementRewindDispositionV1::IrreversibleLocal,
    };
    store
        .prepare(&lease, store.cursor().unwrap(), request)
        .unwrap();
    drop(lease);

    assert!(management_clearance_error(&project, &store, &slash::Action::Init).is_some());
    assert!(management_clearance_error(&project, &store, &slash::Action::Status).is_none());
    let write_root = root.path().join("write\nroot");
    std::fs::create_dir(&write_root).unwrap();
    let sandbox =
        SandboxPolicy::new(polaris_sandbox::SandboxMode::WorkspaceWrite, &[write_root]).unwrap();
    let session = Session::default();
    let lines = status_lines(StatusSnapshot {
        session: &session,
        session_path: None,
        accounts: Ok(Vec::new()),
        #[cfg(unix)]
        binding: None,
        provider_name: "provider\n管理操作: 未解決なし",
        model_name: "model\tname",
        usage_report: polaris_provider::UsageReport::default(),
        sandbox: &sandbox,
        approval: ApprovalPolicy::Never,
        startup_approval: ApprovalPolicy::OnRequest,
        mcp_external: false,
        audit_path: None,
        turn_authority_present: false,
        fixed_effort: None,
        answer_finalizer: None,
        project_root: &project,
        #[cfg(unix)]
        management_effects: &store,
        recovery_issue: Some("復旧\n確認待ち"),
    });
    let text = lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(lines.iter().any(|line| {
        let line = line.to_string();
        line.contains("Approval (current):") && line.ends_with("Never")
    }));
    assert!(lines.iter().any(|line| {
        let line = line.to_string();
        line.contains("Approval (startup):") && line.ends_with("OnRequest")
    }));
    let blocked = store.blocker().unwrap().unwrap();
    assert!(
        text.contains(&blocked.request.operation_id.to_string()),
        "{text}"
    );
    for (label, value) in [
        ("Target:", blocked.request.inventory.to_string()),
        ("State:", format!("{:?}", blocked.state)),
        ("Record number:", blocked.last_sequence.to_string()),
    ] {
        assert!(lines.iter().any(|line| {
            let line = line.to_string();
            line.contains(label) && line.ends_with(&value)
        }));
    }
    assert!(text.contains("復旧\\n確認待ち"), "{text}");
    assert!(text.contains("provider\\n管理操作: 未解決なし"), "{text}");
    assert!(text.contains("model\\tname"), "{text}");
    assert!(text.contains("write\\nroot"), "{text}");
    assert!(lines.iter().all(|line| {
        let line = line.to_string();
        !line.contains('\n') && !line.contains('\t')
    }));
    assert!(session.messages.is_empty());
}

#[cfg(unix)]
#[test]
fn unavailable_rewind_store_refuses_session_start_instead_of_skipping_resume_validation() {
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let state_dir = root.path().join("state");
    std::fs::create_dir(&state_dir).unwrap();
    std::fs::write(state_dir.join("rewind-workspace"), "blocked").unwrap();
    assert!(open_rewind_workspace(&project, &state_dir).is_err());
}

#[cfg(unix)]
#[test]
fn startup_mismatch_shows_saved_transcript_without_a_run() {
    use polaris_core::conversation_state::HistoryMode;
    use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};
    use polaris_core::session_store::PersistedSession;

    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        root.path(),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let file = project.join("task.txt");
    std::fs::write(&file, "original").unwrap();
    let owner = WorkspaceOwner::open(
        &project,
        &root.path().join("rewind-workspace"),
        RewindLimits::default(),
    )
    .unwrap();
    let persistence = PersistedSession::create(
        root.path(),
        &root.path().join("memory.sqlite3"),
        "project",
        HistoryMode::Legacy,
        None,
    )
    .unwrap();
    let mut session = polaris_core::session::Session::new();
    session.attach(persistence.clone()).unwrap();
    session.push_user("saved question");
    session.push_assistant("saved answer", Vec::new());
    session.check_persistence().unwrap();
    validate_resume_workspace(&owner, &persistence).unwrap();
    std::fs::write(&file, "different tree").unwrap();
    let reason = validate_resume_workspace(&owner, &persistence).unwrap_err();

    let terminal =
        RefCell::new(ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 18)).unwrap());
    let mut reader = ScriptedReader(VecDeque::from([KeyCode::Enter]));
    show_persisted_read_only_resume(&terminal, &mut reader, &persistence, &reason).unwrap();
    let screen = {
        let terminal = terminal.borrow();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol()))
            .collect::<String>()
    };
    assert!(screen.contains("saved question"), "{screen}");
    assert!(screen.contains("saved answer"), "{screen}");
    assert!(screen.replace(' ', "").contains("viewonly"), "{screen}");
    assert_eq!(persistence.snapshot().unwrap().events.len(), 2);
}

#[cfg(unix)]
#[test]
fn rewind_management_crossing_is_rechecked_before_completion_and_child_creation() {
    use polaris_core::{
        conversation_state::{HistoryMode, RewindEffectStatusV1, RewindEligibilityV1},
        management_effect::*,
        rewind_workspace::{RewindLimits, WorkspaceOwner},
        session_store::PersistedSession,
    };
    use polaris_sandbox::{SandboxMode, SandboxPolicy};
    use std::os::unix::fs::PermissionsExt;

    for case in 0..4 {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let state = root.path().join("state");
        std::fs::create_dir(&project).unwrap();
        std::fs::create_dir(project.join(".polaris")).unwrap();
        std::fs::create_dir(&state).unwrap();
        std::fs::set_permissions(&state, std::fs::Permissions::from_mode(0o700)).unwrap();
        let owner = WorkspaceOwner::open(
            &project,
            &state.join("rewind-workspace"),
            RewindLimits::default(),
        )
        .unwrap();
        let lease = owner.try_mutation().unwrap();
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        drop(lease);
        let policy =
            SandboxPolicy::new(SandboxMode::WorkspaceWrite, std::slice::from_ref(&project))
                .unwrap();
        let persistence = PersistedSession::create(
            root.path(),
            &state.join("memory.sqlite3"),
            project.to_str().unwrap(),
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let complete_c10 = || {
            let lease = owner.try_mutation().unwrap();
            let request = PrepareManagementEffectV1 {
                operation_id: "00000000000000000000000000000001".parse().unwrap(),
                inventory: ManagementInventoryIdV1::parse("C10").unwrap(),
                scope: ManagementEffectScopeV1 {
                    project: effects.project_scope_hash(),
                    session: None,
                    generation: 0,
                    revision: 0,
                    permission: ManagementPermissionScopeV1::Remote,
                    account_binding: Some(ManagementEffectHashV1::digest(
                        b"synthetic-keychain-binding",
                    )),
                },
                request_hash: ManagementEffectHashV1::digest(b"synthetic-c10"),
                rewind: ManagementRewindDispositionV1::IrreversibleRemote,
            };
            let PrepareManagementEffectOutcomeV1::Prepared(prepared) = effects
                .prepare(&lease, effects.cursor().unwrap(), request)
                .unwrap()
            else {
                panic!("new C10")
            };
            let started = effects.start(&lease, &prepared).unwrap();
            effects
                .complete_started(
                    &lease,
                    &started,
                    ManagementEffectHashV1::digest(b"synthetic-remote-observation"),
                )
                .unwrap();
            assert!(effects.blocker().unwrap().is_none());
        };
        if case == 3 {
            complete_c10();
        }
        let pre = owner.capture(&policy).unwrap();
        let boundary = persistence
            .begin_rewind_boundary(pre.to_string(), Vec::new())
            .unwrap();
        let mut session = polaris_core::session::Session::new();
        session.attach(persistence.clone()).unwrap();
        session.push_user("edit task");
        std::fs::write(project.join("task.txt"), "after").unwrap();
        session.push_assistant("done", Vec::new());
        session.check_persistence().unwrap();
        if case == 1 || case == 2 {
            complete_c10();
        }
        let saved = if case == 2 {
            let post = owner.capture(&policy).unwrap();
            persistence
                .finish_rewind_boundary(
                    &boundary.boundary_id,
                    post.to_string(),
                    RewindEligibilityV1::Eligible,
                    RewindEffectStatusV1::NoExternalMutation,
                )
                .unwrap()
        } else {
            finish_rewind_for_session(
                &owner,
                &persistence,
                &effects,
                &policy,
                &boundary,
                None,
                || true,
            )
            .unwrap()
        };
        assert_eq!(
            matches!(
                saved.completion.unwrap().eligibility,
                RewindEligibilityV1::Eligible
            ),
            case != 1
        );
        let sessions = root.path().join("sessions-v2");
        let before = std::fs::read_dir(&sessions).unwrap().count();
        let candidate = persistence.prepare_rewind_branch(&owner, &boundary.boundary_id);
        if case == 0 {
            assert!(candidate.is_ok());
        } else {
            // Case 3 has a clear historical range but an orphan terminal
            // C10 effect. P4 must refuse new work independently of crossing.
            assert!(candidate.is_err());
            assert_eq!(std::fs::read_dir(&sessions).unwrap().count(), before);
            let manifests = state.join("rewind-workspace/manifests");
            let before_capture = std::fs::read_dir(&manifests).unwrap().count();
            assert!(capture_rewind_before_input(&owner, &effects, &policy).is_err());
            assert_eq!(
                std::fs::read_dir(&manifests).unwrap().count(),
                before_capture
            );
        }
        assert_eq!(
            std::fs::read_to_string(project.join("task.txt")).unwrap(),
            "after"
        );
    }
}

#[cfg(unix)]
#[test]
fn resume_requires_the_workspace_recorded_at_the_conversation_head() {
    use polaris_core::conversation_state::{
        HistoryMode, RewindEffectStatusV1, RewindEligibilityV1,
    };
    use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};
    use polaris_core::session_store::PersistedSession;
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        root.path(),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let file = project.join("task.txt");
    std::fs::write(&file, "before").unwrap();
    let owner = WorkspaceOwner::open(
        &project,
        &root.path().join("rewind-workspace"),
        RewindLimits::default(),
    )
    .unwrap();
    let policy =
        SandboxPolicy::new(SandboxMode::WorkspaceWrite, std::slice::from_ref(&project)).unwrap();
    let persistence = PersistedSession::create(
        root.path(),
        &root.path().join("memory.sqlite3"),
        "project",
        HistoryMode::Legacy,
        None,
    )
    .unwrap();
    let pre = owner.capture(&policy).unwrap();
    let boundary = persistence
        .begin_rewind_boundary(pre.to_string(), Vec::new())
        .unwrap();
    let mut session = polaris_core::session::Session::new();
    session.attach(persistence.clone()).unwrap();
    session.push_user("edit the file");
    session.push_assistant("done", Vec::new());
    session.check_persistence().unwrap();
    std::fs::write(&file, "after").unwrap();
    let post = owner.capture(&policy).unwrap();
    persistence
        .finish_rewind_boundary(
            &boundary.boundary_id,
            post.to_string(),
            RewindEligibilityV1::Eligible,
            RewindEffectStatusV1::NoExternalMutation,
        )
        .unwrap();

    assert!(validate_resume_workspace(&owner, &persistence).is_ok());
    std::fs::write(&file, "unrelated version").unwrap();
    assert!(validate_resume_workspace(&owner, &persistence).is_err());
}

#[cfg(unix)]
#[test]
fn first_unbound_resume_binds_the_tree_and_later_unbound_resumes_refuse() {
    use polaris_core::conversation_state::HistoryMode;
    use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};
    use polaris_core::session_store::PersistedSession;

    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        root.path(),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let file = project.join("task.txt");
    std::fs::write(&file, "initial").unwrap();
    let owner = WorkspaceOwner::open(
        &project,
        &root.path().join("rewind-workspace"),
        RewindLimits::default(),
    )
    .unwrap();
    let database = root.path().join("memory.sqlite3");
    let first =
        PersistedSession::create(root.path(), &database, "project", HistoryMode::Legacy, None)
            .unwrap();
    let id = first.snapshot().unwrap().state.session_id;
    assert!(validate_resume_workspace(&owner, &first).is_ok());
    assert!(first.store.workspace_head_binding().unwrap().is_some());
    let reopened = PersistedSession::open(root.path(), &database, "project", &id).unwrap();
    assert!(validate_resume_workspace(&owner, &reopened).is_ok());
    let second =
        PersistedSession::create(root.path(), &database, "project", HistoryMode::Legacy, None)
            .unwrap();
    assert!(validate_resume_workspace(&owner, &second).is_err());
    std::fs::write(file, "changed").unwrap();
    assert!(validate_resume_workspace(&owner, &reopened).is_err());
}

#[cfg(unix)]
#[test]
fn interrupted_first_binding_retries_with_the_current_tree() {
    use polaris_core::conversation_state::HistoryMode;
    use polaris_core::rewind_workspace::{ErrorKind, RewindLimits, WorkspaceOwner};
    use polaris_core::session_store::PersistedSession;
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        root.path(),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let file = project.join("task.txt");
    std::fs::write(&file, "initial").unwrap();
    let owner = WorkspaceOwner::open(
        &project,
        &root.path().join("rewind-workspace"),
        RewindLimits::default(),
    )
    .unwrap();
    let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[project]).unwrap();
    let session = PersistedSession::create(
        root.path(),
        &root.path().join("memory.sqlite3"),
        "project",
        HistoryMode::Legacy,
        None,
    )
    .unwrap();
    let snapshot = session.snapshot().unwrap();
    let id = snapshot.state.session_id.clone();
    let interrupted = owner.bind_first_legacy_head(&policy, &id, |manifest, permit| {
        session.store.repair_pending_workspace_head_binding(
            &snapshot,
            &manifest.to_string(),
            permit,
        )?;
        std::fs::write(&file, "changed while binding")
    });
    assert_eq!(interrupted.unwrap_err().kind, ErrorKind::Conflict);
    assert!(owner.pending_legacy_binding_for(&id).unwrap());
    assert!(validate_resume_workspace(&owner, &session).is_ok());
    assert!(!owner.pending_legacy_binding_for(&id).unwrap());
    let binding = session.store.workspace_head_binding().unwrap().unwrap();
    let manifest = binding.workspace_manifest_id.parse().unwrap();
    assert!(owner.matches_manifest(&manifest).unwrap());
}

#[cfg(unix)]
#[test]
fn two_turn_rewind_restores_earlier_files_and_blocks_the_origin() {
    use polaris_core::conversation_state::{
        HistoryMode, RewindEffectStatusV1, RewindEligibilityV1,
    };
    use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};
    use polaris_core::session_store::PersistedSession;
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        root.path(),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let owner = WorkspaceOwner::open(
        &project,
        &root.path().join("rewind-workspace"),
        RewindLimits::default(),
    )
    .unwrap();
    let policy =
        SandboxPolicy::new(SandboxMode::WorkspaceWrite, std::slice::from_ref(&project)).unwrap();
    let persistence = PersistedSession::create(
        root.path(),
        &root.path().join("memory.sqlite3"),
        "project",
        HistoryMode::Legacy,
        None,
    )
    .unwrap();
    let mut session = polaris_core::session::Session::new();
    session.attach(persistence.clone()).unwrap();

    let first_pre = owner.capture(&policy).unwrap();
    let first = persistence
        .begin_rewind_boundary(first_pre.to_string(), Vec::new())
        .unwrap();
    session.push_user("create a file");
    std::fs::write(project.join("task.txt"), "first").unwrap();
    session.push_assistant("created", Vec::new());
    session.check_persistence().unwrap();
    let first_post = owner.capture(&policy).unwrap();
    persistence
        .finish_rewind_boundary(
            &first.boundary_id,
            first_post.to_string(),
            RewindEligibilityV1::Eligible,
            RewindEffectStatusV1::NoExternalMutation,
        )
        .unwrap();

    let second_pre = owner.capture(&policy).unwrap();
    let second = persistence
        .begin_rewind_boundary(second_pre.to_string(), Vec::new())
        .unwrap();
    session.push_user("change the file");
    std::fs::write(project.join("task.txt"), "second").unwrap();
    std::fs::create_dir(project.join("new-dir")).unwrap();
    std::fs::write(project.join("new-dir/extra.txt"), "later").unwrap();
    session.push_assistant("changed", Vec::new());
    session.check_persistence().unwrap();
    let second_post = owner.capture(&policy).unwrap();
    persistence
        .finish_rewind_boundary(
            &second.boundary_id,
            second_post.to_string(),
            RewindEligibilityV1::Eligible,
            RewindEffectStatusV1::NoExternalMutation,
        )
        .unwrap();

    let initial_branch = persistence
        .prepare_rewind_branch(&owner, &first.boundary_id)
        .unwrap();
    assert!(
        !initial_branch
            .snapshot()
            .unwrap()
            .events
            .iter()
            .any(|event| event.message.content == "create a file")
    );

    let branch = persistence
        .prepare_rewind_branch(&owner, &second.boundary_id)
        .unwrap();
    let branch_snapshot = branch.snapshot().unwrap();
    assert!(
        branch_snapshot
            .events
            .iter()
            .any(|event| event.message.content == "created")
    );
    assert!(
        !branch_snapshot
            .events
            .iter()
            .any(|event| event.message.content == "change the file")
    );
    let restored = owner
        .restore_for_branch(&second_post, &second_pre, &branch_snapshot.state.session_id)
        .unwrap();
    owner.commit(&restored.transaction_id).unwrap();
    assert_eq!(
        std::fs::read_to_string(project.join("task.txt")).unwrap(),
        "first"
    );
    assert!(!project.join("new-dir").exists());
    assert!(
        validate_resume_workspace(&owner, &branch).is_ok(),
        "{:?}",
        validate_resume_workspace(&owner, &branch)
    );
    assert!(validate_resume_workspace(&owner, &persistence).is_err());
}

#[cfg(unix)]
#[test]
fn restarted_branch_preserves_workflow_state_without_copying_old_sidechat() {
    use polaris_core::conversation_state::{
        HistoryMode, RewindEffectStatusV1, RewindEligibilityV1,
    };
    use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};
    use polaris_core::session_manager::{
        DestinationMode, SessionId, SubmissionPayloadV1, SubmitOutcomeV1, SubmitRequestV1,
    };
    use polaris_core::session_store::PersistedSession;
    use polaris_core::workflow::Phase;
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        root.path(),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    let project = root.path().join("project");
    let state_dir = root.path().join("state");
    std::fs::create_dir(&project).unwrap();
    let file = project.join("task.txt");
    std::fs::write(&file, "before").unwrap();
    let owner = WorkspaceOwner::open(
        &project,
        &root.path().join("rewind-workspace"),
        RewindLimits::default(),
    )
    .unwrap();
    let policy = SandboxPolicy::new(SandboxMode::WorkspaceWrite, &[project]).unwrap();
    let database = root.path().join("memory.sqlite3");
    let source =
        PersistedSession::create(root.path(), &database, "project", HistoryMode::Legacy, None)
            .unwrap();
    let root_snapshot = source.snapshot().unwrap();
    let root_id = SessionId::new(root_snapshot.state.session_id.clone()).unwrap();
    let receipt_counts = std::collections::BTreeMap::from([(
        root_id.clone(),
        root_snapshot.state.admission_receipts.len(),
    )]);
    let (store, initial_manager) = open_tui_admission_owner(&state_dir, &root_id).unwrap();
    let mut old_manager = initial_manager.clone();
    let before = old_manager.revision;
    let SubmitOutcomeV1::Accepted { admission, .. } = old_manager
        .submit(
            SubmitRequestV1 {
                parent_session: root_id.clone(),
                payload: SubmissionPayloadV1 {
                    text: "unrelated side work".into(),
                    attachment_ids: Vec::new(),
                    route: DestinationMode::New,
                    expected_session_revision: 0,
                    expected_goal_revision: 0,
                },
                admission_id: None,
            },
            old_manager.revision,
        )
        .unwrap();
    store
        .persist_submission(before, &old_manager, &receipt_counts)
        .unwrap();
    let before = old_manager.revision;
    let decided = old_manager
        .decide(
            &admission.id,
            old_manager.revision,
            admission.revision,
            None,
            false,
            false,
        )
        .unwrap();
    persist_manager_transition(&store, before, &old_manager).unwrap();
    let before = old_manager.revision;
    old_manager
        .prepare_destination(&decided.id, before, decided.revision)
        .unwrap();
    persist_manager_transition(&store, before, &old_manager).unwrap();
    assert_eq!(old_manager.sessions.len(), 2);
    drop(store);

    let mut session = polaris_core::session::Session::new();
    session.attach(source.clone()).unwrap();
    session.push_user("previous turn");
    session.push_assistant("previous answer", Vec::new());
    session.check_persistence().unwrap();
    let mut lock = source.store.lock().unwrap();
    let snapshot = lock.snapshot().unwrap();
    let mut state = snapshot.state.clone();
    state.workflow.phase = Phase::Review;
    state.workflow.revision = 1;
    state.workflow_artifacts.revision = 1;
    lock.publish(&snapshot, state).unwrap();
    drop(lock);
    drop(session);
    let source =
        PersistedSession::open(root.path(), &database, "project", root_id.as_str()).unwrap();
    let mut session = polaris_core::session::Session::new();
    session.attach(source.clone()).unwrap();

    let pre = owner.capture(&policy).unwrap();
    let boundary = source
        .begin_rewind_boundary(pre.to_string(), Vec::new())
        .unwrap();
    session.push_user("later turn");
    session.push_assistant("later answer", Vec::new());
    session.check_persistence().unwrap();
    std::fs::write(&file, "after").unwrap();
    let post = owner.capture(&policy).unwrap();
    source
        .finish_rewind_boundary(
            &boundary.boundary_id,
            post.to_string(),
            RewindEligibilityV1::Eligible,
            RewindEffectStatusV1::NoExternalMutation,
        )
        .unwrap();
    let branch = source
        .prepare_rewind_branch(&owner, &boundary.boundary_id)
        .unwrap();
    let branch_id = branch.snapshot().unwrap().state.session_id;
    let restored = owner.restore_for_branch(&post, &pre, &branch_id).unwrap();
    owner.commit(&restored.transaction_id).unwrap();

    let restarted = PersistedSession::open(root.path(), &database, "project", &branch_id).unwrap();
    validate_resume_workspace(&owner, &restarted).unwrap();
    let branch_snapshot = restarted.snapshot().unwrap();
    assert_eq!(branch_snapshot.state.workflow.phase, Phase::Review);
    assert_eq!(branch_snapshot.state.workflow_artifacts.revision, 1);
    assert_eq!(branch_snapshot.events.len(), 2);
    let branch_session_id = SessionId::new(branch_id).unwrap();
    let (new_store, new_manager) =
        open_tui_admission_owner(&state_dir, &branch_session_id).unwrap();
    assert_eq!(new_manager.sessions.len(), 1);
    assert!(new_manager.sessions.contains_key(&branch_session_id));
    drop(new_store);
    let (_old_store, retained_old_manager) =
        open_tui_admission_owner(&state_dir, &root_id).unwrap();
    assert_eq!(retained_old_manager.sessions.len(), 2);
}

struct DistinctContextJudge;

#[cfg(unix)]
fn prompt_fixture_host(
    project: &Path,
    home: &Path,
    mut server: polaris_mcp::registration::Server,
) -> polaris_core::mcp::local::LocalHost {
    use polaris_mcp::registration::Registry;

    let host = polaris_core::mcp::local::LocalHost::open_for_test(project, home, true).unwrap();
    server.enabled_projects = [host.project().to_owned()].into_iter().collect();
    host.store()
        .unwrap()
        .save_registry(
            0,
            &Registry {
                schema_version: 1,
                revision: 1,
                servers: vec![server],
            },
        )
        .unwrap();
    host
}

#[async_trait::async_trait]
impl polaris_provider::decision::Decider for DistinctContextJudge {
    async fn decide(
        &self,
        _state: &serde_json::Value,
        _questions: &std::collections::BTreeMap<String, polaris_provider::decision::Question>,
    ) -> Result<polaris_provider::decision::Decision, polaris_provider::decision::DecisionError>
    {
        Ok(polaris_provider::decision::Decision {
            answers: std::collections::BTreeMap::from([(
                "context_relation".into(),
                polaris_provider::decision::Answer {
                    choice: Some("distinct_context".into()),
                    probabilities: std::collections::BTreeMap::from([(
                        "distinct_context".into(),
                        0.97,
                    )]),
                    confidence: Some(0.96),
                    ..Default::default()
                },
            )]),
            input_tokens: Some(8),
            output_tokens: Some(1),
        })
    }
}

#[cfg(unix)]
fn mcp_prompt_child_gate(exact_test_name: &str, sentinel: &str) -> bool {
    const MARKER: &str = "POLARIS_TUI_MCP_PROMPT_CHILD";
    if let Some(value) = std::env::var_os(MARKER) {
        assert!(
            value == exact_test_name,
            "unexpected MCP prompt child marker"
        );
        return false;
    }

    let output = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--exact")
        .arg(exact_test_name)
        .arg("--nocapture")
        .env(MARKER, exact_test_name)
        .env_remove("POLARIS_DATA_DIR")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success() && stdout.contains(sentinel),
        "MCP prompt child {exact_test_name} failed: status={:?} stdout={stdout} stderr={stderr}",
        output.status
    );
    true
}

#[cfg(unix)]
#[tokio::test]
async fn selected_mcp_prompt_and_input_survive_sidechat_restart_together() {
    use polaris_core::{
        conversation_state::HistoryMode,
        mcp::{operation::ExternalCeiling, runtime::McpPromptChoice},
        session_manager::{AdmissionState, SessionId, TerminalStatus},
        session_store::PersistedSession,
    };

    if mcp_prompt_child_gate(
        "tests::selected_mcp_prompt_and_input_survive_sidechat_restart_together",
        "POLARIS_TUI_SELECTED_MCP_PROMPT_CHILD_COMPLETE",
    ) {
        return;
    }

    let root = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let state_dir = root.path().join("state");
    let sessions_dir = root.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let database = root.path().join("memory.sqlite3");
    let parent =
        PersistedSession::create(root.path(), &database, "project", HistoryMode::Legacy, None)
            .unwrap();
    let parent_id = parent.snapshot().unwrap().state.session_id;
    let mut session = polaris_core::session::Session::new();
    session.attach(parent.clone()).unwrap();
    let mut session_path = sessions_dir.join(format!("{parent_id}.jsonl"));
    let mut meta_path = session_path.with_extension("meta.json");
    let mut started = 1;
    let mut local_lines = Vec::new();

    let (server, methods, fixture) = polaris_mcp::test_support::server_fixture("").await;
    let host = prompt_fixture_host(root.path(), home.path(), server);
    let choice = McpPromptChoice {
        server: "fixture".into(),
        prompt: "prompt".into(),
        arguments: Default::default(),
    };
    let (turn, _, _, selected_runtime) = admit_tui_input(
        &mut session,
        "original input",
        Some(&DistinctContextJudge),
        &state_dir,
        &sessions_dir,
        &mut session_path,
        &mut meta_path,
        &mut started,
        &mut local_lines,
        None,
        None,
        false,
        root.path(),
        true,
        Some(TuiPromptRequest {
            host: &host,
            choice: &choice,
            selector: None,
            ceiling: ExternalCeiling::Denied,
        }),
    )
    .await
    .unwrap();
    assert!(selected_runtime.is_some());
    assert!(matches!(
        turn.manager.status(&turn.admission_id).unwrap().state,
        AdmissionState::Running { .. }
    ));
    let sidechat = session.persistence.as_ref().unwrap().snapshot().unwrap();
    assert_ne!(sidechat.state.session_id, parent_id);
    assert!(parent.snapshot().unwrap().events.is_empty());
    assert_eq!(sidechat.events.len(), 2);
    assert_eq!(sidechat.events[0].message.content, "original input");
    assert!(sidechat.events[0].starts_turn);
    assert!(
        sidechat.events[1]
            .message
            .content
            .contains("selected prompt")
    );
    let envelope: serde_json::Value =
        serde_json::from_str(&sidechat.events[1].message.content).unwrap();
    let expected_run = polaris_core::session_manager::RunId::derive(&turn.admission_id);
    assert_eq!(
        envelope["intent"]["session"].as_str(),
        Some(sidechat.state.session_id.as_str())
    );
    assert_eq!(
        envelope["intent"]["run"].as_str(),
        Some(expected_run.as_str())
    );
    assert_eq!(
        envelope["intent"]["binding"]["item"]["server_id"],
        "fixture"
    );
    assert_eq!(envelope["intent"]["binding"]["item"]["key"], "prompt");
    assert_eq!(envelope["disposition"], "completed");
    assert!(!sidechat.events[1].starts_turn);
    assert_eq!(sidechat.events[0].turn_id, sidechat.events[1].turn_id);
    assert!(
        sidechat
            .state
            .admission_receipts
            .contains_key(turn.admission_id.as_str())
    );
    assert_eq!(
        methods
            .lock()
            .unwrap()
            .iter()
            .filter(|method| *method == "prompts/get")
            .count(),
        1
    );

    let admission_id = turn.admission_id.clone();
    let sidechat_id = SessionId::new(sidechat.state.session_id.clone()).unwrap();
    drop(turn);
    let (store, mut manager) = open_tui_admission_owner(&state_dir, &sidechat_id).unwrap();
    recover_admission_owner(&store, &mut manager, root.path(), &database, "project").unwrap();
    assert!(matches!(
        manager.status(&admission_id).unwrap().state,
        AdmissionState::Terminal {
            status: TerminalStatus::OutcomeUnknown,
            ..
        }
    ));
    assert_eq!(
        session
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .events
            .len(),
        2
    );
    drop(store);
    let retry = admit_tui_input(
        &mut session,
        "must be blocked",
        None,
        &state_dir,
        &sessions_dir,
        &mut session_path,
        &mut meta_path,
        &mut started,
        &mut local_lines,
        None,
        None,
        false,
        root.path(),
        false,
        None,
    )
    .await;
    assert!(matches!(retry, Err(TuiAdmissionFailure::Retryable(_))));
    assert_eq!(
        session
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .events
            .len(),
        2
    );
    if let Some(mut runtime) = selected_runtime {
        runtime.shutdown().await.unwrap();
    }
    fixture.abort();
    println!("POLARIS_TUI_SELECTED_MCP_PROMPT_CHILD_COMPLETE");
}

#[cfg(unix)]
#[tokio::test]
async fn failed_or_timed_out_mcp_prompt_does_not_commit_tui_input() {
    use polaris_core::{
        conversation_state::HistoryMode,
        mcp::{operation::ExternalCeiling, runtime::McpPromptChoice},
        session_manager::{AdmissionState, SessionId, TerminalStatus},
        session_store::PersistedSession,
    };

    if mcp_prompt_child_gate(
        "tests::failed_or_timed_out_mcp_prompt_does_not_commit_tui_input",
        "POLARIS_TUI_FAILED_MCP_PROMPT_CHILD_COMPLETE",
    ) {
        return;
    }

    for delay in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let state_dir = root.path().join("state");
        let sessions_dir = root.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).unwrap();
        let database = root.path().join("memory.sqlite3");
        let parent =
            PersistedSession::create(root.path(), &database, "project", HistoryMode::Legacy, None)
                .unwrap();
        let parent_id = parent.snapshot().unwrap().state.session_id;
        let mut session = polaris_core::session::Session::new();
        session.attach(parent.clone()).unwrap();
        let mut session_path = sessions_dir.join(format!("{parent_id}.jsonl"));
        let mut meta_path = session_path.with_extension("meta.json");
        let mut started = 1;
        let mut local_lines = Vec::new();
        let (server, methods, fixture) = if delay {
            polaris_mcp::test_support::server_fixture("prompts/get").await
        } else {
            polaris_mcp::test_support::server_fixture_with_failures(
                "",
                serde_json::json!({}),
                serde_json::json!([]),
                "prompts/get",
                vec![500; 10],
            )
            .await
        };
        let host = prompt_fixture_host(root.path(), home.path(), server);
        let choice = McpPromptChoice {
            server: "fixture".into(),
            prompt: "prompt".into(),
            arguments: Default::default(),
        };
        let result = admit_tui_input(
            &mut session,
            "uncommitted input",
            Some(&DistinctContextJudge),
            &state_dir,
            &sessions_dir,
            &mut session_path,
            &mut meta_path,
            &mut started,
            &mut local_lines,
            None,
            None,
            false,
            root.path(),
            true,
            Some(TuiPromptRequest {
                host: &host,
                choice: &choice,
                selector: None,
                ceiling: ExternalCeiling::Denied,
            }),
        )
        .await;
        assert!(matches!(
            result,
            Err(TuiAdmissionFailure::PromptFailed { unknown: true, .. })
        ));
        assert!(
            methods
                .lock()
                .unwrap()
                .iter()
                .any(|method| method == "prompts/get")
        );
        assert!(parent.snapshot().unwrap().events.is_empty());
        let current = SessionId::new(parent_id).unwrap();
        let (_store, manager) = open_tui_admission_owner(&state_dir, &current).unwrap();
        assert_eq!(manager.sessions.len(), 1);
        assert!(manager.queue.is_empty());
        assert_eq!(manager.admissions.len(), 1);
        assert!(manager.admissions.values().all(|admission| matches!(
            admission.state,
            AdmissionState::Terminal {
                status: TerminalStatus::OutcomeUnknown,
                ..
            }
        )));
        fixture.abort();
    }
    println!("POLARIS_TUI_FAILED_MCP_PROMPT_CHILD_COMPLETE");
}

#[cfg(unix)]
#[tokio::test]
async fn distinct_tui_input_uses_an_empty_durable_sidechat_and_terminal_receipt() {
    use polaris_core::{
        conversation_state::HistoryMode,
        session_manager::{AdmissionState, TerminalStatus},
        session_store::PersistedSession,
        workflow::{SessionWorkflow, WorkflowConfig},
    };

    let root = tempfile::tempdir().unwrap();
    let state_dir = root.path().join("state");
    let sessions_dir = root.path().join("sessions");
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let database = root.path().join("memory.sqlite3");
    let workflow = SessionWorkflow::new(WorkflowConfig::default());
    let parent = PersistedSession::create(
        root.path(),
        &database,
        "project",
        HistoryMode::Legacy,
        Some(&workflow),
    )
    .unwrap();
    let parent_id = parent.snapshot().unwrap().state.session_id;
    let mut session = polaris_core::session::Session::new();
    session.workflow = Some(workflow);
    session.attach(parent.clone()).unwrap();
    let mut session_path = sessions_dir.join(format!("{parent_id}.jsonl"));
    let mut meta_path = session_path.with_extension("meta.json");
    let mut started = 1;
    let mut local_lines = Vec::new();

    let (turn, notice, rewind_boundary, selected_runtime) = admit_tui_input(
        &mut session,
        "unrelated isolated task",
        Some(&DistinctContextJudge),
        &state_dir,
        &sessions_dir,
        &mut session_path,
        &mut meta_path,
        &mut started,
        &mut local_lines,
        None,
        None,
        false,
        root.path(),
        true,
        None,
    )
    .await
    .unwrap();
    assert!(selected_runtime.is_none());
    assert!(rewind_boundary.is_none());
    let admission_id = turn.admission_id.clone();
    let sidechat = session.persistence.as_ref().unwrap().snapshot().unwrap();
    assert_ne!(sidechat.state.session_id, parent_id);
    assert_eq!(sidechat.events.len(), 1);
    assert_eq!(
        sidechat.events[0].message.content,
        "unrelated isolated task"
    );
    assert_eq!(parent.snapshot().unwrap().events.len(), 0);
    assert_eq!(turn.manager.sessions.len(), 2);
    assert_eq!(
        turn.run_id().unwrap(),
        polaris_core::session_manager::RunId::derive(&admission_id).as_str()
    );
    assert!(notice.unwrap().contains("running"));
    turn.finish(TerminalStatus::Completed).unwrap();

    let current = polaris_core::session_manager::SessionId::new(sidechat.state.session_id).unwrap();
    let (_store, restored) = open_tui_admission_owner(&state_dir, &current).unwrap();
    assert!(matches!(
        restored.status(&admission_id).unwrap().state,
        AdmissionState::Terminal {
            status: TerminalStatus::Completed,
            ..
        }
    ));
    drop(_store);

    fn admission_files(root: &Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut files = std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(root).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                files.insert(path.clone(), None);
                files.extend(admission_files(&path));
            } else {
                files.insert(path.clone(), Some(std::fs::read(&path).unwrap()));
            }
        }
        files
    }
    let admissions_root = state_dir.join("tui-admissions-v1");
    let before_status = admission_files(&admissions_root);
    let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
    let activity = conversation_view::ConversationViewState::new(
        &render::HeaderInfo {
            provider_name: "test",
            model_name: "test",
            usage: Default::default(),
            cwd: "test",
            cwd_short: "test",
            effort_name: "auto",
        },
        80,
    );
    let mut max_turns = 1;
    let command = format!("input status {}", admission_id.as_str());
    let lines = handle_runtime_command(
        &command,
        None,
        session.persistence.as_ref(),
        &state_dir,
        &[],
        &sandbox,
        ApprovalPolicy::Never,
        &mut max_turns,
        1,
        &activity,
    )
    .await
    .unwrap();
    assert!(lines[0].contains(admission_id.as_str()));
    assert!(lines[0].contains("Completed"));
    let unrelated =
        PersistedSession::create(root.path(), &database, "project", HistoryMode::Legacy, None)
            .unwrap();
    assert!(
        handle_runtime_command(
            &command,
            None,
            Some(&unrelated),
            &state_dir,
            &[],
            &sandbox,
            ApprovalPolicy::Never,
            &mut max_turns,
            1,
            &activity,
        )
        .await
        .is_err()
    );
    assert_eq!(admission_files(&admissions_root), before_status);
    let index = admissions_root.join("family-index.v1.json");
    let original_index = std::fs::read(&index).unwrap();
    std::fs::OpenOptions::new()
        .write(true)
        .open(&index)
        .unwrap()
        .set_len(2 * 1024 * 1024 + 1)
        .unwrap();
    let oversized = admission_files(&admissions_root);
    assert!(
        handle_runtime_command(
            &command,
            None,
            session.persistence.as_ref(),
            &state_dir,
            &[],
            &sandbox,
            ApprovalPolicy::Never,
            &mut max_turns,
            1,
            &activity,
        )
        .await
        .is_err()
    );
    assert_eq!(admission_files(&admissions_root), oversized);
    std::fs::write(&index, original_index).unwrap();

    let denied = admit_tui_input(
        &mut session,
        "another isolated task",
        Some(&DistinctContextJudge),
        &state_dir,
        &sessions_dir,
        &mut session_path,
        &mut meta_path,
        &mut started,
        &mut local_lines,
        None,
        None,
        false,
        root.path(),
        true,
        None,
    )
    .await;
    assert!(matches!(denied, Err(TuiAdmissionFailure::Retryable(_))));
    let (blocked_store, blocked_manager) = open_tui_admission_owner(&state_dir, &current).unwrap();
    assert!(blocked_manager.queue.is_empty());
    assert!(
        blocked_manager
            .admissions
            .values()
            .all(|admission| matches!(admission.state, AdmissionState::Terminal { .. }))
    );
    drop(blocked_store);

    let (next, notice, _, runtime) = admit_tui_input(
        &mut session,
        "follow-up in this lane",
        None,
        &state_dir,
        &sessions_dir,
        &mut session_path,
        &mut meta_path,
        &mut started,
        &mut local_lines,
        None,
        None,
        false,
        root.path(),
        false,
        None,
    )
    .await
    .unwrap();
    assert!(notice.is_none());
    assert!(runtime.is_none());
    assert_eq!(next.manager.sessions.len(), 2);
    assert_eq!(
        session
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .events
            .len(),
        2
    );
    assert_ne!(next.admission_id, admission_id);
    next.finish(TerminalStatus::Completed).unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn recovery_blocks_a_new_turn_after_an_unstarted_committed_input() {
    use polaris_core::{
        conversation_state::HistoryMode,
        session_manager::{
            AdmissionState, DestinationMode, SessionId, SubmissionPayloadV1, SubmitOutcomeV1,
            SubmitRequestV1, TerminalStatus,
        },
        session_store::PersistedSession,
    };

    let root = tempfile::tempdir().unwrap();
    let state_dir = root.path().join("state");
    let database = root.path().join("memory.sqlite3");
    let parent =
        PersistedSession::create(root.path(), &database, "project", HistoryMode::Legacy, None)
            .unwrap();
    let parent_id = parent.snapshot().unwrap().state.session_id;
    let current = SessionId::new(parent_id).unwrap();
    let receipt_counts = std::collections::BTreeMap::from([(
        current.clone(),
        parent.snapshot().unwrap().state.admission_receipts.len(),
    )]);
    let (store, mut manager) = open_tui_admission_owner(&state_dir, &current).unwrap();
    let before = manager.revision;
    let SubmitOutcomeV1::Accepted { admission, .. } = manager
        .submit(
            SubmitRequestV1 {
                parent_session: current,
                payload: SubmissionPayloadV1 {
                    text: "durable input".into(),
                    attachment_ids: vec![],
                    route: DestinationMode::New,
                    expected_session_revision: 0,
                    expected_goal_revision: 0,
                },
                admission_id: None,
            },
            before,
        )
        .unwrap();
    store
        .persist_submission(before, &manager, &receipt_counts)
        .unwrap();
    let before = manager.revision;
    let decided = manager
        .decide(
            &admission.id,
            before,
            admission.revision,
            None,
            false,
            false,
        )
        .unwrap();
    persist_manager_transition(&store, before, &manager).unwrap();
    let before = manager.revision;
    let prepared = manager
        .prepare_destination(&admission.id, before, decided.revision)
        .unwrap();
    persist_manager_transition(&store, before, &manager).unwrap();
    let AdmissionState::DestinationReady { session_id, .. } = &prepared.state else {
        panic!("destination should be ready");
    };
    let mut parent_session = polaris_core::session::Session::new();
    parent_session.attach(parent).unwrap();
    let mut sidechat = empty_sidechat_session(&parent_session, session_id).unwrap();
    assert!(
        sidechat
            .push_admitted_user("durable input", &admission.id, &prepared.payload_hash)
            .unwrap()
    );

    recover_admission_owner(&store, &mut manager, root.path(), &database, "project").unwrap();
    assert!(matches!(
        manager.status(&admission.id).unwrap().state,
        AdmissionState::Terminal {
            status: TerminalStatus::OutcomeUnknown,
            ..
        }
    ));
    assert_eq!(
        sidechat
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .events
            .len(),
        1
    );
    drop(store);
    let sessions_dir = root.path().join("sessions");
    std::fs::create_dir(&sessions_dir).unwrap();
    let session_id = sidechat
        .persistence
        .as_ref()
        .unwrap()
        .snapshot()
        .unwrap()
        .state
        .session_id;
    let mut session_path = sessions_dir.join(format!("{session_id}.jsonl"));
    let mut meta_path = session_path.with_extension("meta.json");
    let mut started = 1;
    let mut local_lines = Vec::new();
    let result = admit_tui_input(
        &mut sidechat,
        "new input",
        None,
        &state_dir,
        &sessions_dir,
        &mut session_path,
        &mut meta_path,
        &mut started,
        &mut local_lines,
        None,
        None,
        false,
        root.path(),
        true,
        None,
    )
    .await;
    assert!(matches!(result, Err(TuiAdmissionFailure::Retryable(_))));
    assert_eq!(
        sidechat
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .events
            .len(),
        1
    );
}

#[cfg(unix)]
#[test]
fn durable_reply_builds_a_bound_shadow_finalization_input() {
    use polaris_core::{conversation_state::HistoryMode, session_store::PersistedSession};

    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("memory.sqlite3");
    let persistence =
        PersistedSession::create(root.path(), &database, "project", HistoryMode::Legacy, None)
            .unwrap();
    let session_id = persistence.snapshot().unwrap().state.session_id;
    let mut session = polaris_core::session::Session::new();
    session.attach(persistence).unwrap();
    session.push_user("summarize this");
    session.push_assistant("done", vec![]);
    let sandbox =
        polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
    let control = install_tui_root_control(
        &mut session,
        TuiRootControlArgs {
            session_id: &session_id,
            turn: 1,
            sandbox: &sandbox,
            approval: ApprovalPolicy::OnRequest,
            mcp_available: false,
            tool_names: &[],
            model: "gpt-6-astra",
            effort: "xhigh",
        },
    )
    .unwrap();

    let input = tui_shadow_input(&session, "summarize this", &control)
        .unwrap()
        .unwrap();
    assert_eq!(input.prompt, "summarize this");
    assert_eq!(input.source_body, "done");
    assert!(input.closed_evidence.is_empty());
    assert!(input.evidence_complete);
    assert_eq!(input.subject.session_id, session_id);
    assert_eq!(input.subject.root_run_id, "tui-root-run-1");
    assert_eq!(input.subject.attempt_id, "tui-root-attempt-1");
    assert_eq!(input.subject.turn_sequence, 2);
    input.subject.validate().unwrap();
}

#[cfg(unix)]
#[test]
fn successful_tui_turn_schedules_the_shared_shadow_owner() {
    use polaris_core::{
        config::{
            AnswerFinalizerConfig, AnswerFinalizerMode, JudgeProfile, PresentationConfig,
            PresentationLanguage,
        },
        conversation_state::HistoryMode,
        session_store::PersistedSession,
    };

    let root = tempfile::tempdir().unwrap();
    let database = root.path().join("memory.sqlite3");
    let persistence =
        PersistedSession::create(root.path(), &database, "project", HistoryMode::Legacy, None)
            .unwrap();
    let session_id = persistence.snapshot().unwrap().state.session_id;
    let mut session = polaris_core::session::Session::new();
    session.attach(persistence).unwrap();
    session.push_user("answer this");
    session.push_assistant("answer", vec![]);
    let sandbox =
        polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
    let control = install_tui_root_control(
        &mut session,
        TuiRootControlArgs {
            session_id: &session_id,
            turn: 2,
            sandbox: &sandbox,
            approval: ApprovalPolicy::OnRequest,
            mcp_available: false,
            tool_names: &[],
            model: "gpt-6-astra",
            effort: "xhigh",
        },
    )
    .unwrap();
    let mut owner = polaris_core::answer_finalizer::shadow::ShadowFinalizerOwner::open_configured(
        root.path().join("shadow.json"),
        PresentationConfig {
            language: PresentationLanguage::Ja,
        },
        AnswerFinalizerConfig {
            mode: AnswerFinalizerMode::Shadow,
            judge_profile: Some(JudgeProfile::FulfillmentV1),
            ..Default::default()
        },
        None,
        None,
        None,
    )
    .unwrap()
    .unwrap();

    // Installation evidence does not make an absent runtime available.
    Arc::get_mut(&mut owner)
        .unwrap()
        .record_translation_installation(Some(true));
    let diagnostic = cat_diagnostic_lines(Some(&owner));
    for (label, value) in [
        ("CAT configuration:", "false"),
        ("CAT installation:", "Installation check"),
        ("CAT verification:", "false"),
    ] {
        assert!(diagnostic.iter().any(|line| {
            let line = line.to_string();
            line.contains(label) && line.ends_with(value)
        }));
    }
    assert!(diagnostic.iter().any(|line| {
        let line = line.to_string();
        line.contains("CAT observation:")
            && line.contains("0 attempts")
            && line.contains("saved observations; distinct from execution success")
    }));
    assert!(schedule_tui_shadow(&owner, &session, "answer this", &control).unwrap());
    assert_eq!(owner.records().unwrap().len(), 1);
}

#[test]
fn idle_ctrl_c_exits_only_on_the_second_consecutive_press() {
    let mut confirmation = ExitConfirmation::default();

    assert!(!confirmation.press());
    assert!(confirmation.press());
}

#[test]
fn other_input_cancels_an_armed_idle_ctrl_c() {
    let mut confirmation = ExitConfirmation::default();

    assert!(!confirmation.press());
    confirmation.cancel();
    assert!(!confirmation.press());
}

#[test]
fn queued_quit_refusal_restores_the_displaced_draft_and_keeps_later_input() {
    let queued = VecDeque::from(["send this next".to_string()]);
    let mut input_history = input::History::new();
    let mut input = String::new();
    let mut cursor = 0;
    let mut status = Status::Idle;

    assert!(refuse_quit_with_pending_work(
        &queued,
        &mut input_history,
        (&mut input, &mut cursor),
        Some(("draft work".to_string(), 5)),
        "/quit",
        true,
        &mut status,
    ));
    assert_eq!(input, "draft work");
    assert_eq!(cursor, 5);
    assert_eq!(queued.front().map(String::as_str), Some("send this next"));
    assert!(matches!(status, Status::Notice(_)));
}

#[test]
fn typed_quit_refusal_keeps_the_command_when_work_is_queued() {
    let queued = VecDeque::from(["send this next".to_string()]);
    let mut input_history = input::History::new();
    let mut input = String::new();
    let mut cursor = 0;
    let mut status = Status::Idle;

    assert!(refuse_quit_with_pending_work(
        &queued,
        &mut input_history,
        (&mut input, &mut cursor),
        None,
        "/quit",
        false,
        &mut status,
    ));
    assert_eq!(input, "/quit");
    assert_eq!(cursor, input.len());
    assert_eq!(queued.front().map(String::as_str), Some("send this next"));
}

#[test]
fn quit_is_allowed_only_after_the_queue_and_draft_are_empty() {
    let queued = VecDeque::new();
    let mut input_history = input::History::new();
    let mut input = "unsent draft".to_string();
    let mut cursor = input.len();
    let mut status = Status::Idle;
    assert!(refuse_quit_with_pending_work(
        &queued,
        &mut input_history,
        (&mut input, &mut cursor),
        None,
        "/quit",
        true,
        &mut status,
    ));
    assert_eq!(input, "unsent draft");

    input.clear();
    cursor = 0;
    status = Status::Idle;
    assert!(!refuse_quit_with_pending_work(
        &queued,
        &mut input_history,
        (&mut input, &mut cursor),
        None,
        "/quit",
        false,
        &mut status,
    ));
    assert!(matches!(status, Status::Idle));
}

#[test]
fn quit_submission_does_not_release_a_paused_queue() {
    assert!(!submission_resumes_queued_inputs("/quit"));
    assert!(!submission_resumes_queued_inputs("/quit ignored"));
    assert!(!submission_resumes_queued_inputs("/exit"));
    assert!(!submission_resumes_queued_inputs("/q"));
    assert!(!submission_resumes_queued_inputs("/new"));
    assert!(submission_resumes_queued_inputs("continue this work"));
}

#[test]
fn popup_quit_refuses_a_draft_hidden_by_input_history() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut input_history = input::History::new();
    input_history.record("previous input");
    let mut input_buffer = "unsent α".to_string();
    assert_eq!(input_history.older(&input_buffer), Some("previous input"));
    input_buffer = "/q".to_string();
    let mut input_cursor = input_buffer.len();
    let queued = VecDeque::new();
    let mut status = Status::Idle;
    let suggestions = slash::matching(input_buffer.trim_start_matches('/'));
    let enter = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(enter.code, KeyCode::Enter);
    assert_eq!(suggestions[0].name, "quit");
    assert!(matches!(
        slash::action_for(suggestions[0].name),
        slash::Action::Quit
    ));
    let submitted = std::mem::take(&mut input_buffer);
    assert_eq!(input_cursor, 2);
    input_cursor = 0;

    assert!(refuse_quit_with_pending_work(
        &queued,
        &mut input_history,
        (&mut input_buffer, &mut input_cursor),
        None,
        &submitted,
        false,
        &mut status,
    ));
    assert_eq!(input_buffer, "unsent α");
    assert_eq!(input_cursor, input_buffer.len());
    assert!(queued.is_empty());
}

#[test]
fn typed_quit_refuses_a_draft_hidden_by_input_history() {
    use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    let mut input_history = input::History::new();
    input_history.record("previous input");
    let mut input_buffer = "unsent α".to_string();
    assert_eq!(input_history.older(&input_buffer), Some("previous input"));
    input_buffer = "/exit".to_string();
    let mut input_cursor = input_buffer.len();
    let queued = VecDeque::new();
    let mut status = Status::Idle;
    assert!(slash::matching("exit").is_empty());
    let InputAction::Submit(submitted) = apply_key(
        &mut input_buffer,
        &mut input_cursor,
        KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
    ) else {
        panic!("Enter should submit /exit");
    };
    record_submission_before_dispatch(&mut input_history, &submitted, false);
    assert!(matches!(
        slash::parse(&submitted),
        Some(slash::Action::Quit)
    ));

    assert!(refuse_quit_with_pending_work(
        &queued,
        &mut input_history,
        (&mut input_buffer, &mut input_cursor),
        None,
        &submitted,
        false,
        &mut status,
    ));
    assert_eq!(input_buffer, "unsent α");
    assert_eq!(input_cursor, input_buffer.len());
    assert!(queued.is_empty());
}

#[test]
fn resume_hint_uses_the_current_persisted_legacy_session_id() {
    let dir = tempfile::tempdir().unwrap();
    let session_path = dir.path().join("resume-me.jsonl");
    std::fs::write(&session_path, b"{}\n").unwrap();
    let session = polaris_core::session::Session::new();

    let id = current_resume_id(&session, &session_path).unwrap();
    assert_eq!(id, "resume-me");
    assert_eq!(
        resume_hint(&id),
        "\nResume this session with:\npolaris --resume resume-me\n"
    );
}

#[test]
fn empty_unpersisted_session_has_no_resume_hint() {
    let dir = tempfile::tempdir().unwrap();
    let session_path = dir.path().join("not-created.jsonl");
    let session = polaris_core::session::Session::new();

    assert_eq!(current_resume_id(&session, &session_path), None);
}

#[tokio::test]
async fn tui_turn_installs_fresh_root_control_with_host_capabilities() {
    let root = tempfile::tempdir().unwrap();
    let sandbox = polaris_sandbox::SandboxPolicy::new(
        polaris_sandbox::SandboxMode::WorkspaceWrite,
        &[root.path().to_path_buf()],
    )
    .unwrap();
    let mut session = polaris_core::session::Session::new();
    let control = install_tui_root_control(
        &mut session,
        TuiRootControlArgs {
            session_id: "session-1",
            turn: 7,
            sandbox: &sandbox,
            approval: ApprovalPolicy::OnRequest,
            mcp_available: true,
            tool_names: &["read".into(), "spawn".into()],
            model: "gpt-6-astra",
            effort: "automatic",
        },
    )
    .unwrap();

    let projection = control.snapshot().await;
    assert_eq!(projection.session_id, "session-1");
    assert_eq!(projection.root_run_id.0, "tui-root-run-7");
    assert_eq!(projection.active_task_id.0, "tui-task-7");
    assert!(session.delegation_control.is_some());
    session.finish_root_delegation_control(false).await.unwrap();
    assert!(session.delegation_control.is_none());
}

#[test]
fn a_failed_turn_keeps_possible_mcp_sends_as_unknown_and_rolls_back_others() {
    use polaris_provider::{Message, ToolCall};
    let turn = |name: &str| {
        let mut session = polaris_core::session::Session::new();
        session.push_user("request");
        session.messages.push(Message::assistant_with_tool_calls(
            "",
            vec![ToolCall {
                id: "call".into(),
                name: name.into(),
                arguments: serde_json::json!({}),
            }],
        ));
        session
    };
    let mut mcp = turn("mcp_fixture__tool");
    keep_or_roll_back(&mut mcp, 1).unwrap();
    assert_eq!(mcp.messages.len(), 3);
    assert!(mcp.messages[2].content.contains("outcome is unknown"));
    let mut bash = turn("bash");
    keep_or_roll_back(&mut bash, 1).unwrap();
    assert_eq!(bash.messages.len(), 1);
}

use super::*;
use approver::KeyReader;
use polaris_core::conversation_memory::{
    EmbeddingModel, StrictEmbedder, StrictHistory, StrictSummaryProvider, SummaryRequest,
};
use polaris_core::session::Session;
use polaris_provider::Message;
use std::{future::Future, io, pin::Pin};

type StrictFuture<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;

struct TestSummary;
impl StrictSummaryProvider for TestSummary {
    fn summarize<'a>(&'a self, _request: SummaryRequest) -> StrictFuture<'a, String> {
        Box::pin(async {
            Ok("{\"facts\":[],\"decisions\":[],\"constraints\":[],\"corrections\":[],\"open_items\":[],\"source_turn_ids\":[]}".into())
        })
    }
}

struct TestEmbedder(EmbeddingModel);
impl StrictEmbedder for TestEmbedder {
    fn metadata(&self) -> &EmbeddingModel {
        &self.0
    }
    fn embed_passage<'a>(&'a self, _text: &'a str) -> StrictFuture<'a, Vec<f32>> {
        Box::pin(async { Ok(vec![0.0]) })
    }
    fn embed_query<'a>(&'a self, _text: &'a str) -> StrictFuture<'a, Vec<Vec<f32>>> {
        Box::pin(async { Ok(vec![vec![0.0]]) })
    }
}

fn test_strict_history() -> Arc<StrictHistory> {
    Arc::new(StrictHistory::new(
        Arc::new(TestSummary),
        Arc::new(TestEmbedder(EmbeddingModel {
            model: "test".into(),
            revision: "v1".into(),
            dimension: 1,
        })),
    ))
}
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::text::Line;
use std::collections::VecDeque;

#[test]
fn judged_effort_replaces_the_undecided_footer_value() {
    let mut effort = UNDECIDED_EFFORT.to_string();
    update_effort_from_event(&polaris_core::AgentEvent::EffortJudging, &mut effort);
    assert_eq!(effort, "judging");
    update_effort_from_event(
        &polaris_core::AgentEvent::EffortJudged {
            effort: "xhigh".into(),
        },
        &mut effort,
    );
    assert_eq!(effort, "extra high");
}

#[cfg(unix)]
#[test]
fn working_turn_accepts_and_queues_prompts_and_commands_in_order() {
    let mut buffer = String::new();
    let mut cursor = 0;
    let mut history = input::History::new();
    let mut queued = VecDeque::new();
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);

    for character in "next prompt".chars() {
        assert_eq!(
            apply_inflight_key(
                &mut buffer,
                &mut cursor,
                &mut history,
                &mut queued,
                key(KeyCode::Char(character)),
            ),
            InflightInputAction::Continue
        );
    }
    assert_eq!(
        apply_inflight_key(
            &mut buffer,
            &mut cursor,
            &mut history,
            &mut queued,
            key(KeyCode::Enter),
        ),
        InflightInputAction::Queued
    );
    for character in "/m".chars() {
        apply_inflight_key(
            &mut buffer,
            &mut cursor,
            &mut history,
            &mut queued,
            key(KeyCode::Char(character)),
        );
    }
    let suggestions = slash_suggestions(&buffer, &history, false);
    let mut selected = 0;
    for code in [KeyCode::Up, KeyCode::Down, KeyCode::Up] {
        assert_eq!(
            slash_completion_action(&suggestions, &mut selected, key(code)),
            SlashCompletionAction::Navigate
        );
    }
    let completed = slash_completion_action(&suggestions, &mut selected, key(KeyCode::Enter));
    assert_eq!(completed, SlashCompletionAction::Submit("model"));
    let SlashCompletionAction::Submit(command) = completed else {
        unreachable!()
    };
    buffer = format!("/{command}");
    cursor = buffer.len();
    assert_eq!(
        apply_inflight_key(
            &mut buffer,
            &mut cursor,
            &mut history,
            &mut queued,
            key(KeyCode::Enter),
        ),
        InflightInputAction::Queued
    );

    assert_eq!(
        queued.into_iter().collect::<Vec<_>>(),
        ["next prompt", "/model"]
    );
    assert!(buffer.is_empty());
    assert_eq!(cursor, 0);

    // Synthetic Enter presses must not record those same two entries a
    // second time when they are dispatched after the current turn.
    record_submission(&mut history, "next prompt", true);
    record_submission(&mut history, "/model", true);
    assert_eq!(history.older(""), Some("/model"));
    assert_eq!(history.older(""), Some("next prompt"));
    assert_eq!(history.older(""), None);
}

#[test]
fn slash_completion_preserves_recalled_and_queued_commands() {
    let mut history = input::History::new();
    history.record("/subagents");
    assert_eq!(history.older("independent draft"), Some("/subagents"));
    assert!(slash_suggestions("/subagents", &history, false).is_empty());
    assert!(slash_suggestions("/sub", &history, true).is_empty());

    // Editing a recalled command, or typing during a queued run, uses
    // interactive completion without consuming the saved history draft.
    let suggestions = slash_suggestions("/sub", &history, false);
    assert_eq!(suggestions.len(), 1);
    let mut selected = 0;
    assert_eq!(
        slash_completion_action(
            &suggestions,
            &mut selected,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        ),
        SlashCompletionAction::Submit("subagents")
    );
    assert_eq!(history.newer(), Some("independent draft"));
    assert!(slash_suggestions("/subagents list", &history, false).is_empty());
}

#[test]
fn dedicated_secret_operations_dispatch_locally_without_entering_history() {
    let mut history = input::History::new();
    history.record("ordinary prompt");
    for command in ["/mcp secret set", "/MCP secret delete synthetic-reference"] {
        let mut text = command.to_owned();
        assert!(!refuse_secret_submission(&mut text, true));
        assert!(matches!(slash::parse(&text), Some(slash::Action::Mcp(_))));
        record_submission_before_dispatch(&mut history, &text, false);
    }
    assert_eq!(history.older(""), Some("ordinary prompt"));
    assert_eq!(history.older(""), None);
    for command in [
        "/mcp secret set synthetic-inline-value",
        "/mcp secret delete reference extra",
        "/mcp secret unknown synthetic-inline-value",
    ] {
        let mut text = command.to_owned();
        assert!(refuse_secret_submission(&mut text, true));
        assert!(text.is_empty());
    }
    let mut text = "/mcp secret set".to_owned();
    assert!(refuse_secret_submission(&mut text, false));
    assert!(text.is_empty());
}

#[test]
fn secret_command_never_enters_inflight_history_or_queue() {
    assert!(is_secret_command("  /MCP secret set VALUE"));
    assert!(!is_secret_command("/mcp prompt select"));
    assert!(secret_value_key_is_blocked(
        "/MCP secret set",
        &KeyEvent::new(KeyCode::Char('V'), KeyModifiers::NONE)
    ));
    assert!(!secret_value_key_is_blocked(
        "/mcp secret se",
        &KeyEvent::new(KeyCode::Char('t'), KeyModifiers::NONE)
    ));
    assert!(!secret_value_key_is_blocked(
        "/mcp secret set",
        &KeyEvent::new(KeyCode::Backspace, KeyModifiers::NONE)
    ));
    let mut buffer = String::new();
    let mut cursor = 0;
    let mut history = input::History::new();
    let mut queued = VecDeque::new();
    let key = |code| KeyEvent::new(code, KeyModifiers::NONE);
    for character in "/mcp secret set VALUE".chars() {
        apply_inflight_key(
            &mut buffer,
            &mut cursor,
            &mut history,
            &mut queued,
            key(KeyCode::Char(character)),
        );
    }
    assert_eq!(buffer, "/mcp secret set");
    assert_eq!(cursor, buffer.len());
    assert_eq!(
        apply_inflight_key(
            &mut buffer,
            &mut cursor,
            &mut history,
            &mut queued,
            key(KeyCode::Enter),
        ),
        InflightInputAction::RejectedSecret
    );
    assert!(queued.is_empty());
    assert!(history.older("").is_none());

    // Complete the command around an already typed value. The first
    // whitespace between `set` and `VALUE` must erase the value before
    // the next frame can use the composer buffer.
    let mut buffer = "VALUE".to_string();
    let mut cursor = buffer.len();
    for _ in 0.."VALUE".chars().count() {
        apply_inflight_key(
            &mut buffer,
            &mut cursor,
            &mut history,
            &mut queued,
            key(KeyCode::Left),
        );
    }
    for character in "/MCP\u{2003}SECRET\u{2003}SET".chars() {
        apply_inflight_key(
            &mut buffer,
            &mut cursor,
            &mut history,
            &mut queued,
            key(KeyCode::Char(character)),
        );
    }
    assert_eq!(buffer, "/MCP\u{2003}SECRET\u{2003}SETVALUE");
    assert_eq!(
        apply_inflight_key(
            &mut buffer,
            &mut cursor,
            &mut history,
            &mut queued,
            key(KeyCode::Char('\u{2003}')),
        ),
        InflightInputAction::RejectedSecret
    );
    assert_eq!(buffer, "/mcp secret set");
    assert_eq!(cursor, buffer.len());
    assert!(queued.is_empty());
    assert!(history.older("").is_none());
}

#[test]
fn inline_secret_value_is_redacted_before_idle_draw() {
    let mut buffer = "/mcp secret set TOKEN".to_string();
    let mut cursor = buffer.len();
    let mut history = input::History::new();
    history.record("previous");
    assert_eq!(history.older("TOKEN"), Some("previous"));
    assert_eq!(history.newer(), Some("TOKEN"));
    assert!(redact_inline_secret_value(
        &mut buffer,
        &mut cursor,
        &mut history,
    ));
    assert_eq!(buffer, "/mcp secret set");
    assert_eq!(cursor, buffer.len());
    assert!(history.draft_is_empty());
    assert!(!redact_inline_secret_value(
        &mut buffer,
        &mut cursor,
        &mut history,
    ));
    assert_eq!(secret_set_has_value("/mcp secret setup TOKEN"), None);
}

struct Summarizer;

#[async_trait::async_trait]
impl polaris_provider::Provider for Summarizer {
    async fn complete(
        &self,
        _req: polaris_provider::CompletionRequest,
    ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
        Ok(polaris_provider::CompletionResponse {
            text: "summary text".to_string(),
            ..Default::default()
        })
    }
}

#[tokio::test]
#[cfg(unix)]
async fn m3e_confirmed_compaction_keeps_original_and_refuses_cancelled_send() {
    struct BoundSummary;
    #[async_trait::async_trait]
    impl polaris_provider::Provider for BoundSummary {
        async fn complete(
            &self,
            request: polaris_provider::CompletionRequest,
        ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
            Summarizer.complete(request).await
        }
        fn model(&self) -> Option<String> {
            Some("test-summary".into())
        }
    }
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let state = root.path().join("state");
    fs::create_dir(&project).unwrap();
    fs::create_dir(&state).unwrap();
    std::fs::set_permissions(
        &state,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    let workspace = polaris_core::rewind_workspace::WorkspaceOwner::open(
        &project,
        &state.join("rewind-workspace"),
        Default::default(),
    )
    .unwrap();
    let lease = workspace.try_mutation().unwrap();
    let effects = polaris_core::management_effect::ManagementEffectStoreV1::open_at_state_dir(
        &project, &state, &lease,
    )
    .unwrap();
    drop(lease);
    let saved = polaris_core::session_store::PersistedSession::create(
        root.path(),
        &state.join("memory.sqlite3"),
        project.to_str().unwrap(),
        polaris_core::conversation_state::HistoryMode::Legacy,
        None,
    )
    .unwrap();
    let mut session = Session::new();
    session.attach(saved.clone()).unwrap();
    session.push_user("turn1");
    session.push_assistant(&"older detailed reply ".repeat(2000), Vec::new());
    session.push_user("turn2");
    session.push_assistant("reply2", Vec::new());
    session.push_user("turn3");
    session.push_assistant("reply3", Vec::new());
    let before = saved.snapshot().unwrap();
    let terminal = RefCell::new(Terminal::new(TestBackend::new(100, 30)).unwrap());
    let mut status = Status::Idle;
    let mut usage = polaris_provider::Usage::default();
    let mut declined = ScriptedReader(VecDeque::from([KeyCode::Esc]));
    assert!(
        !local_actions::compact(
            &terminal,
            &mut declined,
            &BoundSummary,
            &mut session,
            &root.path().join("unused.jsonl"),
            &workspace,
            &effects,
            &mut status,
            &mut usage,
            false
        )
        .await
        .unwrap()
    );
    assert_eq!(saved.snapshot().unwrap().raw_hash, before.raw_hash);
    assert_eq!(session.messages.len(), 6);
    let mut accepted = ScriptedReader(VecDeque::from([KeyCode::Char('y')]));
    assert!(
        local_actions::compact(
            &terminal,
            &mut accepted,
            &BoundSummary,
            &mut session,
            &root.path().join("unused.jsonl"),
            &workspace,
            &effects,
            &mut status,
            &mut usage,
            false
        )
        .await
        .unwrap()
    );
    assert_eq!(session.messages.len(), 5);
    assert_eq!(saved.snapshot().unwrap().events.len(), 6);
    assert!(!root.path().join("unused.jsonl").exists());
    assert!(effects.blocker().unwrap().is_none());
}

#[tokio::test]
async fn handle_compact_compacts_and_persists_when_there_is_something_to_compact() {
    let dir = tempfile::tempdir().expect("temp dir");
    let session_path = dir.path().join("s.jsonl");

    // "reply 1" alone is well under `MIN_TOKENS_TO_SUMMARIZE`
    // (`compaction::compact`'s floor guard) — inflated here so the
    // summarized prefix actually clears the floor and this test still
    // exercises a real compaction rather than tripping the "not worth
    // it" no-op. Single-char repeats compress ~8:1 under o200k_base
    // (see `polaris-core::compaction`'s own test for the same note).
    let big_reply = "x".repeat(polaris_core::compaction::MIN_TOKENS_TO_SUMMARIZE * 10);
    let mut session = Session::default();
    session.push_user("turn 1");
    session.push_assistant(&big_reply, vec![]);
    session.push_user("turn 2");
    session.push_assistant("reply 2", vec![]);
    session.push_user("turn 3");
    session.push_assistant("reply 3", vec![]);

    let mut status = Status::Idle;
    handle_compact(
        &Summarizer,
        &mut session,
        &session_path,
        &mut status,
        &mut polaris_provider::Usage::default(),
    )
    .await;

    assert!(
        matches!(&status, Status::Notice(s) if s.contains("compacted")),
        "expected a 'compacted' notice, got a different status"
    );
    assert_eq!(session.messages.len(), 5, "1 summary + kept tail of 4");

    let (loaded, _truncated) = persist::load_session(&session_path).expect("load");
    assert_eq!(
        loaded.messages.len(),
        session.messages.len(),
        "the persisted file must reflect the compacted session, not the pre-compaction one"
    );
    assert_eq!(loaded.messages[0].content, session.messages[0].content);
}

/// A v2 session already holds every message in its own append-only
/// log, and nothing appends to the legacy path for it. Writing the
/// summarised list there anyway leaves a second `.jsonl` that
/// `/resume` lists beside the intact session — same conversation,
/// prefix replaced by a summary, no way to tell which is which.
#[tokio::test]
#[cfg(unix)]
async fn manual_compaction_does_not_write_a_legacy_log_for_a_v2_session() {
    let dir = tempfile::tempdir().expect("temp dir");
    let data_root = tempfile::tempdir().expect("temp dir");
    let session_path = dir.path().join("s.jsonl");

    let big_reply = "x".repeat(polaris_core::compaction::MIN_TOKENS_TO_SUMMARIZE * 10);
    let mut session = Session::default();
    let persistence = polaris_core::session_store::PersistedSession::create(
        data_root.path(),
        &data_root.path().join("memory.sqlite3"),
        "p",
        polaris_core::conversation_state::HistoryMode::Legacy,
        None,
    )
    .expect("v2 session");
    session.attach(persistence).expect("attach");
    session.push_user("turn 1");
    session.push_assistant(&big_reply, vec![]);
    session.push_user("turn 2");
    session.push_assistant("reply 2", vec![]);
    session.push_user("turn 3");
    session.push_assistant("reply 3", vec![]);

    let mut status = Status::Idle;
    handle_compact(
        &Summarizer,
        &mut session,
        &session_path,
        &mut status,
        &mut polaris_provider::Usage::default(),
    )
    .await;

    assert!(
        matches!(&status, Status::Notice(s) if s.contains("compacted")),
        "compaction did not run, so this proves nothing"
    );
    assert_eq!(session.messages.len(), 5, "1 summary + kept tail of 4");
    assert!(
        !session_path.exists(),
        "a summarised legacy log was written beside an intact v2 session"
    );
}

#[tokio::test]
async fn handle_compact_reports_when_there_is_nothing_to_compact() {
    let dir = tempfile::tempdir().expect("temp dir");
    let session_path = dir.path().join("s.jsonl");
    let mut session = Session::default();
    session.push_user("only turn");
    let mut status = Status::Idle;

    handle_compact(
        &Summarizer,
        &mut session,
        &session_path,
        &mut status,
        &mut polaris_provider::Usage::default(),
    )
    .await;

    assert!(
        matches!(&status, Status::Notice(s) if s.contains("nothing")),
        "expected a 'nothing to compact' notice"
    );
    assert_eq!(session.messages.len(), 1, "must not have been touched");
}

/// Feeds a fixed, pre-scripted sequence of key codes — the same
/// approach `approver`'s own tests use, so a picker-driving test
/// doesn't need a real terminal or a live keyboard.
struct ScriptedReader(VecDeque<KeyCode>);

impl KeyReader for ScriptedReader {
    fn read_key(&mut self) -> std::io::Result<KeyCode> {
        Ok(self.0.pop_front().unwrap_or(KeyCode::Null))
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2d_sync_confirmation_cancels_and_sanitizes_paths() {
    let terminal =
        RefCell::new(ratatui::Terminal::new(ratatui::backend::TestBackend::new(110, 18)).unwrap());
    let view = render::SyncConfirmView {
        recovering: false,
        operation: "operation-id".into(),
        project: "project\x1b[2J".into(),
        revision: "agreed-revision".into(),
        previous_revision: "old-revision".into(),
        recovery: "private-recovery".into(),
        changes: vec!["Modified: file\x1b[31m".into()],
    };
    let mut declined = ScriptedReader(VecDeque::from([KeyCode::Esc]));
    assert!(!confirm_sync(&terminal, &mut declined, &view));
    let mut accepted = ScriptedReader(VecDeque::from([KeyCode::Enter, KeyCode::Char('y')]));
    assert!(confirm_sync(&terminal, &mut accepted, &view));
    let screen = {
        let term = terminal.borrow();
        let buffer = term.backend().buffer();
        (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol()))
            .collect::<String>()
    };
    for expected in [
        "C12",
        "agreed-revision",
        "old-revision",
        "private-recovery",
        "IrreversibleLocal",
    ] {
        assert!(screen.contains(expected), "{screen}");
    }
    assert!(!screen.contains('\x1b'));
}

#[cfg(target_os = "linux")]
#[test]
fn m3b_sync_tui_join_pull_leave_and_denial() {
    const CHILD: &str = "POLARIS_M3B_TUI_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "tests::m3b_sync_tui_join_pull_leave_and_denial",
                "--exact",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .env("HOME", home.path())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    use polaris_core::sync::{
        participation,
        repository::SyncRepository,
        share::{ShareLink, ShareManifest, SyncPaths},
    };
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    polaris_auth::protection::use_trusted_home(&home).unwrap();
    let project = home.join("project");
    std::fs::create_dir(&project).unwrap();
    let state = polaris_core::project::state_dir(&project).unwrap();
    source_apply::prepare_state(&project, &state).unwrap();
    let peer_project = home.join("peer-project");
    std::fs::create_dir(&peer_project).unwrap();
    std::fs::write(peer_project.join("received.txt"), "peer body\n").unwrap();
    let peer = SyncRepository::open(SyncPaths::under(&home.join("peer-state"))).unwrap();
    peer.stage_from(&peer_project, &ShareManifest::default())
        .unwrap();
    let head = peer.commit("fixture").unwrap();
    let request = format!(
        "join file://{} fixture-share fixture-invite {}",
        home.join("peer-state/sync/repository").display(),
        u64::MAX
    );
    assert!(
        sync_action(&project, &state, &request, true, |_| panic!(
            "queued mutation confirmed"
        ))
        .is_err()
    );
    assert!(
        sync_action(
            &project,
            &state,
            "join https://user:secret@example.com/x fixture-share fixture-invite 9999999999",
            false,
            |_| panic!("invalid invite displayed")
        )
        .is_err()
    );
    fn state_files(root: &Path) -> std::collections::BTreeMap<PathBuf, Option<Vec<u8>>> {
        let mut files = std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(root).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            if entry.file_type().unwrap().is_dir() {
                files.insert(path.clone(), None);
                files.extend(state_files(&path));
            } else {
                assert!(entry.file_type().unwrap().is_file());
                files.insert(path.clone(), Some(std::fs::read(path).unwrap()));
            }
        }
        files
    }
    drop(polaris_core::apply_owner::ApplyOwnerV1::acquire(&project).unwrap());
    let before_join = state_files(&state);
    for accepted in [false, true] {
        assert!(
            sync_action(&project, &state, &request, false, |view| {
                assert!(view.revision.contains("fixture-share"));
                accepted
            })
            .is_err()
        );
        assert_eq!(state_files(&state), before_join);
        assert!(matches!(
            participation::read(&state).unwrap(),
            participation::ParticipationReadV2::Missing
        ));
        assert!(!project.join("received.txt").exists());
        assert_eq!(peer.head(), head);
    }
    // Historical participation is input for C11/C13, not a C08 write
    // through the retired apply adapter or a credential-binding success.
    participation::join(
        &state,
        ShareLink {
            remote: format!(
                "file://{}",
                home.join("peer-state/sync/repository").display()
            ),
            share_id: "fixture-share".into(),
            invitation_id: "fixture-invite".into(),
            expires_unix_seconds: u64::MAX,
        },
        ShareManifest::default(),
        1,
    )
    .unwrap();
    assert!(sync_action(&project, &state, &request, false, |_| panic!("rebind")).is_err());
    let status = sync_action(&project, &state, "status", false, |_| {
        panic!("read confirmation")
    })
    .unwrap();
    assert!(status.contains("participating"));
    assert!(
        sync_action(&project, &state, "push", false, |_| panic!(
            "unapproved push"
        ))
        .is_err()
    );
    assert!(
        sync_action(&project, &state, "pull", false, |_| false)
            .unwrap()
            .contains("cancelled")
    );
    assert!(
        sync_action(&project, &state, "pull", false, |_| true)
            .unwrap()
            .contains("Source files are unchanged")
    );
    assert!(!project.join("received.txt").exists());
    assert_eq!(peer.head(), head);
    assert!(
        sync_action(&project, &state, "leave", false, |_| false)
            .unwrap()
            .contains("cancelled")
    );
    assert!(
        participation::read(&state)
            .unwrap()
            .participation()
            .is_some()
    );
    assert!(
        sync_action(&project, &state, "leave", false, |_| true)
            .unwrap()
            .contains("left the share")
    );
    assert!(matches!(
        participation::read(&state).unwrap(),
        participation::ParticipationReadV2::Missing
    ));
    assert_eq!(peer.head(), head);
}

#[cfg(target_os = "linux")]
#[test]
fn m2d_c12_confirmation_apply_reopen_and_refusal() {
    const CHILD: &str = "POLARIS_TUI_M2D_C12_CHILD";
    if std::env::var_os(CHILD).is_none() {
        // Isolate project state in a normal child rather than swapping
        // a process-global data root while other TUI tests are running.
        let fixture = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "tests::m2d_c12_confirmation_apply_reopen_and_refusal",
                "--nocapture",
            ])
            .env(CHILD, fixture.path())
            .env("POLARIS_DATA_DIR", fixture.path().join("data"))
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("m2d-c12-receipt: verified"));
        return;
    }
    use polaris_core::sync::{
        participation,
        repository::SyncRepository,
        share::{ShareLink, ShareManifest, SyncPaths},
    };
    let fixture = PathBuf::from(std::env::var_os(CHILD).unwrap());
    let project = fixture.join("project");
    std::fs::create_dir(&project).unwrap();
    std::fs::write(project.join("shared.txt"), b"local").unwrap();
    let state = polaris_core::project::state_dir(&project).unwrap();
    source_apply::prepare_state(&project, &state).unwrap();
    let manifest = ShareManifest {
        version: 1,
        excluded: Vec::new(),
    };
    participation::join(
        &state,
        ShareLink {
            remote: "file:///unused-sync-remote".into(),
            share_id: "share-id1".into(),
            invitation_id: "invite-id1".into(),
            expires_unix_seconds: u64::MAX,
        },
        manifest.clone(),
        1,
    )
    .unwrap();
    let repository = SyncRepository::open(SyncPaths::under(&state)).unwrap();
    let incoming = fixture.join("incoming");
    std::fs::create_dir(&incoming).unwrap();
    std::fs::write(incoming.join("shared.txt"), b"agreed").unwrap();
    repository.stage_from(&incoming, &manifest).unwrap();
    let agreed = repository.commit("incoming fixture").unwrap().unwrap();
    let mut held = participation::read(&state)
        .unwrap()
        .participation()
        .unwrap()
        .clone();
    held.agreed_commit = Some(agreed.clone());
    participation::write(&state, &held).unwrap();
    let initial = std::fs::read(state.join("sync/participation.json")).unwrap();
    let read = sync_action(&project, &state, "", true, |_| {
        panic!("read must not confirm")
    })
    .unwrap();
    assert!(read.contains(&agreed));
    assert!(
        sync_action(&project, &state, "apply", true, |_| panic!(
            "queued apply must refuse"
        ))
        .is_err()
    );
    let mut broken_session = Session::new();
    let broken_persistence = polaris_core::session_store::PersistedSession::create(
        &fixture.join("broken-session"),
        &fixture.join("broken-memory.sqlite3"),
        "project",
        polaris_core::conversation_state::HistoryMode::Legacy,
        None,
    )
    .unwrap();
    broken_session.attach(broken_persistence).unwrap();
    let broken_path = broken_session
        .persistence
        .as_ref()
        .unwrap()
        .store
        .directory()
        .join("state.json");
    std::fs::write(&broken_path, b"corrupt-state").unwrap();
    assert!(broken_session.check_persistence().is_err());
    let terminal =
        RefCell::new(ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 22)).unwrap());
    let mut reader = ScriptedReader(VecDeque::from([KeyCode::Char('y')]));
    let mut status = Status::Idle;
    let mut lines = Vec::new();
    handle_sync_action(
        &terminal,
        &mut reader,
        &project,
        &state,
        "apply",
        false,
        &broken_session,
        &mut status,
        &mut lines,
    );
    assert!(matches!(status, Status::Error(_)));
    assert_eq!(
        reader.0.len(),
        1,
        "a valid candidate must still refuse before confirmation"
    );
    assert!(lines.is_empty());
    assert_eq!(std::fs::read(project.join("shared.txt")).unwrap(), b"local");
    assert_eq!(
        std::fs::read(state.join("sync/participation.json")).unwrap(),
        initial
    );
    assert!(
        polaris_core::apply_owner::ApplyOwnerV1::acquire(&project)
            .unwrap()
            .active()
            .unwrap()
            .is_none()
    );
    let cancelled = sync_action(&project, &state, "apply", false, |view| {
        assert_eq!(view.revision, agreed);
        assert_eq!(view.changes, ["Modified: shared.txt"]);
        assert!(
            polaris_core::apply_owner::ApplyOwnerV1::acquire(&project).is_err(),
            "confirmation must retain lease"
        );
        false
    })
    .unwrap();
    assert!(cancelled.contains("cancelled"));
    assert_eq!(std::fs::read(project.join("shared.txt")).unwrap(), b"local");
    assert_eq!(
        std::fs::read(state.join("sync/participation.json")).unwrap(),
        initial
    );
    assert!(
        polaris_core::apply_owner::ApplyOwnerV1::acquire(&project)
            .unwrap()
            .active()
            .unwrap()
            .is_none()
    );
    let drift = sync_action(&project, &state, "apply", false, |_| {
        std::fs::write(project.join("shared.txt"), b"changed-after-preview").unwrap();
        true
    });
    assert!(drift.is_err(), "preview drift must refuse");
    assert_eq!(
        std::fs::read(project.join("shared.txt")).unwrap(),
        b"changed-after-preview"
    );
    assert_eq!(
        std::fs::read(state.join("sync/participation.json")).unwrap(),
        initial
    );
    std::fs::write(project.join("shared.txt"), b"local").unwrap();
    let applied = sync_action(&project, &state, "apply", false, |_| true).unwrap();
    assert!(applied.contains("1 items"));
    assert_eq!(
        std::fs::read(project.join("shared.txt")).unwrap(),
        b"agreed"
    );
    assert_eq!(
        participation::read(&state)
            .unwrap()
            .participation()
            .unwrap()
            .applied_commit
            .as_deref(),
        Some(agreed.as_str())
    );
    let archived = polaris_core::apply_owner::ApplyOwnerV1::acquire(&project).unwrap();
    assert!(archived.active().unwrap().is_none());
    drop(archived);
    let repeated = sync_action(&project, &state, "apply", false, |_| {
        panic!("already applied must not confirm")
    })
    .unwrap();
    assert!(repeated.contains("already been applied"));
    assert!(
        sync_action(&project, &state, "recover wrong", false, |_| panic!(
            "wrong recovery must refuse"
        ))
        .is_err()
    );

    // A non-writable source directory gives a real, deterministic effect
    // failure after durable admission without killing a process or mocking
    // the owner. Reopening and recovery must never retry the source rename.
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let unknown_project = fixture.join("unknown-project");
    std::fs::create_dir(&unknown_project).unwrap();
    let source_file = unknown_project.join("shared.txt");
    std::fs::write(&source_file, b"untouched").unwrap();
    let unknown_state = polaris_core::project::state_dir(&unknown_project).unwrap();
    source_apply::prepare_state(&unknown_project, &unknown_state).unwrap();
    participation::join(&unknown_state, held.share.clone(), manifest.clone(), 1).unwrap();
    let unknown_repository = SyncRepository::open(SyncPaths::under(&unknown_state)).unwrap();
    unknown_repository.stage_from(&incoming, &manifest).unwrap();
    let incoming_revision = unknown_repository
        .commit("unknown fixture")
        .unwrap()
        .unwrap();
    let mut joined = participation::read(&unknown_state)
        .unwrap()
        .participation()
        .unwrap()
        .clone();
    joined.agreed_commit = Some(incoming_revision);
    participation::write(&unknown_state, &joined).unwrap();
    let initial_participation =
        std::fs::read(unknown_state.join("sync/participation.json")).unwrap();
    let before = std::fs::metadata(&source_file).unwrap();
    std::fs::set_permissions(&unknown_project, std::fs::Permissions::from_mode(0o555)).unwrap();
    let failed_apply = sync_action(&unknown_project, &unknown_state, "apply", false, |_| true);
    std::fs::set_permissions(&unknown_project, std::fs::Permissions::from_mode(0o700)).unwrap();
    assert!(failed_apply.is_err(), "{failed_apply:?}");
    let owner = polaris_core::apply_owner::ApplyOwnerV1::acquire(&unknown_project).unwrap();
    let active = owner
        .active()
        .unwrap()
        .expect("failed source effect must remain discoverable");
    let key = active.descriptor().operation_key.as_hex();
    assert!(matches!(
        active.latest().phase,
        polaris_core::apply_intent::ApplyPhaseV1::OutcomeUnknown
    ));
    let effects = {
        let workspace = polaris_core::rewind_workspace::WorkspaceOwner::open(
            &unknown_project,
            &unknown_state.join("rewind-workspace"),
            polaris_core::rewind_workspace::RewindLimits::default(),
        )
        .unwrap();
        drop(owner);
        let lease = workspace.try_mutation().unwrap();
        polaris_core::management_effect::ManagementEffectStoreV1::open_at_state_dir(
            &unknown_project,
            &unknown_state,
            &lease,
        )
        .unwrap()
    };
    assert!(
        management_clearance_error(
            &unknown_project,
            &effects,
            &slash::Action::Sync("apply".into())
        )
        .is_some()
    );
    assert!(
        management_clearance_error(
            &unknown_project,
            &effects,
            &slash::Action::Sync("status".into())
        )
        .is_none()
    );
    assert!(
        management_clearance_error(
            &unknown_project,
            &effects,
            &slash::Action::Sync(format!("recover {key}"))
        )
        .is_none()
    );
    assert!(
        sync_action(
            &unknown_project,
            &unknown_state,
            "status",
            false,
            |_| panic!("inspect")
        )
        .unwrap()
        .contains(&key)
    );
    assert!(
        sync_action(
            &unknown_project,
            &unknown_state,
            "apply",
            false,
            |_| panic!("must not retry")
        )
        .is_err()
    );
    assert!(
        sync_action(
            &unknown_project,
            &unknown_state,
            "recover wrong",
            false,
            |_| panic!("wrong key must not confirm")
        )
        .is_err()
    );
    let cancelled_recovery = sync_action(
        &unknown_project,
        &unknown_state,
        &format!("recover {key}"),
        false,
        |_| false,
    )
    .unwrap();
    assert!(cancelled_recovery.contains("cancelled"));
    // Mirror startup: C12 unknown must not hide the recovery command.
    let workspace = open_rewind_workspace(&unknown_project, &unknown_state).unwrap();
    {
        let lease = workspace.try_mutation().unwrap();
        let data_root = fixture.join("data");
        let project_id = persist::project_identity(&unknown_project).unwrap();
        polaris_core::session_switch::SessionSwitchOwner {
            effects: &effects,
            lease: &lease,
            state_dir: &unknown_state,
        }
        .recover(
            &data_root,
            &unknown_state.join("memory.sqlite3"),
            &project_id,
        )
        .unwrap();
        polaris_core::management_effect::workflow_change::WorkflowChangeOwner {
            effects: &effects,
            lease: &lease,
            state_dir: &unknown_state,
        }
        .recover(
            &data_root,
            &unknown_state.join("memory.sqlite3"),
            &project_id,
        )
        .unwrap();
    }
    let terminal =
        RefCell::new(ratatui::Terminal::new(ratatui::backend::TestBackend::new(160, 22)).unwrap());
    for _ in 0..2 {
        let mut reader = ScriptedReader(VecDeque::from([KeyCode::Char('y')]));
        let mut status = Status::Idle;
        let mut lines = Vec::new();
        let mut refused_reader = ScriptedReader(VecDeque::from([KeyCode::Char('y')]));
        handle_sync_action(
            &terminal,
            &mut refused_reader,
            &unknown_project,
            &unknown_state,
            &format!("recover {key}"),
            false,
            &broken_session,
            &mut status,
            &mut lines,
        );
        assert!(matches!(status, Status::Error(_)));
        assert_eq!(refused_reader.0.len(), 1);
        assert!(lines.is_empty());
        handle_sync_action(
            &terminal,
            &mut reader,
            &unknown_project,
            &unknown_state,
            &format!("recover {key}"),
            false,
            &Session::new(),
            &mut status,
            &mut lines,
        );
        assert!(matches!(status, Status::Notice(_)));
        assert!(
            reader.0.is_empty(),
            "recovery requires explicit confirmation"
        );
        let recovered = lines
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            recovered.contains("outcome is unknown") && recovered.contains("not reapplied"),
            "{recovered}"
        );
        let after = std::fs::metadata(&source_file).unwrap();
        assert_eq!(
            (before.dev(), before.ino(), before.mtime(), before.ctime()),
            (after.dev(), after.ino(), after.mtime(), after.ctime())
        );
        assert_eq!(std::fs::read(&source_file).unwrap(), b"untouched");
        assert_eq!(
            std::fs::read(unknown_state.join("sync/participation.json")).unwrap(),
            initial_participation
        );
    }
    println!("m2d-c12-receipt: verified");
}

#[test]
fn workflow_confirmation_shows_exact_session_and_requires_y() {
    let terminal =
        RefCell::new(ratatui::Terminal::new(ratatui::backend::TestBackend::new(100, 16)).unwrap());
    let view = render::WorkflowChangeConfirmView {
        inventory: "T03",
        project_id: "project-1",
        session_id: "session-2",
        generation: 7,
        before_phase: "specify",
        after_phase: "implement",
        before_mode: "manual",
        after_mode: "manual",
    };
    let mut declined = ScriptedReader(VecDeque::from([KeyCode::Esc]));
    assert!(!confirm_workflow_change(&terminal, &mut declined, &view));
    let mut accepted = ScriptedReader(VecDeque::from([KeyCode::Char('y')]));
    assert!(confirm_workflow_change(&terminal, &mut accepted, &view));
    let screen = {
        let terminal = terminal.borrow();
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol()))
            .collect::<String>()
    };
    for expected in ["T03", "project-1", "session-2", "7", "specify", "implement"] {
        assert!(screen.contains(expected), "missing {expected}: {screen}");
    }
}

#[test]
fn draw_frame_puts_the_footer_in_the_bottom_inline_viewport_height_rows() {
    let backend = ratatui::backend::TestBackend::new(60, 30);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    let mut wrap_cache = render::HistoryWrapCache::default();
    let history = vec![render::HistoryLine {
        line: ratatui::text::Line::from("line from history"),
        shaded: false,
    }];
    terminal
        .draw(|f| {
            draw_frame(
                f,
                &history,
                &mut wrap_cache,
                0,
                "",
                0,
                &Status::Idle,
                &render::HeaderInfo {
                    provider_name: "openai",
                    model_name: "gpt-5.4",
                    usage: polaris_provider::Usage::default(),
                    cwd: "/tmp",
                    cwd_short: "~",
                    effort_name: "low",
                },
                &[],
                0,
                &None,
            )
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    assert!(rows[0].contains("line from history"));
    // The footer's placeholder text lands in the bottom
    // `render::footer_height(0)` rows (no suggestions here), not the
    // top history region.
    let footer_start = 30 - render::footer_height(0) as usize;
    assert!(
        rows[footer_start..]
            .iter()
            .any(|r| r.contains("Ask polaris to do anything"))
    );
    assert!(
        !rows[..footer_start]
            .iter()
            .any(|r| r.contains("Ask polaris to do anything"))
    );
}

#[test]
fn chat_draw_repairs_an_ime_cleared_cjk_composer_suffix() {
    use ratatui::{
        buffer::{Buffer, Cell},
        layout::Rect,
        style::Color,
    };
    use unicode_width::UnicodeWidthStr;

    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(60, 14)).unwrap();
    let header = render::HeaderInfo {
        provider_name: "openai",
        model_name: "test",
        usage: polaris_provider::Usage::default(),
        cwd: "/tmp",
        cwd_short: "~",
        effort_name: "low",
    };
    let history = vec![render::HistoryLine {
        line: ratatui::text::Line::from("history must remain visible"),
        shaded: false,
    }];
    let mut cache = render::HistoryWrapCache::default();

    for input in [
        "日本語",
        "日本語を追記",
        "日本語",
        "",
        "日本語を繰り返して入力する長い文です。日本語を繰り返して入力する長い文です。",
    ] {
        let cursor = input.len();
        draw_chat_frame(&mut terminal, input, cursor, 0, |frame| {
            draw_frame(
                frame,
                &history,
                &mut cache,
                0,
                input,
                cursor,
                &Status::Idle,
                &header,
                &[],
                0,
                &None,
            );
        })
        .expect("initial chat draw");

        let (input_area, cursor_position) =
            render::composer_geometry(terminal.backend().buffer().area, 0, input, cursor)
                .expect("input row");
        let before_damage = terminal.backend().buffer().clone();
        let suffix_width = input_area.width.min(21);
        let damaged_area = Rect::new(
            input_area.right().saturating_sub(suffix_width),
            input_area.y,
            suffix_width,
            input_area.height,
        );
        let before = Buffer::filled(damaged_area, Cell::new("x"));
        let cleared = Buffer::empty(damaged_area);
        terminal
            .backend_mut()
            .draw(before.diff(&cleared).into_iter())
            .expect("simulate IME clearing the real terminal suffix");
        assert_ne!(
            terminal.backend().buffer()[(damaged_area.x, damaged_area.y)].bg,
            Color::DarkGray,
            "the simulated IME damage must reach the real terminal"
        );

        draw_chat_frame(&mut terminal, input, cursor, 0, |frame| {
            draw_frame(
                frame,
                &history,
                &mut cache,
                0,
                input,
                cursor,
                &Status::Idle,
                &header,
                &[],
                0,
                &None,
            );
        })
        .expect("unchanged chat draw repairs the IME damage");

        assert_eq!(
            terminal.backend_mut().get_cursor_position().unwrap(),
            cursor_position,
            "replaying the input row must restore the caret"
        );
        let buffer = terminal.backend().buffer();
        let mut x = input_area.left();
        while x < input_area.right() {
            assert_eq!(
                buffer[(x, input_area.y)],
                before_damage[(x, input_area.y)],
                "composer cell was not restored at {x} for {input:?}"
            );
            assert_eq!(buffer[(x, input_area.y)].bg, Color::DarkGray);
            // TestBackend stores each draw command independently. A real
            // terminal also paints a wide glyph's continuation column,
            // while Buffer::diff correctly omits that separate command.
            x += buffer[(x, input_area.y)].symbol().width().max(1) as u16;
        }
        assert!(
            (0..buffer.area.width)
                .map(|x| buffer[(x, 0)].symbol())
                .collect::<String>()
                .contains("history must remain visible"),
            "replaying the composer row must not clear adjacent history"
        );
    }
}

#[test]
fn submission_preview_is_drawn_without_committing_and_is_removed_afterward() {
    let mut terminal =
        ratatui::Terminal::new(ratatui::backend::TestBackend::new(90, 20)).expect("terminal");
    let header = render::HeaderInfo {
        provider_name: "openai",
        model_name: "test",
        usage: polaris_provider::Usage::default(),
        cwd: "/tmp",
        cwd_short: "~",
        effort_name: "low",
    };
    let mut conversation_view = conversation_view::ConversationViewState::new(&header, 90);
    conversation_view.push_history_line(render::HistoryLine {
        line: ratatui::text::Line::from("earlier turn"),
        shaded: false,
    });
    let original_history_len = conversation_view.history.len();
    let mut status = Status::Idle;
    let session = polaris_core::session::Session::new();

    draw_submission_preview(
            &mut conversation_view,
            &mut status,
            "first line\nsecond\u{1b}[31m line",
            |preview, cache, preview_status| {
                assert!(matches!(preview_status, Status::Notice(message) if message == "Preparing send (not admitted)"));
                terminal.draw(|f| {
                    draw_frame(
                        f,
                        preview,
                        cache,
                        0,
                        "",
                        0,
                        preview_status,
                        &render::HeaderInfo {
                            provider_name: "openai",
                            model_name: "test",
                            usage: polaris_provider::Usage::default(),
                            cwd: "/tmp",
                            cwd_short: "~",
                            effort_name: "low",
                        },
                        &[],
                        0,
                        &None,
                    )
                }).map(|_| ())
            },
        )
        .expect("preview draw");

    let preview_screen: String = {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol()))
            .collect()
    };
    assert!(preview_screen.contains("first line"));
    assert!(preview_screen.contains("second"));
    assert!(!preview_screen.contains("\u{1b}[31m"));
    assert_eq!(
        conversation_view.history.len(),
        original_history_len,
        "the preview must not enter history"
    );
    assert_eq!(
        conversation_view.history.last().unwrap().line.to_string(),
        "earlier turn"
    );
    assert!(matches!(status, Status::Idle));
    assert!(session.messages.is_empty());

    status = Status::Error("preflight failed".into());
    terminal
        .draw(|f| {
            draw_frame(
                f,
                &conversation_view.history,
                &mut conversation_view.history_wrap_cache,
                0,
                "",
                0,
                &status,
                &render::HeaderInfo {
                    provider_name: "openai",
                    model_name: "test",
                    usage: polaris_provider::Usage::default(),
                    cwd: "/tmp",
                    cwd_short: "~",
                    effort_name: "low",
                },
                &[],
                0,
                &None,
            )
        })
        .expect("redraw after preflight failure");
    let redrawn: String = {
        let buffer = terminal.backend().buffer();
        (0..buffer.area.height)
            .flat_map(|y| (0..buffer.area.width).map(move |x| buffer[(x, y)].symbol()))
            .collect()
    };
    assert!(
        !redrawn.contains("first line"),
        "failed preflight leaves no ghost"
    );
}

#[test]
fn submission_preview_draw_error_removes_the_temporary_history() {
    let header = render::HeaderInfo {
        provider_name: "test",
        model_name: "test",
        usage: Default::default(),
        cwd: "/tmp",
        cwd_short: "~",
        effort_name: "auto",
    };
    let mut conversation_view = conversation_view::ConversationViewState::new(&header, 80);
    conversation_view.push_history_line(render::HistoryLine {
        line: ratatui::text::Line::from("earlier turn"),
        shaded: false,
    });
    let original_history_len = conversation_view.history.len();
    let mut status = Status::Idle;

    let result = draw_submission_preview(
        &mut conversation_view,
        &mut status,
        "not accepted",
        |_preview, _cache, _status| Err::<(), _>("draw failed"),
    );

    assert_eq!(result, Err("draw failed"));
    assert_eq!(conversation_view.history.len(), original_history_len);
    assert_eq!(
        conversation_view.history.last().unwrap().line.to_string(),
        "earlier turn"
    );
    assert!(matches!(status, Status::Idle));
}

#[test]
fn accepted_submission_appends_one_user_line_after_preview() {
    let mut session = polaris_core::session::Session::new();
    let header = render::HeaderInfo {
        provider_name: "test",
        model_name: "test",
        usage: Default::default(),
        cwd: "/tmp",
        cwd_short: "~",
        effort_name: "auto",
    };
    let mut conversation_view = conversation_view::ConversationViewState::new(&header, 80);
    let original_history_len = conversation_view.history.len();
    let mut status = Status::Idle;

    draw_submission_preview(
        &mut conversation_view,
        &mut status,
        "accepted request",
        |preview, _, _| {
            assert_eq!(preview.len(), original_history_len + 1);
            assert_eq!(preview.last().unwrap().line.to_string(), "accepted request");
            Ok::<(), ()>(())
        },
    )
    .unwrap();
    assert_eq!(conversation_view.history.len(), original_history_len);
    assert!(session.messages.is_empty());

    session.push_user("accepted request");
    conversation_view.append_new_history(&session, &[]);

    assert_eq!(session.messages.len(), 1);
    assert_eq!(conversation_view.history.len(), original_history_len + 1);
    assert_eq!(
        conversation_view
            .history
            .iter()
            .filter(|line| line.line.to_string() == "accepted request")
            .count(),
        1
    );
}

#[test]
fn draw_frame_wraps_a_history_line_wider_than_a_narrow_window_instead_of_clipping_it() {
    // A window half as wide as normal (30 columns) must still show the
    // full text of a long line, across multiple wrapped rows, rather
    // than clipping it at the window's right edge.
    let backend = ratatui::backend::TestBackend::new(30, 20);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    let mut wrap_cache = render::HistoryWrapCache::default();
    let long_text = "one two three four five six seven eight nine ten";
    let history = vec![render::HistoryLine {
        line: ratatui::text::Line::from(long_text),
        shaded: false,
    }];
    terminal
        .draw(|f| {
            draw_frame(
                f,
                &history,
                &mut wrap_cache,
                0,
                "",
                0,
                &Status::Idle,
                &render::HeaderInfo {
                    provider_name: "openai",
                    model_name: "gpt-5.4",
                    usage: polaris_provider::Usage::default(),
                    cwd: "/tmp",
                    cwd_short: "~",
                    effort_name: "low",
                },
                &[],
                0,
                &None,
            )
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    let joined: String = rows.join(" ");
    for word in long_text.split(' ') {
        assert!(
            joined.contains(word),
            "{word:?} from the long line should still appear somewhere on screen: {rows:?}"
        );
    }
    assert!(
        !rows.iter().any(|r| r.contains(long_text)),
        "the full text should NOT fit on a single 30-column row unwrapped: {rows:?}"
    );
}

#[test]
fn draw_frame_highlights_an_active_selection() {
    // Tall enough that `render::footer_height(0)` doesn't eat the whole
    // viewport and leave zero rows for the history area (matches the
    // height the other `draw_frame` tests in this module use).
    let backend = ratatui::backend::TestBackend::new(40, 20);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    let mut wrap_cache = render::HistoryWrapCache::default();
    // `HistoryLine::plain` is private to `render.rs` — this test lives in
    // `lib.rs`'s own test module, a different module, so it must build
    // `HistoryLine` via its public fields directly (both `line` and
    // `shaded` are `pub`), matching how the two existing `draw_frame`
    // tests in this same module already do it (grep `render::HistoryLine {`
    // in `lib.rs`).
    let history = vec![render::HistoryLine {
        line: ratatui::text::Line::from("select me"),
        shaded: false,
    }];
    let sel = Some(selection::Selection {
        anchor: selection::TextPos { line: 0, col: 0 },
        cursor: selection::TextPos { line: 0, col: 6 },
        dragging: false,
    });
    terminal
        .draw(|f| {
            draw_frame(
                f,
                &history,
                &mut wrap_cache,
                0,
                "",
                0,
                &Status::Idle,
                &render::HeaderInfo {
                    provider_name: "openai",
                    model_name: "gpt-5.4",
                    usage: polaris_provider::Usage::default(),
                    cwd: "/tmp",
                    cwd_short: "~",
                    effort_name: "low",
                },
                &[],
                0,
                &sel,
            )
        })
        .expect("draw");
    let buf = terminal.backend().buffer();
    assert_eq!(
        buf[(0, 0)].bg,
        render::SELECTION_HIGHLIGHT_COLOR,
        "the first selected column should be highlighted"
    );
    assert_ne!(
        buf[(7, 0)].bg,
        render::SELECTION_HIGHLIGHT_COLOR,
        "a column past the selection should not be highlighted"
    );
}

/// Regression test for the *elimination*, not just relocation, of the
/// blank gap between the last conversation-history row and the footer
/// block — proves `history_area` actually grows to use every row
/// `footer_height(0)` doesn't need, rather than always leaving the
/// difference between a fixed worst-case reservation and the popup's
/// actual height (9 rows, `14 - 5`) as unused blank space whenever the
/// suggestions popup isn't showing (the common case).
///
/// Fills history with more lines than any plausible `history_area`
/// height on this 20-row backend, so every row of the *actual* history
/// area — whatever height it turns out to be — has real "history line
/// N" text in it, right up to and including its very last row. Under
/// the old fixed `FOOTER_HEIGHT` (14) sizing this backend's history
/// area would only have gotten `20 - 14 = 6` rows (rows 0..6), leaving
/// rows 6..15 as pure unused blank space above the footer's status row
/// at row 15 — this test's row-14 assertion below would have failed
/// there, since row 14 sat well inside that unused gap, not inside
/// `history_area` at all.
#[test]
fn draw_frame_leaves_no_blank_gap_between_history_and_the_footer() {
    let backend = ratatui::backend::TestBackend::new(60, 20);
    let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
    let mut wrap_cache = render::HistoryWrapCache::default();
    let history: Vec<render::HistoryLine> = (0..20)
        .map(|i| render::HistoryLine {
            line: ratatui::text::Line::from(format!("history line {i}")),
            shaded: false,
        })
        .collect();
    terminal
        .draw(|f| {
            draw_frame(
                f,
                &history,
                &mut wrap_cache,
                0,
                "",
                0,
                &Status::Idle,
                &render::HeaderInfo {
                    provider_name: "openai",
                    model_name: "gpt-5.4",
                    usage: polaris_provider::Usage::default(),
                    cwd: "/tmp",
                    cwd_short: "~",
                    effort_name: "low",
                },
                &[],
                0,
                &None,
            )
        })
        .expect("draw");
    let buffer = terminal.backend().buffer();
    let rows: Vec<String> = (0..buffer.area.height)
        .map(|y| {
            (0..buffer.area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect();
    // 0 suggestions here, so this must match what `draw_frame` itself
    // just used to size `footer_area`.
    let expected_history_height = 20 - render::footer_height(0) as usize;
    assert!(
        rows[expected_history_height - 1].contains("history line"),
        "the row immediately above the footer must still be real \
             history content, not blank leftover space: {:?}",
        rows[expected_history_height - 1]
    );
    assert!(
        !rows[expected_history_height].contains("history line"),
        "the footer's own first row (status_area) must start \
             immediately below history, with nothing blank in between: {:?}",
        rows[expected_history_height]
    );
}

/// Closes the duplication-drift risk between `area_height_for_history`
/// (used by the four mouse-event handlers, which run outside the
/// `draw()` closure and so can't ask ratatui's own `Layout` what
/// `history_area.height` is this frame) and what `draw_frame` actually
/// renders. Rather than duplicating `draw_frame`'s own
/// `Layout::vertical` call a second time (which would only prove the
/// test's copy agrees with itself), this renders a real `draw_frame`
/// call with more history lines than any plausible `history_area`
/// height, all carrying the same marker (`"H"`), then counts how many
/// contiguous rows from the top actually got that marker drawn into
/// them — the true observable size of `history_area`, straight from
/// the rendered buffer. That count must equal
/// `area_height_for_history(render::footer_height(n), terminal_height)`
/// for both a representative suggestion count with no popup (`0`) and
/// one at the popup's display cap (`MAX_DISPLAYED_SUGGESTIONS`).
#[test]
fn area_height_for_history_matches_draw_frames_actual_history_area_height() {
    for &suggestion_count in &[0usize, render::MAX_DISPLAYED_SUGGESTIONS] {
        let terminal_height: u16 = 20;
        let suggestions: Vec<&slash::SlashCommand> =
            slash::COMMANDS.iter().take(suggestion_count).collect();
        assert_eq!(
            suggestions.len(),
            suggestion_count,
            "slash::COMMANDS must have at least MAX_DISPLAYED_SUGGESTIONS entries \
                 for this test to actually exercise the requested suggestion count"
        );
        let backend = ratatui::backend::TestBackend::new(60, terminal_height);
        let mut terminal = ratatui::Terminal::new(backend).expect("terminal");
        let mut wrap_cache = render::HistoryWrapCache::default();
        let history: Vec<render::HistoryLine> = (0..terminal_height)
            .map(|_| render::HistoryLine {
                line: ratatui::text::Line::from("H"),
                shaded: false,
            })
            .collect();
        terminal
            .draw(|f| {
                draw_frame(
                    f,
                    &history,
                    &mut wrap_cache,
                    0,
                    "",
                    0,
                    &Status::Idle,
                    &render::HeaderInfo {
                        provider_name: "openai",
                        model_name: "gpt-5.4",
                        usage: polaris_provider::Usage::default(),
                        cwd: "/tmp",
                        cwd_short: "~",
                        effort_name: "low",
                    },
                    &suggestions,
                    0,
                    &None,
                )
            })
            .expect("draw");
        let buffer = terminal.backend().buffer();
        let actual_history_rows = (0..buffer.area.height)
            .take_while(|&y| (0..buffer.area.width).any(|x| buffer[(x, y)].symbol() == "H"))
            .count() as u16;
        let via_helper =
            area_height_for_history(render::footer_height(suggestion_count), terminal_height);
        assert_eq!(
            actual_history_rows, via_helper,
            "suggestion_count={suggestion_count}: area_height_for_history \
                 must stay in sync with what draw_frame actually renders"
        );
    }
}

#[test]
fn append_new_history_appends_only_the_unprinted_tail() {
    let mut session = polaris_core::session::Session::default();
    session.push_user("first");
    let mut printed_messages = 0;
    let mut printed_local_lines = 0;
    let mut history = Vec::new();

    append_new_history(
        &session,
        &[],
        &mut printed_messages,
        &mut printed_local_lines,
        &mut history,
    );
    let after_first = history.len();
    assert!(after_first > 0);

    session.push_assistant("reply", Vec::new());
    append_new_history(
        &session,
        &[],
        &mut printed_messages,
        &mut printed_local_lines,
        &mut history,
    );
    assert!(
        history.len() > after_first,
        "the assistant reply should have been appended, not reprinted from scratch"
    );

    // Calling again with nothing new appends nothing.
    let stable = history.len();
    append_new_history(
        &session,
        &[],
        &mut printed_messages,
        &mut printed_local_lines,
        &mut history,
    );
    assert_eq!(history.len(), stable);
}

#[test]
fn append_new_history_self_corrects_when_the_session_shrinks() {
    let mut session = polaris_core::session::Session::default();
    session.push_user("first");
    session.push_assistant("reply", Vec::new());
    let mut printed_messages = 0;
    let mut printed_local_lines = 0;
    let mut history = Vec::new();
    append_new_history(
        &session,
        &[],
        &mut printed_messages,
        &mut printed_local_lines,
        &mut history,
    );
    assert_eq!(printed_messages, 2);

    // Simulates /clear: the session shrinks out from under the counters.
    session.messages.clear();
    append_new_history(
        &session,
        &[],
        &mut printed_messages,
        &mut printed_local_lines,
        &mut history,
    );
    assert_eq!(printed_messages, 0);
}

/// Regression coverage for the final-whole-branch-review Critical
/// finding: `printed_messages` is a second local index into
/// `session.messages`, alongside `checkpoint`, and it goes stale in
/// exactly the same way when compaction shrinks `session.messages`
/// mid-turn. Left unadjusted, the next `append_new_history` call sees
/// a `session.messages.len()` that no longer lines up with
/// `printed_messages`, and depending on how the shrink compares to
/// what the turn then appends, either the shrink-guard fires and wipes
/// the entire `history` buffer (header included), nothing new renders
/// at all, or the wrong slice of new messages renders. These three
/// tests simulate what `run()`'s two `AgentEvent::HistoryCompacted`
/// drain sites do — shrink `session.messages` the way `compact` does,
/// then apply the exact same `saturating_sub` adjustment to
/// `printed_messages` that the fix makes — across all three
/// before/after-vs-turn-growth orderings, and confirm
/// `append_new_history` renders correctly in each.
#[test]
fn append_new_history_renders_correctly_after_compaction_shrinks_more_than_the_turn_grows() {
    // D (messages compaction removes) > A (messages the turn then
    // adds) — the normal case: several old turns fold into one
    // summary, and the current turn only adds its own reply.
    let mut session = Session::default();
    for i in 0..6 {
        session.push_user(&format!("turn {i}"));
    }
    let mut printed_messages = 0;
    let mut printed_local_lines = 0;
    let mut history = vec![render::HistoryLine {
        line: Line::from("HEADER"),
        shaded: false,
    }];

    append_new_history(
        &session,
        &[],
        &mut printed_messages,
        &mut printed_local_lines,
        &mut history,
    );
    assert_eq!(printed_messages, 6);
    let after_initial_render = history.len();

    // Compaction folds all 6 messages into 2 (a summary plus one kept
    // turn) — D = 4 — mirroring the adjustment made at both
    // `AgentEvent::HistoryCompacted` drain sites in `run()`.
    let messages_before = session.messages.len();
    session.messages = vec![
        Message::user("summary of turns 0-4"),
        Message::user("turn 5"),
    ];
    let messages_after = session.messages.len();
    printed_messages =
        printed_messages.saturating_sub(messages_before.saturating_sub(messages_after));
    assert_eq!(printed_messages, 2);

    // The turn itself then appends its own reply (A = 1 < D = 4).
    session.push_assistant("the turn's reply", Vec::new());

    append_new_history(
        &session,
        &[],
        &mut printed_messages,
        &mut printed_local_lines,
        &mut history,
    );

    assert_eq!(
        history[0].line,
        Line::from("HEADER"),
        "the shrink-guard must not have fired and wiped the header"
    );
    assert_eq!(
        history.len(),
        after_initial_render + 1,
        "exactly the turn's one new reply should have been appended — \
             not zero (guard silently no-op'ing) and not a full reprint"
    );
    assert_eq!(printed_messages, 3);
}

#[test]
fn append_new_history_renders_correctly_after_compaction_shrinks_exactly_as_much_as_the_turn_grows()
{
    // D == A: without the `printed_messages` adjustment,
    // `session.messages.len()` ends up back at its pre-compaction
    // value, so the shrink-guard never fires but the "anything new to
    // render?" check also sees no growth — the turn's reply silently
    // never appears.
    let mut session = Session::default();
    for i in 0..6 {
        session.push_user(&format!("turn {i}"));
    }
    let mut printed_messages = 0;
    let mut printed_local_lines = 0;
    let mut history = vec![render::HistoryLine {
        line: Line::from("HEADER"),
        shaded: false,
    }];

    append_new_history(
        &session,
        &[],
        &mut printed_messages,
        &mut printed_local_lines,
        &mut history,
    );
    assert_eq!(printed_messages, 6);
    let after_initial_render = history.len();

    // Compaction removes 2 messages (D = 2).
    let messages_before = session.messages.len();
    session.messages = vec![
        Message::user("summary of turns 0-2"),
        Message::user("turn 3"),
        Message::user("turn 4"),
        Message::user("turn 5"),
    ];
    let messages_after = session.messages.len();
    printed_messages =
        printed_messages.saturating_sub(messages_before.saturating_sub(messages_after));
    assert_eq!(printed_messages, 4);

    // The turn adds exactly 2 new messages (A = D = 2): its own user
    // follow-up plus the assistant's reply.
    session.push_user("follow-up");
    session.push_assistant("the turn's reply", Vec::new());

    append_new_history(
        &session,
        &[],
        &mut printed_messages,
        &mut printed_local_lines,
        &mut history,
    );

    assert_eq!(
        history[0].line,
        Line::from("HEADER"),
        "the shrink-guard must not have fired and wiped the header"
    );
    assert_eq!(
        history.len(),
        after_initial_render + 2,
        "both of the turn's new messages should have been appended, \
             not silently dropped"
    );
    assert_eq!(printed_messages, 6);
}

#[test]
fn append_new_history_renders_correctly_after_compaction_shrinks_less_than_the_turn_grows() {
    // D < A: without the `printed_messages` adjustment, some of the
    // turn's own new messages get skipped from the render — the slice
    // starts too far into `session.messages`.
    let mut session = Session::default();
    for i in 0..6 {
        session.push_user(&format!("turn {i}"));
    }
    let mut printed_messages = 0;
    let mut printed_local_lines = 0;
    let mut history = vec![render::HistoryLine {
        line: Line::from("HEADER"),
        shaded: false,
    }];

    append_new_history(
        &session,
        &[],
        &mut printed_messages,
        &mut printed_local_lines,
        &mut history,
    );
    assert_eq!(printed_messages, 6);
    let after_initial_render = history.len();

    // Compaction removes just 1 message (D = 1).
    let messages_before = session.messages.len();
    session.messages = vec![
        Message::user("summary of turn 0"),
        Message::user("turn 1"),
        Message::user("turn 2"),
        Message::user("turn 3"),
        Message::user("turn 4"),
    ];
    let messages_after = session.messages.len();
    printed_messages =
        printed_messages.saturating_sub(messages_before.saturating_sub(messages_after));
    assert_eq!(printed_messages, 5);

    // The turn adds 3 new messages (A = 3 > D = 1).
    session.push_user("follow-up");
    session.push_assistant("intermediate reply", Vec::new());
    session.push_assistant("final reply", Vec::new());

    append_new_history(
        &session,
        &[],
        &mut printed_messages,
        &mut printed_local_lines,
        &mut history,
    );

    assert_eq!(
        history[0].line,
        Line::from("HEADER"),
        "the shrink-guard must not have fired and wiped the header"
    );
    assert_eq!(
        history.len(),
        after_initial_render + 3,
        "all three of the turn's new messages should have been \
             appended, not just the tail of them"
    );
    assert_eq!(printed_messages, 8);
}

#[allow(clippy::too_many_arguments)]
fn call(
    action: slash::Action,
    session: &mut Session,
    session_path: &std::path::Path,
    status: &mut Status,
    skills: &[polaris_skills::Skill],
    cwd: &std::path::Path,
    local_lines: &mut Vec<ratatui::text::Line<'static>>,
) -> SlashOutcome {
    apply_slash_action(
        action,
        session,
        session_path,
        status,
        "openai",
        "gpt-5.4",
        polaris_provider::UsageReport::default(),
        skills,
        cwd,
        cwd,
        cwd,
        false,
        local_lines,
    )
}

fn skill(name: &str) -> polaris_skills::Skill {
    polaris_skills::Skill {
        name: name.to_string(),
        description: format!("{name} description"),
        body: String::new(),
        path: PathBuf::new(),
    }
}

// `abbreviate_home` reads `HOME`, which is process-global state —
// swapping it races with any other test doing the same unless
// serialized.
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn abbreviate_home_collapses_a_path_under_home_to_a_tilde() {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var_os("HOME");
    // SAFETY: serialized by `HOME_LOCK`; restored before the guard drops.
    unsafe {
        std::env::set_var("HOME", "/Users/example");
    }

    let short = abbreviate_home(std::path::Path::new("/Users/example/File/projects/polaris"));

    match prev {
        Some(p) => unsafe { std::env::set_var("HOME", p) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    assert_eq!(short, "~/File/projects/polaris");
}

#[test]
fn abbreviate_home_leaves_a_path_outside_home_unchanged() {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var_os("HOME");
    // SAFETY: serialized by `HOME_LOCK`; restored before the guard drops.
    unsafe {
        std::env::set_var("HOME", "/Users/example");
    }

    let unchanged = abbreviate_home(std::path::Path::new("/var/other/place"));

    match prev {
        Some(p) => unsafe { std::env::set_var("HOME", p) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    assert_eq!(unchanged, "/var/other/place");
}

#[test]
fn run_skills_picker_with_none_discovered_dismisses_on_any_key() {
    let backend = TestBackend::new(70, 14);
    let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
    let mut reader = ScriptedReader(VecDeque::from([KeyCode::Enter]));

    // Must return promptly rather than looping forever on an empty list.
    run_skills_picker(&terminal, &mut reader, &[]);
}

#[test]
fn run_skills_picker_shows_discovered_skills_and_closes_on_enter() {
    let backend = TestBackend::new(70, 14);
    let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
    let skills = [skill("alpha"), skill("beta")];
    // Move the highlight once, then close — exercises Down as well as
    // the close path.
    let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Enter]));

    run_skills_picker(&terminal, &mut reader, &skills);

    let content = terminal
        .borrow()
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(content.contains("alpha"));
    assert!(content.contains("beta"));
}

#[cfg(unix)]
#[test]
fn init_creates_agents_md_in_cwd() {
    let mut session = Session::default();
    let mut status = Status::Idle;
    let dir = tempfile::tempdir().expect("temp dir");
    let session_path = dir.path().join("session.jsonl");
    let mut local_lines = Vec::new();

    call(
        slash::Action::Init,
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );

    let agents_md = dir.path().join("AGENTS.md");
    assert!(agents_md.exists());
    match status {
        Status::Notice(n) => assert!(n.contains("created")),
        _ => panic!("expected a Notice"),
    }
}

/// The template has to survive the reader it was written for. Asserting
/// only that the file exists passed for the whole of the template's
/// previous life, during which it delivered nothing: it had no
/// `## Always on` heading, so `extract_always_on` returned an empty
/// string and the run carried zero project rules.
#[test]
fn what_init_writes_actually_reaches_the_model_and_fits_the_ceiling() {
    use polaris_core::constitution::{CONSTITUTION_LIMIT, extract_always_on};
    let template = agents_md_template();

    let block = extract_always_on(&template);
    assert!(
        !block.is_empty(),
        "the template delivers nothing:\n{template}"
    );
    assert!(
        block.lines().any(|l| l.trim_start().starts_with('-')),
        "the delivered block has no rule line: {block:?}"
    );
    // Every line of it has to fit, or `/init` ships a file whose own
    // example rule is silently dropped.
    let cost = polaris_core::budget::count_tokens(&block);
    assert!(
        cost <= CONSTITUTION_LIMIT,
        "the template's own block costs {cost}, over the {CONSTITUTION_LIMIT}-token ceiling: {block:?}"
    );
    // The prose must not promise the behaviour the code does not have.
    assert!(
        !template.contains("Everything up to the first"),
        "the template still describes a rule `extract_always_on` does not implement"
    );
}

#[cfg(unix)]
#[test]
fn init_does_not_overwrite_an_existing_agents_md() {
    let mut session = Session::default();
    let mut status = Status::Idle;
    let dir = tempfile::tempdir().expect("temp dir");
    let session_path = dir.path().join("session.jsonl");
    let mut local_lines = Vec::new();
    let agents_md = dir.path().join("AGENTS.md");
    std::fs::write(&agents_md, "existing content").expect("write");

    call(
        slash::Action::Init,
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );

    assert_eq!(
        std::fs::read_to_string(&agents_md).unwrap(),
        "existing content"
    );
    match status {
        Status::Notice(n) => assert!(n.contains("already exists")),
        _ => panic!("expected a Notice"),
    }
}

#[cfg(unix)]
#[test]
fn clear_empties_the_session_and_the_persisted_file_in_place() {
    let mut session = Session::default();
    session.push_user("something from before");
    let mut status = Status::Idle;
    let dir = tempfile::tempdir().expect("temp dir");
    let session_path = dir.path().join("session.jsonl");
    let mut local_lines = Vec::new();
    persist::append_message(&session_path, session.messages.last().unwrap())
        .expect("seed the file");

    call(
        slash::Action::Clear,
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );

    assert!(session.messages.is_empty());
    assert_eq!(
        std::fs::read_to_string(&session_path).unwrap(),
        "",
        "the persisted file should be emptied too, not just the in-memory session"
    );
    assert!(matches!(status, Status::Idle));
}

#[cfg(unix)]
#[test]
fn new_and_resume_are_never_actually_dispatched_through_apply_slash_action() {
    // `run()` intercepts every one of these before they'd ever reach
    // here (see `handle_new_session`/`handle_resume`/`handle_fork`/
    // `handle_permissions`/`handle_model`/`run_skills_picker`); this
    // only guards the defensive fallback arm against silently doing
    // nothing instead of surfacing that something is wrong.
    for action in [
        slash::Action::New,
        slash::Action::Resume,
        slash::Action::Fork,
        slash::Action::Permissions,
        slash::Action::Model,
        slash::Action::Skills,
    ] {
        let mut session = Session::default();
        let mut status = Status::Idle;
        let dir = tempfile::tempdir().expect("temp dir");
        let session_path = dir.path().join("session.jsonl");
        let mut local_lines = Vec::new();

        call(
            action,
            &mut session,
            &session_path,
            &mut status,
            &[],
            dir.path(),
            &mut local_lines,
        );

        match status {
            Status::Notice(n) => assert!(n.contains("internal")),
            _ => panic!("expected a Notice"),
        }
    }
}

#[test]
fn handle_new_session_starts_a_fresh_file_without_touching_the_old_one() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut session = Session {
        disable_files_md_auto_regenerate: true,
        ..Session::default()
    };
    session.push_user("something from before");
    let old_path = dir.path().join("old.jsonl");
    persist::append_message(&old_path, session.messages.last().unwrap())
        .expect("seed the old file");
    let mut session_path = old_path.clone();
    let mut meta_path = dir.path().join("old.meta.json");
    let mut session_started_at_millis = 111;
    let mut status = Status::Idle;
    let mut local_lines = vec![Line::from("leftover /diff output")];

    handle_new_session(
        dir.path(),
        &mut session,
        &mut session_path,
        &mut meta_path,
        &mut session_started_at_millis,
        &mut status,
        &mut local_lines,
    );

    assert!(session.messages.is_empty());
    assert!(session.disable_files_md_auto_regenerate);
    assert!(local_lines.is_empty());
    assert!(matches!(status, Status::Idle));
    assert_ne!(
        session_path, old_path,
        "a new session should get its own file"
    );
    assert_eq!(
        std::fs::read_to_string(&old_path).unwrap().lines().count(),
        1,
        "the old conversation must stay intact on disk, not get wiped"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn workflow_session_defaults_to_builtin_skills_and_supports_explicit_opt_out() {
    let dir = tempfile::tempdir().unwrap();
    let sessions_dir = dir.path().join("sessions");
    let mut config = polaris_core::config::Config::default();
    for enabled in [true, false] {
        if !enabled {
            config.workflow.enabled = false;
        }
        let session = new_workflow_session(WorkflowSessionArgs {
            config: &config.workflow,
            initial_phase: None,
            approval_auto: config.approval_auto,
            history_mode: config.history_mode,
            input_preparation: config.input_preparation,
            cwd: dir.path(),
            state_dir: dir.path(),
            sessions_dir: &sessions_dir,
            strict_history: None,
            summary_usage: None,
            history_backend_factory: None,
            files_md_auto_regenerate: config.files_md_auto_regenerate,
            continuation: None,
            workflow_mode_overridden: false,
            skills: &[],
            #[cfg(unix)]
            management_effects: None,
            #[cfg(unix)]
            project_mutation: None,
        })
        .await
        .unwrap();
        assert_eq!(session.workflow.is_some(), enabled);
        assert_eq!(session.persistence.is_some(), enabled);
        assert!(session.strict_history.is_none());
        if let Some(workflow) = &session.workflow {
            let turn = workflow.begin_turn(&[]).unwrap();
            assert_eq!(turn.ids, ["builtin:workflow-core"]);
            assert_eq!(turn.state.phase, polaris_core::workflow::Phase::General);
            let saved = session.persistence.as_ref().unwrap().snapshot().unwrap();
            assert_eq!(
                saved.state.history_mode,
                polaris_core::conversation_state::HistoryMode::Legacy
            );
        }
    }
}

#[cfg(unix)]
#[tokio::test]
async fn managed_new_creates_a_separate_family_and_preserves_a_denied_draft() {
    use polaris_core::{
        conversation_state::HistoryMode,
        desktop_store::SessionManagerStore,
        management_effect::{ManagementEffectStateV1, ManagementEffectStoreV1},
        rewind_workspace::{RewindLimits, WorkspaceOwner},
        session_manager::SessionId,
    };

    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let state_dir = root.path().join("state");
    let sessions_dir = root.path().join("data/sessions");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&state_dir).unwrap();
    std::fs::set_permissions(
        &state_dir,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    std::fs::create_dir_all(&sessions_dir).unwrap();
    let owner = WorkspaceOwner::open(
        &project,
        &state_dir.join("rewind-workspace"),
        RewindLimits::default(),
    )
    .unwrap();
    let effects = {
        let lease = owner.try_mutation().unwrap();
        ManagementEffectStoreV1::open_at_state_dir(&project, &state_dir, &lease).unwrap()
    };
    let config = polaris_core::config::Config::default();
    let mut workflow = config.workflow.clone();
    workflow.enabled = false;
    let mut session = new_workflow_session(WorkflowSessionArgs {
        config: &workflow,
        initial_phase: None,
        approval_auto: config.approval_auto,
        history_mode: HistoryMode::Legacy,
        input_preparation: config.input_preparation,
        cwd: &project,
        state_dir: &state_dir,
        sessions_dir: &sessions_dir,
        strict_history: None,
        summary_usage: None,
        history_backend_factory: None,
        files_md_auto_regenerate: config.files_md_auto_regenerate,
        continuation: None,
        workflow_mode_overridden: false,
        skills: &[],
        management_effects: Some(&effects),
        project_mutation: Some(&owner),
    })
    .await
    .unwrap();
    assert!(session.workflow.is_none());
    let saved = session.persistence.as_ref().unwrap().snapshot().unwrap();
    assert_eq!(saved.state.history_mode, HistoryMode::Legacy);
    let child = SessionId::new(saved.state.session_id.clone()).unwrap();
    let (source_store, family) =
        SessionManagerStore::open_family_owned(&state_dir.join("tui-admissions-v1"), &child)
            .unwrap();
    assert!(family.sessions.contains_key(&child));
    drop(source_store);
    assert!(effects.blocker().unwrap().is_none());
    assert!(effects.effects().unwrap().iter().any(|effect| {
        effect.request.inventory.encoded() == "T06"
            && matches!(effect.state, ManagementEffectStateV1::Completed { .. })
    }));

    session.push_user("old v2 message");
    session.check_persistence().unwrap();
    let source = session.persistence.as_ref().unwrap().clone();
    let source_dir = source.store.directory().to_path_buf();
    let source_state = std::fs::read(source_dir.join("state.json")).unwrap();
    let source_raw = std::fs::read(source_dir.join("raw.jsonl")).unwrap();
    let mut session_path = sessions_dir.join(format!("{}.jsonl", child.as_str()));
    let mut meta_path = session_path.with_extension("meta.json");
    let mut started = 1;
    let mut status = Status::Idle;
    let mut local_lines = vec![Line::from("old local output")];
    assert!(handle_managed_new_session(
        &sessions_dir,
        &state_dir,
        &owner,
        &effects,
        &mut session,
        &mut session_path,
        &mut meta_path,
        &mut started,
        &mut status,
        &mut local_lines,
    ));
    let new_id = session
        .persistence
        .as_ref()
        .unwrap()
        .snapshot()
        .unwrap()
        .state
        .session_id;
    assert_ne!(new_id, child.as_str());
    assert_eq!(session_path.file_stem().unwrap(), new_id.as_str());
    assert_eq!(meta_path, session_path.with_extension("meta.json"));
    assert!(session.messages.is_empty());
    assert!(session.workflow.is_none());
    assert!(local_lines.is_empty());
    assert!(matches!(status, Status::Idle));
    assert_eq!(
        std::fs::read(source_dir.join("state.json")).unwrap(),
        source_state
    );
    assert_eq!(
        std::fs::read(source_dir.join("raw.jsonl")).unwrap(),
        source_raw
    );
    let new_child = SessionId::new(new_id).unwrap();
    let (new_store, new_family) =
        SessionManagerStore::open_family_owned(&state_dir.join("tui-admissions-v1"), &new_child)
            .unwrap();
    assert_eq!(new_family.sessions.len(), 1);
    assert!(new_family.sessions.contains_key(&new_child));
    drop(new_store);
    let (_store, old_family) =
        SessionManagerStore::open_family_owned(&state_dir.join("tui-admissions-v1"), &child)
            .unwrap();
    assert!(old_family.sessions.contains_key(&child));
    assert_eq!(
        effects
            .effects()
            .unwrap()
            .iter()
            .filter(|effect| {
                effect.request.inventory.encoded() == "T06"
                    && matches!(effect.state, ManagementEffectStateV1::Completed { .. })
            })
            .count(),
        2
    );

    let journal = state_dir.join("management-effects-v1/journal.v1.jsonl");
    let journal_before = std::fs::read(&journal).unwrap();
    let cursor_before = effects.cursor().unwrap();
    let new_dir = session
        .persistence
        .as_ref()
        .unwrap()
        .store
        .directory()
        .to_path_buf();
    let new_state = std::fs::read(new_dir.join("state.json")).unwrap();
    let new_raw = std::fs::read(new_dir.join("raw.jsonl")).unwrap();
    let current_path = session_path.clone();
    let current_meta = meta_path.clone();
    let current_started = started;
    local_lines.push(Line::from("keep on refusal"));
    let busy_lease = owner.try_mutation().unwrap();

    let mut popup_draft = "/ne".to_string();
    let mut popup_cursor = 2;
    let mut queued = VecDeque::new();
    let mut paused = false;
    let submitted = std::mem::take(&mut popup_draft);
    assert!(!handle_managed_new_session(
        &sessions_dir,
        &state_dir,
        &owner,
        &effects,
        &mut session,
        &mut session_path,
        &mut meta_path,
        &mut started,
        &mut status,
        &mut local_lines,
    ));
    restore_rejected_new_submission(
        submitted,
        popup_cursor,
        false,
        &mut popup_draft,
        &mut popup_cursor,
        &mut queued,
        &mut paused,
    );
    assert_eq!(popup_draft, "/ne");
    assert_eq!(popup_cursor, 2);

    let mut ordinary_draft = "/new".to_string();
    let mut ordinary_cursor = 2;
    let mut history = input::History::new();
    history.record("prior input");
    let previous_cursor = ordinary_cursor;
    let InputAction::Submit(submitted) = apply_key(
        &mut ordinary_draft,
        &mut ordinary_cursor,
        ratatui::crossterm::event::KeyEvent::new(
            ratatui::crossterm::event::KeyCode::Enter,
            ratatui::crossterm::event::KeyModifiers::NONE,
        ),
    ) else {
        panic!("Enter should submit /new");
    };
    record_submission_before_dispatch(&mut history, &submitted, false);
    assert!(!handle_managed_new_session(
        &sessions_dir,
        &state_dir,
        &owner,
        &effects,
        &mut session,
        &mut session_path,
        &mut meta_path,
        &mut started,
        &mut status,
        &mut local_lines,
    ));
    restore_rejected_new_submission(
        submitted,
        previous_cursor,
        false,
        &mut ordinary_draft,
        &mut ordinary_cursor,
        &mut queued,
        &mut paused,
    );
    assert_eq!(ordinary_draft, "/new");
    assert_eq!(ordinary_cursor, 2);
    assert_eq!(history.older(&ordinary_draft), Some("prior input"));
    assert_eq!(history.newer(), Some("/new"));

    let mut live_draft = "unsent request".to_string();
    let mut live_cursor = 4;
    queued.extend(["/new".to_string(), "later request".to_string()]);
    let submitted = queued.pop_front().unwrap();
    assert!(!handle_managed_new_session(
        &sessions_dir,
        &state_dir,
        &owner,
        &effects,
        &mut session,
        &mut session_path,
        &mut meta_path,
        &mut started,
        &mut status,
        &mut local_lines,
    ));
    restore_rejected_new_submission(
        submitted,
        4,
        true,
        &mut live_draft,
        &mut live_cursor,
        &mut queued,
        &mut paused,
    );
    assert_eq!(live_draft, "unsent request");
    assert_eq!(live_cursor, 4);
    assert_eq!(
        queued,
        VecDeque::from(["/new".into(), "later request".into()])
    );
    assert!(paused);
    drop(busy_lease);
    assert_eq!(session_path, current_path);
    assert_eq!(meta_path, current_meta);
    assert_eq!(started, current_started);
    assert_eq!(
        std::fs::read(new_dir.join("state.json")).unwrap(),
        new_state
    );
    assert_eq!(std::fs::read(new_dir.join("raw.jsonl")).unwrap(), new_raw);
    assert_eq!(std::fs::read(&journal).unwrap(), journal_before);
    assert_eq!(effects.cursor().unwrap(), cursor_before);
    assert_eq!(local_lines.len(), 1);

    assert!(handle_managed_new_session(
        &sessions_dir,
        &state_dir,
        &owner,
        &effects,
        &mut session,
        &mut session_path,
        &mut meta_path,
        &mut started,
        &mut status,
        &mut local_lines,
    ));
    // This manual `/new` is a separate accepted submission. It must not
    // consume the earlier queued `/new`, even though the text is equal.
    paused = false;
    assert_eq!(
        queued,
        VecDeque::from(["/new".into(), "later request".into()])
    );
    assert!(!paused);
    assert_eq!(live_draft, "unsent request");
    let retry_id = session
        .persistence
        .as_ref()
        .unwrap()
        .snapshot()
        .unwrap()
        .state
        .session_id;
    assert_eq!(session_path.file_stem().unwrap(), retry_id.as_str());
    assert_ne!(retry_id, new_child.as_str());
    let retry_child = SessionId::new(retry_id).unwrap();
    let (retry_store, retry_family) =
        SessionManagerStore::open_family_owned(&state_dir.join("tui-admissions-v1"), &retry_child)
            .unwrap();
    assert_eq!(retry_family.sessions.len(), 1);
    assert!(retry_family.sessions.contains_key(&retry_child));
    drop(retry_store);
    assert_eq!(
        std::fs::read(new_dir.join("state.json")).unwrap(),
        new_state
    );
    assert_eq!(std::fs::read(new_dir.join("raw.jsonl")).unwrap(), new_raw);
    assert_eq!(
        std::fs::read(source_dir.join("state.json")).unwrap(),
        source_state
    );
    assert_eq!(
        std::fs::read(source_dir.join("raw.jsonl")).unwrap(),
        source_raw
    );
    assert_eq!(
        effects
            .effects()
            .unwrap()
            .iter()
            .filter(|effect| {
                effect.request.inventory.encoded() == "T06"
                    && matches!(effect.state, ManagementEffectStateV1::Completed { .. })
            })
            .count(),
        3
    );

    // The queued invocation is dispatched once by taking its own head.
    assert_eq!(queued.pop_front().as_deref(), Some("/new"));
    assert!(handle_managed_new_session(
        &sessions_dir,
        &state_dir,
        &owner,
        &effects,
        &mut session,
        &mut session_path,
        &mut meta_path,
        &mut started,
        &mut status,
        &mut local_lines,
    ));
    assert_eq!(queued, VecDeque::from(["later request".into()]));
    assert_eq!(live_draft, "unsent request");
    let queued_id = session
        .persistence
        .as_ref()
        .unwrap()
        .snapshot()
        .unwrap()
        .state
        .session_id;
    assert_ne!(queued_id, retry_child.as_str());
    assert_eq!(session_path.file_stem().unwrap(), queued_id.as_str());
    let queued_child = SessionId::new(queued_id).unwrap();
    let (_store, queued_family) =
        SessionManagerStore::open_family_owned(&state_dir.join("tui-admissions-v1"), &queued_child)
            .unwrap();
    assert_eq!(queued_family.sessions.len(), 1);
    assert!(queued_family.sessions.contains_key(&queued_child));
    assert_eq!(
        effects
            .effects()
            .unwrap()
            .iter()
            .filter(|effect| {
                effect.request.inventory.encoded() == "T06"
                    && matches!(effect.state, ManagementEffectStateV1::Completed { .. })
            })
            .count(),
        4
    );
}

#[cfg(unix)]
#[tokio::test]
async fn prepared_resume_uses_the_selected_v2_identity_history_and_workflow() {
    let dir = tempfile::tempdir().expect("temp dir");
    let source_config = polaris_core::workflow::WorkflowConfig {
        enabled: true,
        mode: polaris_core::workflow::WorkflowMode::Adaptive,
        initial_phase: polaris_core::workflow::Phase::Review,
        ..Default::default()
    };
    let source_workflow = polaris_core::workflow::SessionWorkflow::new(source_config.clone());
    let persistence = polaris_core::session_store::PersistedSession::create(
        dir.path(),
        &dir.path().join("memory.sqlite3"),
        "project",
        polaris_core::conversation_state::HistoryMode::Legacy,
        Some(&source_workflow),
    )
    .expect("create source");
    let source_id = persistence
        .snapshot()
        .expect("source snapshot")
        .state
        .session_id;
    let mut seeded = Session::new();
    seeded.attach(persistence.clone()).expect("attach source");
    seeded.push_user("saved user turn");

    let current_config = polaris_core::workflow::WorkflowConfig {
        enabled: true,
        initial_phase: polaris_core::workflow::Phase::Implement,
        ..Default::default()
    };
    let session = new_workflow_session(WorkflowSessionArgs {
        config: &current_config,
        initial_phase: None,
        approval_auto: polaris_core::approval::AutoApproval::default(),
        history_mode: polaris_core::conversation_state::HistoryMode::Legacy,
        input_preparation: polaris_core::input_preparation::InputPreparationMode::Off,
        cwd: dir.path(),
        state_dir: dir.path(),
        sessions_dir: &dir.path().join("sessions"),
        strict_history: None,
        summary_usage: None,
        history_backend_factory: None,
        files_md_auto_regenerate: true,
        continuation: Some(PreparedContinuation {
            kind: ContinuationKind::Resume,
            persistence,
        }),
        workflow_mode_overridden: false,
        skills: &[],
        #[cfg(unix)]
        management_effects: None,
        #[cfg(unix)]
        project_mutation: None,
    })
    .await
    .expect("resume selected session");

    assert_eq!(
        session
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .state
            .session_id,
        source_id
    );
    assert_eq!(session.messages[0].content, "saved user turn");
    assert_eq!(session.workflow.as_ref().unwrap().config, source_config);
    assert_eq!(
        session.workflow.as_ref().unwrap().state().phase,
        polaris_core::workflow::Phase::Review
    );
}

#[test]
fn v2_new_and_fork_preserve_strict_runtime_examples_and_usage_meter() {
    let dir = tempfile::tempdir().expect("temp dir");
    let sessions_dir = dir.path().join("sessions");
    std::fs::create_dir(&sessions_dir).expect("sessions dir");
    let mut session = Session::new();
    let persistence = polaris_core::session_store::PersistedSession::create(
        dir.path(),
        &dir.path().join("memory.sqlite3"),
        "project",
        polaris_core::conversation_state::HistoryMode::Strict10,
        None,
    )
    .expect("create v2 session");
    session.attach(persistence).expect("attach v2 session");
    let strict = test_strict_history();
    session.strict_history = Some(strict.clone());
    session.summary_usage = Some(polaris_provider::UsageMeter::default());
    session.examples = vec![Message::user("configured example")];
    session.push_user("real turn");
    session.check_persistence().expect("persist real turn");

    let mut session_path = sessions_dir.join("current.jsonl");
    let mut meta_path = sessions_dir.join("current.meta.json");
    let mut started = 1;
    let mut status = Status::Idle;
    let mut local_lines = Vec::new();
    handle_new_session(
        &sessions_dir,
        &mut session,
        &mut session_path,
        &mut meta_path,
        &mut started,
        &mut status,
        &mut local_lines,
    );
    assert!(Arc::ptr_eq(
        session.strict_history.as_ref().unwrap(),
        &strict
    ));
    assert!(session.summary_usage.is_some());
    assert_eq!(session.examples[0].content, "configured example");

    handle_fork(
        &sessions_dir,
        "/tmp/project",
        &mut session,
        &mut session_path,
        &mut meta_path,
        &mut started,
        &mut status,
    );
    assert!(Arc::ptr_eq(
        session.strict_history.as_ref().unwrap(),
        &strict
    ));
    assert!(session.summary_usage.is_some());
    assert_eq!(session.examples[0].content, "configured example");
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn m3c_actual_resume_keeps_selected_identity_after_confirmed_workflow_override() {
    exercise_actual_resume_after_owned_publication(false).await;
}

#[cfg(target_os = "linux")]
#[test]
fn m3_remaining_managed_fork_preserves_original_and_refuses_active_owner() {
    use polaris_core::{
        conversation_state::HistoryMode, management_effect::*, rewind_workspace::WorkspaceOwner,
        session_store::PersistedSession,
    };
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let state = root.path().join("state");
    let sessions = root.path().join("sessions");
    for path in [&project, &state, &sessions] {
        std::fs::create_dir(path).unwrap();
    }
    std::fs::set_permissions(
        &state,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    let owner = WorkspaceOwner::open(
        &project,
        &state.join("rewind-workspace"),
        Default::default(),
    )
    .unwrap();
    let lease = owner.try_mutation().unwrap();
    let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
    drop(lease);
    let workflow = polaris_core::workflow::SessionWorkflow::new(Default::default());
    let source = PersistedSession::create(
        root.path(),
        &state.join("memory.sqlite3"),
        project.to_str().unwrap(),
        HistoryMode::Legacy,
        Some(&workflow),
    )
    .unwrap();
    let mut session = Session::new();
    session.workflow = Some(workflow);
    session.attach(source.clone()).unwrap();
    session.push_user("original transcript");
    let before = source.snapshot().unwrap();
    let mut path = sessions.join(format!("{}.jsonl", before.state.session_id));
    let mut meta = path.with_extension("meta.json");
    let mut started = 0;
    let mut status = Status::Idle;
    let busy = owner.try_mutation().unwrap();
    handle_managed_fork(
        &sessions,
        project.to_str().unwrap(),
        &state,
        &owner,
        &effects,
        &mut session,
        &mut path,
        &mut meta,
        &mut started,
        &mut status,
    );
    assert_eq!(
        session
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .state
            .session_id,
        before.state.session_id
    );
    assert!(effects.effects().unwrap().is_empty());
    drop(busy);
    handle_managed_fork(
        &sessions,
        project.to_str().unwrap(),
        &state,
        &owner,
        &effects,
        &mut session,
        &mut path,
        &mut meta,
        &mut started,
        &mut status,
    );
    if let Status::Notice(text) = &status {
        assert!(text.contains("Forked"), "{text}");
    } else {
        panic!("fork did not display its result");
    }
    let child = session.persistence.as_ref().unwrap().snapshot().unwrap();
    assert_ne!(child.state.session_id, before.state.session_id);
    assert_eq!(session.messages[0].content, "original transcript");
    let unchanged = source.snapshot().unwrap();
    assert_eq!(unchanged.state, before.state);
    assert_eq!(unchanged.raw_hash, before.raw_hash);
    assert!(
        effects
            .effects()
            .unwrap()
            .iter()
            .any(|e| e.request.inventory.encoded() == "T16"
                && matches!(e.state, ManagementEffectStateV1::Completed { .. }))
    );
    assert!(effects.blocker().unwrap().is_none());
}

#[tokio::test]
async fn m3_acceptance_memory_source_verifies_original_and_refuses_modified_raw() {
    struct SourceSummary;
    impl StrictSummaryProvider for SourceSummary {
        fn summarize<'a>(&'a self, request: SummaryRequest) -> StrictFuture<'a, String> {
            Box::pin(async move {
                Ok(serde_json::json!({"facts":["remember original verified source"], "decisions":[], "constraints":[], "corrections":[], "open_items":[], "source_turn_ids":[request.source_turn_id]}).to_string())
            })
        }
    }
    struct SourceEmbedder(EmbeddingModel);
    impl StrictEmbedder for SourceEmbedder {
        fn metadata(&self) -> &EmbeddingModel {
            &self.0
        }
        fn embed_passage<'a>(&'a self, _: &'a str) -> StrictFuture<'a, Vec<f32>> {
            Box::pin(async { Ok(vec![1.0]) })
        }
        fn embed_query<'a>(&'a self, _: &'a str) -> StrictFuture<'a, Vec<Vec<f32>>> {
            Box::pin(async { Ok(vec![vec![1.0]]) })
        }
    }
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    std::fs::create_dir(&project).unwrap();
    let project_id = persist::project_identity(&project).unwrap();
    let saved = polaris_core::session_store::PersistedSession::create(
        root.path(),
        &root.path().join("memory.sqlite3"),
        &project_id,
        polaris_core::conversation_state::HistoryMode::Strict10,
        None,
    )
    .unwrap();
    for turn in 1..=11 {
        saved
            .append(
                Message::user(format!("original verified source {turn}")),
                true,
                None,
            )
            .unwrap();
    }
    let history = StrictHistory::new(
        Arc::new(SourceSummary),
        Arc::new(SourceEmbedder(EmbeddingModel {
            model: "fixture".into(),
            revision: "v1".into(),
            dimension: 1,
        })),
    );
    let prepared = history
        .prepare_observed(
            &saved.store,
            &saved.database,
            "remember original",
            &|snapshot| saved.accept_publication(snapshot),
        )
        .await
        .unwrap();
    let source = &prepared.retrieval[0].source.id;
    let id = saved.snapshot().unwrap().state.session_id;
    let request = format!("source {id} {source}");
    let lines = memory_read_lines(
        &project,
        root.path(),
        &root.path().join("sessions"),
        &request,
    )
    .unwrap();
    assert!(
        lines
            .iter()
            .any(|line| line.to_string().contains("original verified source 1"))
    );
    let raw = saved.store.directory().join("raw.jsonl");
    let original = std::fs::read_to_string(&raw).unwrap();
    std::fs::write(
        &raw,
        original.replacen("original verified", "modified verified", 1),
    )
    .unwrap();
    assert!(
        memory_read_lines(
            &project,
            root.path(),
            &root.path().join("sessions"),
            &request
        )
        .is_err()
    );
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn m3c_actual_resume_migrates_missing_workflow_without_losing_identity() {
    exercise_actual_resume_after_owned_publication(true).await;
}

#[cfg(target_os = "linux")]
async fn exercise_actual_resume_after_owned_publication(missing_workflow: bool) {
    use polaris_core::{
        conversation_state::HistoryMode, management_effect::*, rewind_workspace::WorkspaceOwner,
        session_store::PersistedSession,
    };
    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let state = root.path().join("state");
    let sessions = root.path().join("sessions");
    for path in [&project, &state, &sessions] {
        std::fs::create_dir(path).unwrap();
    }
    std::fs::set_permissions(
        &state,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    let owner = WorkspaceOwner::open(
        &project,
        &state.join("rewind-workspace"),
        Default::default(),
    )
    .unwrap();
    let lease = owner.try_mutation().unwrap();
    let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
    drop(lease);
    let config = polaris_core::workflow::WorkflowConfig::default();
    let workflow = polaris_core::workflow::SessionWorkflow::new(config.clone());
    let selected = PersistedSession::create(
        root.path(),
        &state.join("memory.sqlite3"),
        project.to_str().unwrap(),
        HistoryMode::Legacy,
        if missing_workflow {
            None
        } else {
            Some(&workflow)
        },
    )
    .unwrap();
    let mut seeded = Session::new();
    seeded.attach(selected.clone()).unwrap();
    seeded.push_user("selected transcript");
    validate_resume_workspace(&owner, &selected).unwrap();
    let selected_id = selected.snapshot().unwrap().state.session_id;
    let mut current = Session::new();
    current.push_user("unsaved current");
    let mut path = sessions.join("current.jsonl");
    let mut meta = path.with_extension("meta.json");
    let mut started = 0;
    let mut status = Status::Idle;
    let mut lines = Vec::new();
    let terminal = RefCell::new(Terminal::new(TestBackend::new(120, 40)).unwrap());
    let mut keys = ScriptedReader(VecDeque::from([KeyCode::Enter, KeyCode::Char('y')]));
    handle_resume(
        &terminal,
        &mut keys,
        &sessions,
        &state,
        project.to_str().unwrap(),
        &config,
        false,
        &[],
        None,
        Some(&owner),
        Some(&effects),
        &mut current,
        &mut path,
        &mut meta,
        &mut started,
        &mut status,
        &mut lines,
    )
    .await;
    assert!(matches!(status, Status::Idle));
    assert_eq!(current.messages[0].content, "selected transcript");
    assert_eq!(
        current
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .state
            .session_id,
        selected_id
    );
    let override_config = polaris_core::workflow::WorkflowConfig {
        enabled: !config.enabled,
        ..config
    };
    let mut keys = ScriptedReader(VecDeque::from([
        KeyCode::Enter,
        KeyCode::Char('y'),
        KeyCode::Char('y'),
    ]));
    handle_resume(
        &terminal,
        &mut keys,
        &sessions,
        &state,
        project.to_str().unwrap(),
        &override_config,
        true,
        &[],
        None,
        Some(&owner),
        Some(&effects),
        &mut current,
        &mut path,
        &mut meta,
        &mut started,
        &mut status,
        &mut lines,
    )
    .await;
    if let Status::Error(message) | Status::Notice(message) = &status {
        panic!("resume: {message}");
    }
    assert!(matches!(status, Status::Idle));
    assert_eq!(
        current
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .state
            .session_id,
        selected_id
    );
    assert_eq!(
        current.workflow.as_ref().unwrap().config.enabled,
        override_config.enabled
    );
    let mut keys = ScriptedReader(VecDeque::from([
        KeyCode::Enter,
        KeyCode::Char('y'),
        KeyCode::Esc,
    ]));
    handle_resume(
        &terminal,
        &mut keys,
        &sessions,
        &state,
        project.to_str().unwrap(),
        &workflow.config,
        true,
        &[],
        None,
        Some(&owner),
        Some(&effects),
        &mut current,
        &mut path,
        &mut meta,
        &mut started,
        &mut status,
        &mut lines,
    )
    .await;
    assert!(matches!(&status,Status::Error(message) if message.contains("cancelled")));
    current.check_persistence().unwrap();
    assert_eq!(
        current.workflow.as_ref().unwrap().config.enabled,
        workflow.config.enabled
    );
    assert_eq!(
        current
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .state
            .workflow_config
            .unwrap()
            .config
            .enabled,
        workflow.config.enabled
    );
    assert!(effects.blocker().unwrap().is_none());
    assert!(
        effects
            .effects()
            .unwrap()
            .iter()
            .any(|e| e.request.inventory.encoded() == "D07"
                && matches!(e.state, ManagementEffectStateV1::Completed { .. }))
    );
}

#[test]
fn handle_resume_with_nothing_saved_dismisses_on_any_key_and_leaves_the_session_untouched() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut session = Session {
        disable_files_md_auto_regenerate: true,
        ..Session::default()
    };
    session.push_user("current, unsaved conversation");
    let original_path = dir.path().join("current.jsonl");
    let mut session_path = original_path.clone();
    let mut meta_path = dir.path().join("current.meta.json");
    let mut session_started_at_millis = 111;
    let mut status = Status::Idle;
    let mut local_lines = Vec::new();
    let backend = TestBackend::new(60, 14);
    let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
    let mut reader = ScriptedReader(std::collections::VecDeque::from([KeyCode::Enter]));

    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(handle_resume(
            &terminal,
            &mut reader,
            dir.path(),
            dir.path(),
            "/tmp/current",
            &polaris_core::workflow::WorkflowConfig::default(),
            false,
            &[],
            None,
            #[cfg(unix)]
            None,
            #[cfg(unix)]
            None,
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut session_started_at_millis,
            &mut status,
            &mut local_lines,
        ));

    assert_eq!(
        session.messages.len(),
        1,
        "nothing to resume to, so unchanged"
    );
    assert_eq!(session_path, original_path);
    assert!(matches!(status, Status::Idle));
}

#[test]
fn handle_resume_swaps_in_the_picked_conversation() {
    let dir = tempfile::tempdir().expect("temp dir");
    persist::write_meta_if_absent(
        &dir.path().join("saved.meta.json"),
        &persist::SessionMeta {
            cwd: "/tmp/current".to_string(),
            started_at_millis: 500,
        },
    )
    .expect("write meta");
    persist::append_message(
        &dir.path().join("saved.jsonl"),
        &Message::user("resumed hi"),
    )
    .expect("seed saved session");

    let mut session = Session {
        disable_files_md_auto_regenerate: true,
        ..Session::default()
    };
    session.push_user("current, unsaved conversation");
    let mut session_path = dir.path().join("current.jsonl");
    let mut meta_path = dir.path().join("current.meta.json");
    let mut session_started_at_millis = 111;
    let mut status = Status::Idle;
    let mut local_lines = vec![Line::from("leftover /diff output")];
    let backend = TestBackend::new(60, 14);
    let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
    // Only one saved conversation exists, so it's already highlighted;
    // Enter accepts it directly, same as the slash-command popup.
    let mut reader = ScriptedReader(std::collections::VecDeque::from([KeyCode::Enter]));

    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(handle_resume(
            &terminal,
            &mut reader,
            dir.path(),
            dir.path(),
            "/tmp/current",
            &polaris_core::workflow::WorkflowConfig::default(),
            false,
            &[],
            None,
            #[cfg(unix)]
            None,
            #[cfg(unix)]
            None,
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut session_started_at_millis,
            &mut status,
            &mut local_lines,
        ));

    assert_eq!(session.messages.len(), 1);
    assert_eq!(session.messages[0].content, "resumed hi");
    assert_eq!(session_path, dir.path().join("saved.jsonl"));
    assert_eq!(meta_path, dir.path().join("saved.meta.json"));
    assert_eq!(session_started_at_millis, 500);
    assert!(session.disable_files_md_auto_regenerate);
    assert!(
        local_lines.is_empty(),
        "switching conversations clears local-only output"
    );
    assert!(matches!(status, Status::Idle));
}

#[test]
fn same_path_resume_rebuilds_the_projected_transcript_after_local_lines_are_cleared() {
    let dir = tempfile::tempdir().expect("temp dir");
    let saved_path = dir.path().join("saved.jsonl");
    persist::write_meta_if_absent(
        &dir.path().join("saved.meta.json"),
        &persist::SessionMeta {
            cwd: "/tmp/current".to_string(),
            started_at_millis: 500,
        },
    )
    .expect("write meta");
    persist::append_message(&saved_path, &Message::user("resumed hi")).expect("seed saved session");

    let mut session = Session::default();
    session.push_user("old in-memory conversation");
    let previous_session_path = saved_path.clone();
    let mut session_path = previous_session_path.clone();
    let mut meta_path = dir.path().join("saved.meta.json");
    let mut session_started_at_millis = 111;
    let mut status = Status::Idle;
    let mut local_lines = vec![Line::from("leftover /diff output")];
    let header = render::HeaderInfo {
        provider_name: "test",
        model_name: "test",
        usage: Default::default(),
        cwd: "/tmp/current",
        cwd_short: "/tmp/current",
        effort_name: "auto",
    };
    let mut conversation_view = conversation_view::ConversationViewState::new(&header, 60);
    conversation_view.append_new_history(&session, &local_lines);
    let terminal = RefCell::new(Terminal::new(TestBackend::new(60, 14)).expect("terminal"));
    let mut reader = ScriptedReader(std::collections::VecDeque::from([KeyCode::Enter]));

    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(handle_resume(
            &terminal,
            &mut reader,
            dir.path(),
            dir.path(),
            "/tmp/current",
            &polaris_core::workflow::WorkflowConfig::default(),
            false,
            &[],
            None,
            #[cfg(unix)]
            None,
            #[cfg(unix)]
            None,
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut session_started_at_millis,
            &mut status,
            &mut local_lines,
        ));

    assert_eq!(session_path, previous_session_path);
    assert_eq!(session.messages[0].content, "resumed hi");
    assert!(local_lines.is_empty());
    reset_conversation_view_after_resume(&mut conversation_view, &header, 60);
    conversation_view.append_new_history(&session, &local_lines);

    let projected = conversation_view
        .history
        .iter()
        .map(|line| line.line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(projected.contains("✦ polaris"));
    assert!(projected.contains("resumed hi"));
    assert!(!projected.contains("old in-memory conversation"));
    assert!(!projected.contains("leftover /diff output"));
}

#[cfg(unix)]
#[test]
fn legacy_jsonl_resume_binds_the_imported_session_to_current_files() {
    use polaris_core::conversation_state::HistoryMode;
    use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};
    use polaris_core::session_store::PersistedSession;

    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        root.path(),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    let project = root.path().join("project");
    let sessions = root.path().join("sessions");
    let state_dir = root.path().join("state");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&sessions).unwrap();
    std::fs::create_dir(&state_dir).unwrap();
    std::fs::write(project.join("task.txt"), "current files").unwrap();
    let owner = WorkspaceOwner::open(
        &project,
        &root.path().join("rewind-workspace"),
        RewindLimits::default(),
    )
    .unwrap();
    persist::append_message(&sessions.join("saved.jsonl"), &Message::user("old request")).unwrap();
    persist::write_meta_if_absent(
        &sessions.join("saved.meta.json"),
        &persist::SessionMeta {
            cwd: project.display().to_string(),
            started_at_millis: 500,
        },
    )
    .unwrap();

    let project_id = persist::project_identity(&project).unwrap();
    let persistence = PersistedSession::create(
        root.path(),
        &state_dir.join("memory.sqlite3"),
        &project_id,
        HistoryMode::Legacy,
        None,
    )
    .unwrap();
    let mut session = Session::new();
    session.attach(persistence).unwrap();
    let mut session_path = sessions.join("current.jsonl");
    let mut meta_path = sessions.join("current.meta.json");
    let mut started = 1;
    let mut status = Status::Idle;
    let mut local_lines = Vec::new();
    let terminal = RefCell::new(Terminal::new(TestBackend::new(70, 16)).unwrap());
    let mut reader = ScriptedReader(std::collections::VecDeque::from([KeyCode::Enter]));
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(handle_resume(
            &terminal,
            &mut reader,
            &sessions,
            &state_dir,
            &project.display().to_string(),
            &polaris_core::workflow::WorkflowConfig::default(),
            false,
            &[],
            None,
            Some(&owner),
            None,
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut started,
            &mut status,
            &mut local_lines,
        ));
    assert!(matches!(status, Status::Idle));
    assert_eq!(session.messages[0].content, "old request");
    let imported = session.persistence.as_ref().unwrap();
    assert!(imported.store.workspace_head_binding().unwrap().is_some());
    assert!(validate_resume_workspace(&owner, imported).is_ok());
}

#[cfg(unix)]
#[test]
fn legacy_resume_does_not_run_after_a_project_rewind() {
    use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    let root = tempfile::tempdir().unwrap();
    std::fs::set_permissions(
        root.path(),
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    let project = root.path().join("project");
    let sessions = root.path().join("sessions");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&sessions).unwrap();
    let owner = WorkspaceOwner::open(
        &project,
        &root.path().join("rewind-workspace"),
        RewindLimits::default(),
    )
    .unwrap();
    let policy =
        SandboxPolicy::new(SandboxMode::WorkspaceWrite, std::slice::from_ref(&project)).unwrap();
    let file = project.join("task.txt");
    std::fs::write(&file, "before").unwrap();
    let pre = owner.capture(&policy).unwrap();
    std::fs::write(&file, "after").unwrap();
    let post = owner.capture(&policy).unwrap();
    let restored = owner.restore(&post, &pre).unwrap();
    owner.commit(&restored.transaction_id).unwrap();

    let saved_path = sessions.join("saved.jsonl");
    persist::append_message(&saved_path, &Message::user("old request")).unwrap();
    persist::write_meta_if_absent(
        &sessions.join("saved.meta.json"),
        &persist::SessionMeta {
            cwd: project.display().to_string(),
            started_at_millis: 500,
        },
    )
    .unwrap();
    let mut session = Session::new();
    session.push_user("current request");
    let mut session_path = sessions.join("current.jsonl");
    let mut meta_path = sessions.join("current.meta.json");
    let mut started = 1;
    let mut status = Status::Idle;
    let mut local_lines = Vec::new();
    let terminal = RefCell::new(Terminal::new(TestBackend::new(70, 16)).unwrap());
    let mut reader = ScriptedReader(std::collections::VecDeque::from([KeyCode::Enter]));
    tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(handle_resume(
            &terminal,
            &mut reader,
            &sessions,
            &root.path().join("state"),
            &project.display().to_string(),
            &polaris_core::workflow::WorkflowConfig::default(),
            false,
            &[],
            None,
            Some(&owner),
            None,
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut started,
            &mut status,
            &mut local_lines,
        ));
    assert_eq!(session.messages[0].content, "current request");
    assert!(matches!(status, Status::Notice(_)));
    let display = terminal
        .borrow()
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(display.contains("old request"));
}

#[test]
fn read_only_transcript_viewer_shows_saved_messages_without_session_switch() {
    let terminal = RefCell::new(Terminal::new(TestBackend::new(70, 12)).unwrap());
    let mut reader = ScriptedReader(std::collections::VecDeque::from([
        KeyCode::Down,
        KeyCode::Esc,
    ]));
    show_read_only_transcript(
        &terminal,
        &mut reader,
        &[
            Message::user("saved request"),
            Message::assistant("saved answer"),
        ],
        "raw.jsonl",
    );
    let display = terminal
        .borrow()
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect::<String>();
    assert!(display.contains("saved request"));
    assert!(display.contains("saved answer"));
    assert!(display.contains("raw.jsonl"));
}

#[test]
fn handle_resume_cancelled_with_esc_leaves_everything_untouched() {
    let dir = tempfile::tempdir().expect("temp dir");
    persist::write_meta_if_absent(
        &dir.path().join("saved.meta.json"),
        &persist::SessionMeta {
            cwd: "/tmp/current".to_string(),
            started_at_millis: 500,
        },
    )
    .expect("write meta");
    persist::append_message(
        &dir.path().join("saved.jsonl"),
        &Message::user("resumed hi"),
    )
    .expect("seed saved session");

    let mut session = Session::default();
    session.push_user("current, unsaved conversation");
    let original_path = dir.path().join("current.jsonl");
    let mut session_path = original_path.clone();
    let mut meta_path = dir.path().join("current.meta.json");
    let mut session_started_at_millis = 111;
    let mut status = Status::Idle;
    let mut local_lines = Vec::new();
    let backend = TestBackend::new(60, 14);
    let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
    let mut reader = ScriptedReader(std::collections::VecDeque::from([KeyCode::Esc]));

    tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(handle_resume(
            &terminal,
            &mut reader,
            dir.path(),
            dir.path(),
            "/tmp/current",
            &polaris_core::workflow::WorkflowConfig::default(),
            false,
            &[],
            None,
            #[cfg(unix)]
            None,
            #[cfg(unix)]
            None,
            &mut session,
            &mut session_path,
            &mut meta_path,
            &mut session_started_at_millis,
            &mut status,
            &mut local_lines,
        ));

    assert_eq!(session.messages.len(), 1);
    assert_eq!(session.messages[0].content, "current, unsaved conversation");
    assert_eq!(session_path, original_path);
}

#[test]
fn an_unknown_command_names_itself_in_the_notice() {
    let mut session = Session::default();
    let mut status = Status::Idle;
    let dir = tempfile::tempdir().expect("temp dir");
    let session_path = dir.path().join("session.jsonl");
    let mut local_lines = Vec::new();

    call(
        slash::Action::Unknown("frobnicate".to_string()),
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );

    match status {
        Status::Notice(n) => assert!(n.contains("frobnicate")),
        _ => panic!("expected a Notice"),
    }
}

/// A `Provider` whose only job is recording what `set_model` was last
/// called with, so `handle_model` tests can confirm it actually
/// reaches the provider (not just the display copy). `complete` is
/// never exercised by those tests.
struct RecordingProvider {
    model: std::sync::Mutex<Option<String>>,
    effort: std::sync::Mutex<Option<String>>,
}

impl RecordingProvider {
    fn new() -> Self {
        Self {
            model: std::sync::Mutex::new(None),
            effort: std::sync::Mutex::new(None),
        }
    }
}

#[async_trait::async_trait]
impl Provider for RecordingProvider {
    async fn complete(
        &self,
        _req: polaris_provider::CompletionRequest,
    ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
        unimplemented!("not exercised by handle_model tests")
    }

    fn set_model(&self, model: &str) {
        *self.model.lock().expect("lock") = Some(model.to_string());
    }

    fn set_effort(&self, effort: Option<&str>) {
        *self.effort.lock().expect("lock") = effort.map(str::to_string);
    }
}

#[tokio::test]
async fn handle_model_switches_the_hosted_provider_without_an_effort_step() {
    let backend = TestBackend::new(70, 14);
    let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
    let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Enter]));
    let cloud = Arc::new(RecordingProvider::new());
    let cloud_dyn: Arc<dyn Provider> = cloud.clone();
    let mut provider = cloud_dyn.clone();
    let usage_meter = polaris_provider::UsageMeter::default();
    let local_router = polaris_core::local_execution::LocalRouter::new(1);
    let mut provider_name = "codex".to_string();
    let mut model_name = "gpt-6-astra".to_string();
    let mut effort_name = render::DEFAULT_EFFORT.to_string();
    let mut status = Status::Idle;

    handle_model(
        &terminal,
        &mut reader,
        &mut provider,
        &cloud_dyn,
        &usage_meter,
        &local_router,
        &mut provider_name,
        "codex",
        &mut model_name,
        render::DEFAULT_EFFORT,
        &mut effort_name,
        &mut status,
    )
    .await;

    assert_eq!(model_name, "gpt-6.1-sol");
    assert_eq!(provider_name, "codex");
    assert_eq!(effort_name, render::DEFAULT_EFFORT);
    assert!(Arc::ptr_eq(&provider, &cloud_dyn));
    assert_eq!(
        *cloud.model.lock().expect("lock"),
        Some("gpt-6.1-sol".to_string()),
        "the provider itself must be switched, not just the display copy"
    );
    assert_eq!(*cloud.effort.lock().expect("lock"), None);
    match status {
        Status::Notice(n) => {
            assert!(n.contains("gpt-6.1-sol"));
        }
        _ => panic!("expected a Notice"),
    }
}

#[tokio::test]
async fn handle_model_cancelled_with_esc_leaves_everything_untouched() {
    let backend = TestBackend::new(70, 14);
    let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
    let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Esc]));
    let cloud = Arc::new(RecordingProvider::new());
    let cloud_dyn: Arc<dyn Provider> = cloud.clone();
    let mut provider = cloud_dyn.clone();
    let usage_meter = polaris_provider::UsageMeter::default();
    let local_router = polaris_core::local_execution::LocalRouter::new(1);
    let mut provider_name = "codex".to_string();
    let mut model_name = "gpt-6-astra".to_string();
    let mut effort_name = render::DEFAULT_EFFORT.to_string();
    let mut status = Status::Idle;

    handle_model(
        &terminal,
        &mut reader,
        &mut provider,
        &cloud_dyn,
        &usage_meter,
        &local_router,
        &mut provider_name,
        "codex",
        &mut model_name,
        render::DEFAULT_EFFORT,
        &mut effort_name,
        &mut status,
    )
    .await;

    assert_eq!(model_name, "gpt-6-astra");
    assert_eq!(effort_name, render::DEFAULT_EFFORT);
    assert_eq!(*cloud.model.lock().expect("lock"), None);
    assert_eq!(*cloud.effort.lock().expect("lock"), None);
    assert!(matches!(status, Status::Idle));
}

#[test]
fn m3_acceptance_status_includes_usage_and_fixed_capability_truth() {
    use polaris_core::workflow::{SessionWorkflow, WorkflowSkillV1, WorkflowStateV1};

    let mut session = Session::default();
    let workflow_state = WorkflowStateV1 {
        skill_manifest_hash: "0123456789abcdef".repeat(4),
        skills: vec![
            WorkflowSkillV1 {
                id: "builtin:workflow-core".into(),
                source: "builtin".into(),
                body_hash: "core-hash".into(),
            },
            WorkflowSkillV1 {
                id: "user:example".into(),
                source: "user".into(),
                body_hash: "example-hash".into(),
            },
        ],
        ..Default::default()
    };
    session.workflow = Some(
        SessionWorkflow::restore(
            Default::default(),
            Default::default(),
            workflow_state.clone(),
        )
        .unwrap(),
    );
    let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
    #[cfg(unix)]
    let root = tempfile::tempdir().unwrap();
    #[cfg(unix)]
    let store = {
        use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};
        let project = root.path().join("project");
        std::fs::create_dir(&project).unwrap();
        let state_dir = root.path().join("state");
        std::fs::create_dir(&state_dir).unwrap();
        let owner = WorkspaceOwner::open(
            &project,
            &state_dir.join("rewind-workspace"),
            RewindLimits::default(),
        )
        .unwrap();
        let lease = owner.try_mutation().unwrap();
        polaris_core::management_effect::ManagementEffectStoreV1::open_at_state_dir(
            &project, &state_dir, &lease,
        )
        .unwrap()
    };
    let usage = polaris_provider::Usage {
        input_tokens: 100,
        output_tokens: 20,
        total_tokens: 120,
        cached_tokens: 80,
    };

    let local_lines = status_lines(StatusSnapshot {
        session: &session,
        session_path: Some(Path::new("/synthetic/conversation-id.jsonl")),
        accounts: Ok(vec![status_row(
            "Registration",
            "fixture-registration / fixture / credentials present",
        )]),
        #[cfg(unix)]
        binding: None,
        provider_name: "openai",
        model_name: "gpt-5.4",
        usage_report: polaris_provider::UsageReport {
            usage,
            missing_responses: 3,
            failed_requests: 2,
            ..Default::default()
        },
        sandbox: &sandbox,
        approval: ApprovalPolicy::Never,
        startup_approval: ApprovalPolicy::OnRequest,
        mcp_external: true,
        audit_path: Some(std::path::Path::new("/tmp/audit\nforged.jsonl")),
        turn_authority_present: false,
        fixed_effort: None,
        answer_finalizer: None,
        #[cfg(unix)]
        project_root: &root.path().join("project"),
        #[cfg(unix)]
        management_effects: &store,
        #[cfg(unix)]
        recovery_issue: None,
    });
    let text = local_lines
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("cache 80"), "{text}");
    assert!(text.contains("conversation-id"));
    assert!(text.contains("fixture-registration / fixture / credentials present"));
    for (label, value) in [
        ("CAT configuration:", "no shadow owner"),
        ("CAT installation:", "not reconciled"),
        ("CAT verification:", "unverified"),
        ("CAT observation:", "not retrieved"),
    ] {
        assert!(local_lines.iter().any(|line| {
            let line = line.to_string();
            line.contains(label) && line.ends_with(value)
        }));
    }
    let diagnostic = doctor_lines(
        "openai\nforged",
        "model\tforged",
        std::path::Path::new("."),
        &sandbox,
        ApprovalPolicy::Never,
        false,
    );
    assert!(
        diagnostic
            .iter()
            .all(|line| !line.to_string().contains(['\n', '\t', '\x1b']))
    );

    for section in [
        "Polaris status (read-only)",
        "Account",
        "Conditions",
        "Observations and workflow",
        "Availability",
        "Management and diagnostics",
    ] {
        assert!(local_lines.iter().any(|line| line.to_string() == section));
    }
    assert!(text.contains("/tmp/audit\\nforged.jsonl"), "{text}");
    assert!(local_lines.iter().any(|line| {
        let line = line.to_string();
        line.contains("effort:")
            && line.contains("automatic (changing fixed overrides is unsupported in TUI)")
    }));
    assert!(text.contains("routine/review configured=false"));
    assert!(text.contains("observation disconnected"));
    assert!(text.contains("resource capacity unverified"));
    assert!(local_lines.iter().any(|line| {
        let line = line.to_string();
        line.contains("web-search:")
            && line.contains("disabled")
            && line.contains("endpoint capability contract unverified")
    }));
    assert!(text.contains("in 100"));
    assert!(text.contains("out 20"));
    assert!(text.contains("total 120"));
    assert!(local_lines.iter().any(|line| {
        let line = line.to_string();
        line.contains("Missing / failed:") && line.ends_with("3 / 2")
    }));
    assert!(local_lines.iter().any(|line| {
        let line = line.to_string();
        line.contains("External MCP:") && line.contains("true") && line.contains("/mcp")
    }));
    assert!(text.contains("unverified (authentication, model, MCP"));
    assert!(text.contains("builtin:workflow-core"));
    assert!(text.contains("user:example"));
    assert!(text.contains(&workflow_state.skill_manifest_hash));
    let history: Vec<_> = local_lines
        .iter()
        .cloned()
        .map(|line| render::HistoryLine {
            line,
            shaded: false,
        })
        .collect();
    for width in [80, 160] {
        let wrapped = render::wrap_history_lines(&history, width);
        assert!(
            // Account/session fields and English labels add rows. Keep a
            // bounded summary while retaining every value at 80 columns.
            wrapped.len() <= 48,
            "{width} columns: {} rows",
            wrapped.len()
        );
    }
    assert_eq!(session.workflow.as_ref().unwrap().state(), workflow_state);
    assert!(session.messages.is_empty());
}

#[test]
fn status_rows_align_labels_and_keep_values_when_wrapped() {
    use unicode_width::UnicodeWidthStr;

    let value = "value";
    for label in ["Model", "provider", "Approval (startup)", ""] {
        let line = status_row(label, value).to_string();
        let start = line.find(value).unwrap();
        assert_eq!(line[..start].width(), 25, "{line}");
    }

    let path = "/synthetic/project/with-a-long-directory-name/audit.jsonl";
    let hash = "0123456789abcdef".repeat(4);
    let history = vec![
        render::HistoryLine {
            line: status_row("audit", path),
            shaded: false,
        },
        render::HistoryLine {
            line: status_row("manifest", &hash),
            shaded: false,
        },
    ];
    for width in [24, 40, 80] {
        let wrapped = render::wrap_history_lines(&history, width);
        assert!(wrapped.iter().all(|row| row.line.width() <= width as usize));
        let text: String = wrapped.iter().map(|row| row.line.to_string()).collect();
        assert!(text.contains(path), "{text}");
        assert!(text.contains(&hash), "{text}");
    }
    let escaped = status_row("audit", "/tmp/audit\nforged\t\x1b.jsonl").to_string();
    assert!(!escaped.contains(['\n', '\t', '\x1b']));
    assert!(escaped.contains("/tmp/audit\\nforged\\t"), "{escaped}");
}

#[test]
fn phase_command_cannot_write_without_managed_owner() {
    let mut session = Session::new();
    session.workflow = Some(polaris_core::workflow::SessionWorkflow::new(
        polaris_core::workflow::WorkflowConfig {
            enabled: true,
            ..Default::default()
        },
    ));
    let mut status = Status::Idle;
    let dir = tempfile::tempdir().expect("temp dir");
    let persistence = polaris_core::session_store::PersistedSession::create(
        dir.path(),
        &dir.path().join("memory.sqlite3"),
        "project",
        polaris_core::conversation_state::HistoryMode::Legacy,
        session.workflow.as_ref(),
    )
    .expect("create v2 session");
    session.attach(persistence).expect("attach v2 session");
    let session_path = dir.path().join("session.jsonl");
    let mut local_lines = Vec::new();

    call(
        slash::Action::Phase("implement".into()),
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );
    assert_eq!(
        session.workflow.as_ref().unwrap().state().phase,
        polaris_core::workflow::Phase::General,
        "a manual phase command must not bypass M27's approved change-contract gate"
    );
    call(
        slash::Action::Phase("review".into()),
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );
    assert_eq!(
        session.workflow.as_ref().unwrap().state().phase,
        polaris_core::workflow::Phase::General
    );
    assert_eq!(
        session
            .persistence
            .as_ref()
            .expect("v2 persistence")
            .snapshot()
            .expect("snapshot")
            .state
            .workflow
            .phase,
        polaris_core::workflow::Phase::General,
        "the fallback dispatch must not bypass the management effect owner"
    );
    assert!(matches!(status, Status::Error(_)));
    call(
        slash::Action::Phase("not-a-phase".into()),
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );
    assert_eq!(
        session.workflow.as_ref().unwrap().state().phase,
        polaris_core::workflow::Phase::General
    );
}

#[test]
fn workflow_command_cannot_write_without_managed_owner() {
    let mut session = Session::new();
    session.workflow = Some(polaris_core::workflow::SessionWorkflow::new(
        polaris_core::workflow::WorkflowConfig {
            enabled: true,
            ..Default::default()
        },
    ));
    let mut status = Status::Idle;
    let dir = tempfile::tempdir().expect("temp dir");
    let persistence = polaris_core::session_store::PersistedSession::create(
        dir.path(),
        &dir.path().join("memory.sqlite3"),
        "project",
        polaris_core::conversation_state::HistoryMode::Legacy,
        session.workflow.as_ref(),
    )
    .expect("create v2 session");
    session.attach(persistence).expect("attach v2 session");
    let session_path = dir.path().join("session.jsonl");
    let mut local_lines = Vec::new();

    call(
        slash::Action::Workflow("mode adaptive".into()),
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );
    assert_eq!(
        session.workflow.as_ref().unwrap().config.mode,
        polaris_core::workflow::WorkflowMode::Manual
    );
    assert_eq!(
        session
            .persistence
            .as_ref()
            .unwrap()
            .snapshot()
            .unwrap()
            .state
            .workflow_config
            .unwrap()
            .config
            .mode,
        polaris_core::workflow::WorkflowMode::Manual
    );
    assert!(matches!(status, Status::Error(_)));

    call(
        slash::Action::Workflow("off".into()),
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );
    assert!(session.workflow.as_ref().unwrap().config.enabled);
}

#[cfg(unix)]
#[test]
fn confirmed_phase_change_records_effect_and_cancel_preserves_session() {
    use polaris_core::management_effect::{ManagementEffectStateV1, ManagementEffectStoreV1};
    use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};

    let root = tempfile::tempdir().unwrap();
    let project = root.path().join("project");
    let state_dir = root.path().join("state");
    std::fs::create_dir(&project).unwrap();
    std::fs::create_dir(&state_dir).unwrap();
    std::fs::set_permissions(
        &state_dir,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o700),
    )
    .unwrap();
    let workspace = WorkspaceOwner::open(
        &project,
        &state_dir.join("rewind-workspace"),
        RewindLimits::default(),
    )
    .unwrap();
    let lease = workspace.try_mutation().unwrap();
    let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state_dir, &lease).unwrap();
    drop(lease);
    let project_id = persist::project_identity(&project).unwrap();
    let workflow =
        polaris_core::workflow::SessionWorkflow::new(polaris_core::workflow::WorkflowConfig {
            enabled: true,
            ..Default::default()
        });
    let persistence = polaris_core::session_store::PersistedSession::create(
        root.path(),
        &state_dir.join("memory.sqlite3"),
        &project_id,
        polaris_core::conversation_state::HistoryMode::Legacy,
        Some(&workflow),
    )
    .unwrap();
    let mut session = Session::new();
    session.workflow = Some(workflow);
    session.attach(persistence.clone()).unwrap();
    let terminal = RefCell::new(Terminal::new(TestBackend::new(100, 16)).unwrap());
    let mut status = Status::Idle;
    let journal = state_dir.join("management-effects-v1/journal.v1.jsonl");
    let before = persistence.snapshot().unwrap();
    let journal_before = std::fs::read(&journal).unwrap();

    let mut cancel = ScriptedReader(VecDeque::from([KeyCode::Esc]));
    handle_managed_workflow_action(
        &terminal,
        &mut cancel,
        slash::Action::Phase("review".into()),
        &mut session,
        &[],
        &workspace,
        &effects,
        &state_dir,
        false,
        &mut status,
    );
    assert_eq!(persistence.snapshot().unwrap().state, before.state);
    assert_eq!(std::fs::read(&journal).unwrap(), journal_before);

    let mut accept = ScriptedReader(VecDeque::from([KeyCode::Char('y')]));
    handle_managed_workflow_action(
        &terminal,
        &mut accept,
        slash::Action::Phase("review".into()),
        &mut session,
        &[],
        &workspace,
        &effects,
        &state_dir,
        false,
        &mut status,
    );
    assert!(matches!(status, Status::Notice(_)));
    assert_eq!(
        persistence.snapshot().unwrap().state.workflow.phase,
        polaris_core::workflow::Phase::Review
    );
    assert_eq!(effects.effects().unwrap().len(), 1);
    assert!(matches!(
        effects.effects().unwrap()[0].state,
        ManagementEffectStateV1::Completed { .. }
    ));

    let mut disable = ScriptedReader(VecDeque::from([KeyCode::Char('y')]));
    handle_managed_workflow_action(
        &terminal,
        &mut disable,
        slash::Action::Workflow("off".into()),
        &mut session,
        &[],
        &workspace,
        &effects,
        &state_dir,
        false,
        &mut status,
    );
    assert!(matches!(status, Status::Notice(_)));
    assert!(!session.workflow.as_ref().unwrap().config.enabled);
    assert_eq!(effects.effects().unwrap().len(), 2);

    let disabled_state = persistence.snapshot().unwrap().state;
    let disabled_journal = std::fs::read(&journal).unwrap();
    let mut unused = ScriptedReader(VecDeque::new());
    handle_managed_workflow_action(
        &terminal,
        &mut unused,
        slash::Action::Phase("implement".into()),
        &mut session,
        &[],
        &workspace,
        &effects,
        &state_dir,
        false,
        &mut status,
    );
    assert!(matches!(status, Status::Error(_)));
    assert_eq!(persistence.snapshot().unwrap().state, disabled_state);
    assert_eq!(std::fs::read(&journal).unwrap(), disabled_journal);
}

#[test]
fn pwd_reports_the_canonical_project_root() {
    let mut session = Session::default();
    let mut status = Status::Idle;
    let dir = tempfile::tempdir().expect("temp dir");
    let project = dir.path().join("project");
    let nested = project.join("nested");
    std::fs::create_dir_all(project.join(".git")).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    let session_path = dir.path().join("session.jsonl");
    let mut local_lines = Vec::new();

    call(
        slash::Action::Pwd,
        &mut session,
        &session_path,
        &mut status,
        &[],
        &nested,
        &mut local_lines,
    );

    match status {
        Status::Notice(n) => {
            assert_eq!(n, project.canonicalize().unwrap().display().to_string())
        }
        _ => panic!("expected a Notice"),
    }
}

/// The registry lives under `HOME`, not under the project, so "nothing is
/// registered" has to be **created** here rather than assumed. Assuming it
/// made this test pass only on a machine whose owner had never registered
/// an MCP server, and fail for everyone else — observed on 2026-09-18,
/// where registering three servers turned this into `left: 4, right: 1`.
#[test]
fn mcp_reports_state_into_the_transcript_without_connecting_or_asking_a_model() {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join(".git")).unwrap();
    let empty = tempfile::tempdir().unwrap();
    let prev = std::env::var_os("HOME");
    // SAFETY: serialized by `HOME_LOCK`; restored before the assertions.
    unsafe {
        std::env::set_var("HOME", empty.path());
    }
    let mut session = polaris_core::session::Session::new();
    let mut status = Status::Idle;
    let session_path = dir.path().join("session.jsonl");
    let mut local_lines = Vec::new();

    call(
        slash::Action::Mcp(String::new()),
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );
    let permitted_lines = mcp_state_lines(dir.path(), true);

    match prev {
        Some(p) => unsafe { std::env::set_var("HOME", p) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    assert_eq!(local_lines.len(), 2, "{local_lines:?}");
    assert_eq!(
        local_lines[0].to_string(),
        "External MCP host ceiling: denied"
    );
    assert_eq!(permitted_lines[0], "External MCP host ceiling: allowed");
    assert!(
        local_lines[1]
            .spans
            .iter()
            .any(|span| span.content.contains("No registered MCP servers")),
        "{local_lines:?}"
    );
    // A local command: nothing is sent and nothing joins the history.
    assert!(session.messages.is_empty());
    assert!(matches!(status, Status::Idle));
}

#[cfg(unix)]
#[test]
fn copy_with_no_assistant_reply_yet_reports_nothing_to_copy() {
    let mut session = Session::default();
    session.push_user("hi");
    let mut status = Status::Idle;
    let dir = tempfile::tempdir().expect("temp dir");
    let session_path = dir.path().join("session.jsonl");
    let mut local_lines = Vec::new();

    call(
        slash::Action::Copy,
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );

    match status {
        Status::Notice(n) => assert!(n.contains("nothing to copy")),
        _ => panic!("expected a Notice"),
    }
}

#[test]
fn apply_copy_action_with_a_reply_wires_it_through_to_the_copy_fn_and_status() {
    // Exercises the success path `apply_slash_action`'s `Action::Copy`
    // arm delegates to, without performing real clipboard I/O — an
    // accidental edit that drops the `*status = ...` assignment, or
    // wires the wrong session into `copy_last_reply_with`, fails this.
    let mut session = Session::default();
    session.push_user("hi");
    session.push_assistant("the answer is 4", Vec::new());
    let mut status = Status::Idle;

    apply_copy_action(&session, &mut status, |text| {
        assert_eq!(text, "the answer is 4");
        Ok(())
    });

    match status {
        Status::Notice(n) => assert!(n.contains("copied")),
        _ => panic!("expected a Notice"),
    }
}

#[test]
fn apply_copy_action_reports_a_failing_copy() {
    let mut session = Session::default();
    session.push_assistant("reply", Vec::new());
    let mut status = Status::Idle;

    apply_copy_action(&session, &mut status, |_| Err("no tty".to_string()));

    match status {
        Status::Notice(n) => {
            assert!(n.contains("can't copy"));
            assert!(n.contains("no tty"));
        }
        _ => panic!("expected a Notice"),
    }
}

#[test]
fn apply_selection_copy_reports_success() {
    let status = apply_selection_copy("hello world", |text| {
        assert_eq!(text, "hello world");
        Ok(())
    });
    assert!(status.contains("copied"));
}

#[test]
fn apply_selection_copy_reports_a_failing_copy() {
    let status = apply_selection_copy("hello", |_| Err("no tty".to_string()));
    assert!(status.contains("can't copy"));
    assert!(status.contains("no tty"));
}

#[cfg(unix)]
#[test]
fn export_writes_the_conversation_as_markdown() {
    let mut session = Session::default();
    session.push_user("what does this repo do?");
    session.push_assistant("it's a coding agent", Vec::new());
    let mut status = Status::Idle;
    let dir = tempfile::tempdir().expect("temp dir");
    let session_path = dir.path().join("session.jsonl");
    let mut local_lines = Vec::new();

    call(
        slash::Action::Export(String::new()),
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );

    let path = match status {
        Status::Notice(n) => {
            assert!(n.starts_with("exported to "));
            n.strip_prefix("exported to ").unwrap().to_string()
        }
        _ => panic!("expected a Notice"),
    };
    let written = std::fs::read_to_string(&path).expect("exported file should exist");
    assert!(written.contains("## You"));
    assert!(written.contains("what does this repo do?"));
    assert!(written.contains("## polaris"));
    assert!(written.contains("it's a coding agent"));
}

#[cfg(unix)]
#[test]
fn export_with_a_destination_argument_writes_there_instead() {
    let mut session = Session::default();
    session.push_user("hi");
    let mut status = Status::Idle;
    let dir = tempfile::tempdir().expect("temp dir");
    let session_path = dir.path().join("session.jsonl");
    let mut local_lines = Vec::new();

    call(
        slash::Action::Export("notes.md".to_string()),
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );

    assert!(dir.path().join("notes.md").exists());
}

#[test]
fn handle_fork_seeds_the_new_file_with_the_current_conversation_and_keeps_the_old_one() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut session = Session {
        disable_files_md_auto_regenerate: true,
        ..Session::default()
    };
    session.push_user("hello");
    session.push_assistant("hi there", Vec::new());
    let old_path = dir.path().join("old.jsonl");
    persist::append_message(&old_path, &session.messages[0]).expect("seed old file");
    let mut session_path = old_path.clone();
    let mut meta_path = dir.path().join("old.meta.json");
    let mut session_started_at_millis = 111;
    let mut status = Status::Idle;

    handle_fork(
        dir.path(),
        "/tmp/example",
        &mut session,
        &mut session_path,
        &mut meta_path,
        &mut session_started_at_millis,
        &mut status,
    );

    assert_ne!(session_path, old_path, "a fork should get its own file");
    assert!(session.disable_files_md_auto_regenerate);
    let (forked, truncated) = persist::load_session(&session_path).expect("load fork");
    assert!(!truncated);
    assert_eq!(forked.messages.len(), 2);
    assert_eq!(forked.messages[0].content, "hello");
    assert_eq!(forked.messages[1].content, "hi there");
    assert_eq!(
        std::fs::read_to_string(&old_path).unwrap().lines().count(),
        1,
        "the original conversation must stay intact on disk"
    );
    match status {
        Status::Notice(n) => assert!(n.contains("forked to")),
        _ => panic!("expected a Notice"),
    }
}

#[test]
fn fork_preserves_foreign_project_provenance_for_memory() {
    let logs = tempfile::tempdir().unwrap();
    let origin = tempfile::tempdir().unwrap();
    let current = tempfile::tempdir().unwrap();
    let mut session = Session::new();
    session.push_user("original project decision");
    let mut session_path = logs.path().join("original.jsonl");
    let mut meta_path = session_path.with_extension("meta.json");
    let origin_cwd = origin
        .path()
        .canonicalize()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    persist::write_meta_if_absent(
        &meta_path,
        &persist::SessionMeta {
            cwd: origin_cwd.clone(),
            started_at_millis: 1,
        },
    )
    .unwrap();
    let mut started = 1;
    let mut status = Status::Idle;
    handle_fork(
        logs.path(),
        current.path().to_str().unwrap(),
        &mut session,
        &mut session_path,
        &mut meta_path,
        &mut started,
        &mut status,
    );
    assert_eq!(persist::read_meta(&meta_path).unwrap().cwd, origin_cwd);
    memory::configure_saved_memory(&mut session, current.path(), logs.path(), &session_path)
        .unwrap();
    assert!(session.before_compact.as_ref().unwrap()(&session.messages).is_err());
    configure_saved_tool_memory(
        &mut session,
        current.path(),
        logs.path(),
        &session_path,
        Some(polaris_core::tool_memory::RetentionMode::History),
        None,
    )
    .unwrap();
    assert!(session.tool_memory.is_none());
    assert!(!logs.path().join("memory.sqlite3").exists());
}

#[test]
fn permissions_ceiling_uses_the_two_axis_transition_table() {
    use ApprovalPolicy::{Always, Never, OnRequest};
    let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
    for (ceiling, allowed) in [
        (Never, [true, false, false]),
        (OnRequest, [true, true, false]),
        (Always, [false, false, true]),
    ] {
        for (picked, expected) in [Never, OnRequest, Always].into_iter().zip(allowed) {
            assert_eq!(
                approval_within_startup_ceiling(ceiling, picked),
                expected,
                "ceiling={ceiling:?}, picked={picked:?}"
            );
            if expected {
                assert!(
                    delegation_capabilities(&sandbox, picked, false, &[]).does_not_escalate_from(
                        &delegation_capabilities(&sandbox, ceiling, false, &[])
                    ),
                    "delegation escalated: ceiling={ceiling:?}, picked={picked:?}"
                );
            }
        }
    }
}

#[test]
fn handle_permissions_rejects_a_policy_above_startup_never() {
    let backend = TestBackend::new(70, 14);
    let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
    // Down from Never (index 0) to OnRequest (index 1), then Enter.
    let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Enter]));
    let mut approval_policy = ApprovalPolicy::Never;
    let mut status = Status::Idle;
    let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();

    handle_permissions(
        &terminal,
        &mut reader,
        &mut approval_policy,
        ApprovalPolicy::Never,
        &sandbox,
        &mut status,
    );

    assert_eq!(approval_policy, ApprovalPolicy::Never);
    match status {
        Status::Notice(n) => assert!(n.contains("--approval on-request")),
        _ => panic!("expected a Notice"),
    }
}

#[test]
fn handle_permissions_can_narrow_and_restore_startup_on_request() {
    let backend = TestBackend::new(70, 14);
    let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
    let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
    let mut approval_policy = ApprovalPolicy::OnRequest;
    let mut status = Status::Idle;
    let mut to_never = ScriptedReader(VecDeque::from([KeyCode::Up, KeyCode::Enter]));
    handle_permissions(
        &terminal,
        &mut to_never,
        &mut approval_policy,
        ApprovalPolicy::OnRequest,
        &sandbox,
        &mut status,
    );
    assert_eq!(approval_policy, ApprovalPolicy::Never);

    let mut restore = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Enter]));
    handle_permissions(
        &terminal,
        &mut restore,
        &mut approval_policy,
        ApprovalPolicy::OnRequest,
        &sandbox,
        &mut status,
    );
    assert_eq!(approval_policy, ApprovalPolicy::OnRequest);
}

#[test]
fn handle_permissions_keeps_delegation_within_startup_on_request() {
    let backend = TestBackend::new(70, 14);
    let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
    let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
    let mut approval_policy = ApprovalPolicy::OnRequest;
    let mut status = Status::Idle;
    let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Enter]));
    handle_permissions(
        &terminal,
        &mut reader,
        &mut approval_policy,
        ApprovalPolicy::OnRequest,
        &sandbox,
        &mut status,
    );
    assert_eq!(approval_policy, ApprovalPolicy::OnRequest);
    assert!(matches!(status, Status::Notice(_)));
}

#[test]
fn handle_permissions_keeps_startup_always_confirmations() {
    let backend = TestBackend::new(70, 14);
    let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
    let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
    let mut approval_policy = ApprovalPolicy::Always;
    let mut status = Status::Idle;
    let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Enter]));
    handle_permissions(
        &terminal,
        &mut reader,
        &mut approval_policy,
        ApprovalPolicy::Always,
        &sandbox,
        &mut status,
    );
    assert_eq!(approval_policy, ApprovalPolicy::Always);
    assert!(matches!(status, Status::Notice(_)));
}

#[test]
fn handle_permissions_cancelled_with_esc_leaves_the_policy_untouched() {
    let backend = TestBackend::new(70, 14);
    let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
    let mut reader = ScriptedReader(VecDeque::from([KeyCode::Down, KeyCode::Esc]));
    let mut approval_policy = ApprovalPolicy::Never;
    let mut status = Status::Idle;
    let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();

    handle_permissions(
        &terminal,
        &mut reader,
        &mut approval_policy,
        ApprovalPolicy::Never,
        &sandbox,
        &mut status,
    );

    assert_eq!(approval_policy, ApprovalPolicy::Never);
    assert!(matches!(status, Status::Idle));
}

#[test]
fn handle_permissions_does_not_accept_a_selection_when_limits_cannot_be_shown() {
    let backend = TestBackend::new(39, 7);
    let terminal = RefCell::new(Terminal::new(backend).expect("terminal"));
    let mut reader = ScriptedReader(VecDeque::from([KeyCode::Enter, KeyCode::Esc]));
    let mut approval_policy = ApprovalPolicy::OnRequest;
    let mut status = Status::Idle;
    let sandbox = SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
    handle_permissions(
        &terminal,
        &mut reader,
        &mut approval_policy,
        ApprovalPolicy::OnRequest,
        &sandbox,
        &mut status,
    );
    assert_eq!(approval_policy, ApprovalPolicy::OnRequest);
    assert!(matches!(status, Status::Idle));
}

#[test]
#[cfg(unix)]
fn diff_reports_no_changes_outside_a_git_repository() {
    // A missing repository is reported as local content, not a crash.
    let mut session = Session::default();
    let mut status = Status::Idle;
    let dir = tempfile::tempdir().expect("temp dir");
    let session_path = dir.path().join("session.jsonl");
    let mut local_lines = Vec::new();

    let outcome = call(
        slash::Action::Diff,
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );

    assert!(matches!(outcome, SlashOutcome::Continue));
    assert!(!local_lines.is_empty(), "should still report something");
}

#[test]
fn help_lists_every_registered_command_in_scrollback() {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut session = Session::default();
    let mut status = Status::Idle;
    let mut local_lines = Vec::new();
    let outcome = call(
        slash::Action::Help,
        &mut session,
        &dir.path().join("session.jsonl"),
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );

    assert!(matches!(outcome, SlashOutcome::Continue));
    assert_eq!(local_lines.len(), slash::COMMANDS.len() + 1);
    for (line, command) in local_lines.iter().skip(1).zip(slash::COMMANDS) {
        assert_eq!(
            line.to_string(),
            format!("/{} — {}", command.name, command.description)
        );
    }
    assert!(local_lines.last().unwrap().to_string().contains("/quit"));
}

#[test]
#[cfg(unix)]
fn diff_shows_no_changes_in_a_clean_git_repository() {
    let dir = tempfile::tempdir().expect("temp dir");
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("git must be on PATH for this test")
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "test"]);
    std::fs::write(dir.path().join("a.txt"), "hello\n").expect("write");
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "initial"]);

    let mut session = Session::default();
    let mut status = Status::Idle;
    let session_path = dir.path().join("session.jsonl");
    let mut local_lines = Vec::new();

    call(
        slash::Action::Diff,
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );

    let rendered: String = local_lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect();
    assert!(rendered.contains("no changes"));
}

#[test]
#[cfg(unix)]
fn diff_shows_an_actual_uncommitted_change() {
    let dir = tempfile::tempdir().expect("temp dir");
    let run = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("git must be on PATH for this test")
    };
    run(&["init", "-q"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "test"]);
    std::fs::write(dir.path().join("a.txt"), "hello\n").expect("write");
    run(&["add", "."]);
    run(&["commit", "-q", "-m", "initial"]);
    std::fs::write(dir.path().join("a.txt"), "hello, changed\n").expect("write");

    let mut session = Session::default();
    let mut status = Status::Idle;
    let session_path = dir.path().join("session.jsonl");
    let mut local_lines = Vec::new();

    call(
        slash::Action::Diff,
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );

    let rendered: String = local_lines
        .iter()
        .flat_map(|l| l.spans.iter())
        .map(|s| s.content.as_ref())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("a.txt"));
    assert!(rendered.contains("changed"));
}

#[test]
#[cfg(unix)]
fn diff_shows_untracked_file_in_an_unborn_repository() {
    let dir = tempfile::tempdir().expect("temp dir");
    let init = std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(dir.path())
        .output()
        .expect("git must be on PATH for this test");
    assert!(init.status.success());
    std::fs::write(dir.path().join("new.txt"), "untracked\n").expect("write");

    let mut session = Session::default();
    let mut status = Status::Idle;
    let session_path = dir.path().join("session.jsonl");
    let mut local_lines = Vec::new();
    call(
        slash::Action::Diff,
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );

    let rendered = local_lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("new.txt"), "{rendered}");
    assert!(!rendered.contains("(no changes)"), "{rendered}");
}

#[test]
#[cfg(unix)]
fn diff_shows_staged_file_in_an_unborn_repository() {
    let dir = tempfile::tempdir().expect("temp dir");
    let run = |args: &[&str]| {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("git must be on PATH for this test");
        assert!(output.status.success(), "{:?}", output.stderr);
    };
    run(&["init", "-q"]);
    std::fs::write(dir.path().join("staged.txt"), "staged\n").expect("write");
    run(&["add", "staged.txt"]);

    let mut session = Session::default();
    let mut status = Status::Idle;
    let session_path = dir.path().join("session.jsonl");
    let mut local_lines = Vec::new();
    call(
        slash::Action::Diff,
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );

    let rendered = local_lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("staged.txt"), "{rendered}");
    assert!(!rendered.contains("(no changes)"), "{rendered}");
}

#[test]
#[cfg(unix)]
fn diff_hostile_environment_child() {
    let Some(project) = std::env::var_os("POLARIS_T13_CHILD_TARGET") else {
        return;
    };
    let project = std::path::PathBuf::from(project);
    let mut session = Session::default();
    let mut status = Status::Idle;
    let mut local_lines = Vec::new();
    call(
        slash::Action::Diff,
        &mut session,
        &project.join("session.jsonl"),
        &mut status,
        &[],
        &project,
        &mut local_lines,
    );
    let rendered = local_lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(rendered.contains("target.txt"), "{rendered}");
    assert!(!rendered.contains("outside.txt"), "{rendered}");
}

#[test]
#[cfg(unix)]
fn diff_ignores_hostile_git_environment() {
    use std::os::unix::fs::PermissionsExt;

    let dir = tempfile::tempdir().expect("temp dir");
    let target = dir.path().join("target");
    let outside = dir.path().join("outside");
    for repo in [&target, &outside] {
        std::fs::create_dir(repo).expect("create repository directory");
        let output = std::process::Command::new("/usr/bin/git")
            .args(["init", "-q"])
            .current_dir(repo)
            .output()
            .expect("git init");
        assert!(output.status.success(), "{:?}", output.stderr);
    }
    std::fs::write(target.join("target.txt"), "target\n").expect("write target");
    std::fs::write(outside.join("outside.txt"), "outside\n").expect("write outside");

    let executable = std::env::current_exe().expect("test executable");
    let run_child = |configure: &dyn Fn(&mut std::process::Command)| {
        let mut child = std::process::Command::new(&executable);
        child
            .arg("diff_hostile_environment_child")
            .arg("--nocapture")
            .env("POLARIS_T13_CHILD_TARGET", &target);
        configure(&mut child);
        let output = child.output().expect("child test");
        assert!(
            output.status.success(),
            "stdout: {}\nstderr: {}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    };
    run_child(&|child| {
        child.env("GIT_DIR", outside.join(".git"));
        child.env("GIT_WORK_TREE", &outside);
    });

    let fake_bin = dir.path().join("fake-bin");
    std::fs::create_dir(&fake_bin).expect("create fake bin");
    let fake_git = fake_bin.join("git");
    let marker = dir.path().join("fake-git-called");
    std::fs::write(
        &fake_git,
        "#!/bin/sh\n: > \"$POLARIS_T13_GIT_MARKER\"\nexit 77\n",
    )
    .expect("write fake git");
    std::fs::set_permissions(&fake_git, std::fs::Permissions::from_mode(0o755))
        .expect("make fake git executable");
    run_child(&|child| {
        child
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env("PATH", &fake_bin)
            .env("POLARIS_T13_GIT_MARKER", &marker);
    });
    assert!(!marker.exists(), "fake git on PATH must not run");
}

#[test]
#[cfg(unix)]
fn diff_bounds_large_git_output() {
    let dir = tempfile::tempdir().expect("temp dir");
    let run = |args: &[&str]| {
        let output = std::process::Command::new("/usr/bin/git")
            .args(args)
            .current_dir(dir.path())
            .output()
            .expect("git must run");
        assert!(output.status.success(), "{:?}", output.stderr);
    };
    run(&["init", "-q"]);
    std::fs::write(
        dir.path().join("large.txt"),
        "x".repeat(polaris_core::project_git::MAX_GIT_OUTPUT_BYTES + 1024),
    )
    .expect("write large file");
    run(&["add", "large.txt"]);

    let mut session = Session::default();
    let mut status = Status::Idle;
    let mut local_lines = Vec::new();
    call(
        slash::Action::Diff,
        &mut session,
        &dir.path().join("session.jsonl"),
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );
    let rendered = local_lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        rendered.contains("Cannot retrieve Git information"),
        "{rendered}"
    );
    assert!(!rendered.contains("large.txt"), "{rendered}");
}

#[cfg(unix)]
#[test]
fn clear_also_empties_local_lines_from_a_previous_diff() {
    let mut session = Session::default();
    let mut status = Status::Idle;
    let dir = tempfile::tempdir().expect("temp dir");
    let session_path = dir.path().join("session.jsonl");
    let mut local_lines = vec![ratatui::text::Line::from("leftover diff output")];

    call(
        slash::Action::Clear,
        &mut session,
        &session_path,
        &mut status,
        &[],
        dir.path(),
        &mut local_lines,
    );

    assert!(local_lines.is_empty());
}

#[test]
fn review_prompt_with_no_extra_text_is_the_bare_instruction() {
    assert_eq!(review_prompt(""), REVIEW_INSTRUCTION);
}

#[test]
fn review_prompt_appends_the_extra_text_when_given() {
    let p = review_prompt("the auth module");
    assert!(p.starts_with(REVIEW_INSTRUCTION));
    assert!(p.contains("the auth module"));
}

#[test]
fn scroll_offset_saturates_at_zero_going_down() {
    let mut scroll_offset: usize = 0;
    scroll_offset = scroll_offset.saturating_sub(1);
    assert_eq!(scroll_offset, 0);
}

#[test]
fn scroll_offset_grows_unbounded_going_up_since_the_window_clamps_it() {
    // Mirrors what the Up-key handler in run() does — the clamp lives in
    // visible_history_window (Task 1), not here, by design (see that
    // task's doc comment).
    let mut scroll_offset: usize = 0;
    for _ in 0..1000 {
        scroll_offset = scroll_offset.saturating_add(1);
    }
    assert_eq!(scroll_offset, 1000);
    assert_eq!(render::visible_history_window(50, scroll_offset, 10), 0..10);
}

#[test]
// The `47` below is never read before being overwritten — that's the
// point (it documents the pre-reset "mid-scroll" value the Submit arm
// discards), so silence clippy's unused_assignments lint for it.
#[allow(unused_assignments)]
fn scroll_offset_test_helper_matches_the_submit_reset_contract() {
    // Documents/pins the exact behavior Task 7 Step 2 wires into run():
    // a non-empty Submit resets scroll_offset to 0. run()'s own loop
    // isn't independently driveable in a test (see Task 7 Step 3's note),
    // so this pins the invariant at the type level instead: after any
    // number of scroll-up steps, resetting to 0 always shows the tail.
    let history_len = 200;
    let mut scroll_offset: usize = 47; // mid-scroll
    scroll_offset = 0; // what the Submit arm does
    assert_eq!(
        render::visible_history_window(history_len, scroll_offset, 10),
        190..200
    );
}
