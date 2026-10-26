
#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_process_signal_install_failure_exits_one_before_terminal_mutation() {
    use clap::Parser;

    let interactive = super::Args::try_parse_from(["polaris"]).unwrap();
    assert!(super::linux_interactive_requested(&interactive));
    let one_shot = super::Args::try_parse_from(["polaris", "--prompt", "hello"]).unwrap();
    assert!(!super::linux_interactive_requested(&one_shot));
    let confined = super::Args::try_parse_from(["polaris", "--confined-read"]).unwrap();
    assert!(!super::linux_interactive_requested(&confined));

    let entered = std::rc::Rc::new(std::cell::Cell::new(false));
    let entered_run = std::rc::Rc::clone(&entered);
    let diagnostics = std::rc::Rc::new(std::cell::RefCell::new(Vec::new()));
    let reported = std::rc::Rc::clone(&diagnostics);
    let status = super::run_with_linux_signal_owner(
        || Err("x".repeat(2048)),
        move || async move {
            entered_run.set(true);
            std::process::ExitCode::SUCCESS
        },
        move |message| reported.borrow_mut().push(message.to_owned()),
    )
    .await;
    assert_eq!(status, std::process::ExitCode::FAILURE);
    assert!(
        !entered.get(),
        "the terminal entry must not run after install failure"
    );
    let diagnostics = diagnostics.borrow();
    assert_eq!(diagnostics.len(), 1);
    assert!(diagnostics[0].starts_with("Linux signal owner installation failed: "));
    assert!(diagnostics[0].len() <= 560);
    assert!(!diagnostics[0].contains('\n'));
}

#[test]
fn setting_accepts_git_but_rejects_the_removed_nextjs_command() {
    let git = Args::try_parse_from(["polaris", "setting", "git"]).unwrap();
    assert!(matches!(git.command, Some(Command::Setting(_))));
    for arguments in [
        vec!["polaris", "setting", "nextjs"],
        vec!["polaris", "setting", "nextjs", "--install"],
    ] {
        let error = Args::try_parse_from(arguments)
            .err()
            .expect("removed subcommand");
        assert_eq!(error.kind(), clap::error::ErrorKind::InvalidSubcommand);
    }
}

#[cfg(unix)]
#[test]
fn ordinary_cli_startup_resolves_pending_rewind_before_execution() {
    use polaris_core::rewind_workspace::{RewindLimits, WorkspaceOwner};
    use polaris_sandbox::{SandboxMode, SandboxPolicy};

    let temp = tempfile::tempdir().unwrap();
    let project = temp.path().join("project");
    let state_dir = temp.path().join("state");
    std::fs::create_dir(&project).unwrap();
    assert!(resolve_pending_workspace_before_execution(&project, &state_dir).is_ok());
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
    let policy =
        SandboxPolicy::new(SandboxMode::WorkspaceWrite, std::slice::from_ref(&project)).unwrap();
    std::fs::write(project.join("file.txt"), b"before").unwrap();
    let pre = owner.capture(&policy).unwrap();
    std::fs::write(project.join("file.txt"), b"after").unwrap();
    let post = owner.capture(&policy).unwrap();
    owner
        .restore_for_branch(&post, &pre, "candidate-session-1234")
        .unwrap();

    let result = resolve_pending_workspace_before_execution(&project, &state_dir);
    assert!(result.unwrap_err().to_string().contains("Recovered files"));
    assert_eq!(std::fs::read(project.join("file.txt")).unwrap(), b"after");
    assert!(resolve_pending_workspace_before_execution(&project, &state_dir).is_ok());
}

#[tokio::test]
async fn resume_and_fork_restore_saved_focus_without_resetting_runtime_settings() {
    use polaris_core::{
        conversation_state::HistoryMode,
        session_store::PersistedSession,
        workflow::{Phase, SessionWorkflow, WorkflowConfig},
    };
    let dir = tempfile::tempdir().unwrap();
    let source_config = WorkflowConfig {
        enabled: true,
        initial_phase: Phase::Review,
        mode: polaris_core::workflow::WorkflowMode::Adaptive,
        ..WorkflowConfig::default()
    };
    let source_workflow = SessionWorkflow::new(source_config);
    let source = PersistedSession::create(
        dir.path(),
        &dir.path().join("memory.sqlite3"),
        "test-project",
        HistoryMode::Legacy,
        Some(&source_workflow),
    )
    .unwrap();
    let child = source.fork(&"a".repeat(64)).unwrap();
    let current_config = WorkflowConfig {
        initial_phase: Phase::Implement,
        ..polaris_core::config::Config::default().workflow
    };
    for (flag, saved) in [("--resume", &source), ("--fork", &child)] {
        let id = saved.snapshot().unwrap().state.session_id;
        let args = Args::try_parse_from(["polaris", flag, &id]).unwrap();
        let mut session = Session {
            workflow: Some(SessionWorkflow::new(current_config.clone())),
            disable_files_md_auto_regenerate: true,
            ..Session::default()
        };
        restore_continuation_workflow(
            args.resume.as_deref(),
            args.fork.as_deref(),
            &mut session,
            saved,
            &current_config,
            None,
            &[],
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            session.workflow.as_ref().unwrap().state().phase,
            Phase::Review,
            "{flag}"
        );
        assert_eq!(
            session.workflow.as_ref().unwrap().config,
            source_workflow.config
        );
        assert!(session.disable_files_md_auto_regenerate);
        let disabled = WorkflowConfig::default();
        restore_continuation_workflow(
            args.resume.as_deref(),
            args.fork.as_deref(),
            &mut session,
            saved,
            &disabled,
            None,
            &[],
            None,
        )
        .await
        .unwrap();
        assert_eq!(
            session.workflow.as_ref().unwrap().config,
            source_workflow.config
        );

        restore_continuation_workflow(
            args.resume.as_deref(),
            args.fork.as_deref(),
            &mut session,
            saved,
            &disabled,
            Some(WorkflowModeArg::Off),
            &[],
            None,
        )
        .await
        .unwrap();
        // F15 is applied only after terminal confirmation by the owner.
        assert!(session.workflow.as_ref().unwrap().config.enabled);
        assert!(
            saved
                .snapshot()
                .unwrap()
                .state
                .workflow_config
                .unwrap()
                .config
                .enabled
        );
    }
}

#[cfg(unix)]
#[test]
fn existing_v2_workflow_override_cancellation_and_noninteractive_do_not_mutate_persistence() {
    use polaris_core::{
        conversation_state::HistoryMode,
        management_effect::ManagementEffectStoreV1,
        project_mutation::ProjectMutationOwnerV1,
        session_store::PersistedSession,
        workflow::{Phase, SessionWorkflow, WorkflowConfig},
    };

    for (label, stdin_is_terminal, stderr_is_terminal, answer, expected) in [
        (
            "cancel",
            true,
            true,
            b"n\n".as_slice(),
            "Workflow change cancelled",
        ),
        (
            "noninteractive",
            false,
            false,
            b"y\n".as_slice(),
            "Changing --workflow-mode for an existing conversation requires confirmation in an interactive terminal",
        ),
    ] {
        let root = tempfile::tempdir().expect("temp root");
        let home = root.path().join("home");
        let project = root.path().join("project");
        let data = root.path().join("data");
        std::fs::create_dir(&home).expect("home");
        std::fs::create_dir(&project).expect("project");
        std::fs::create_dir(project.join(".polaris")).expect("project marker");
        std::fs::create_dir(&data).expect("data");
        with_home(&home, || {
            let state_dir = polaris_core::project::state_dir(&project).expect("state dir");
            let workflow = SessionWorkflow::new(WorkflowConfig {
                enabled: true,
                initial_phase: Phase::Review,
                ..WorkflowConfig::default()
            });
            let saved = PersistedSession::create(
                &data,
                &data.join("memory.sqlite3"),
                "test-project",
                HistoryMode::Legacy,
                Some(&workflow),
            )
            .expect("create saved v2 session");
            let mutation_owner = ProjectMutationOwnerV1::for_project(&project).expect("owner");
            let lease = mutation_owner.try_mutation().expect("lease");
            ManagementEffectStoreV1::open_at_state_dir(&project, &state_dir, &lease)
                .expect("effect store");
            drop(lease);
            let journal = state_dir.join("management-effects-v1/journal.v1.jsonl");
            let management_before = std::fs::read(&journal).expect("management journal");
            let session_id = saved.snapshot().expect("snapshot").state.session_id;
            let session_dir = data.join("sessions-v2").join(session_id);
            let state_before = std::fs::read(session_dir.join("state.json")).expect("state bytes");
            let raw_before = std::fs::read(session_dir.join("raw.jsonl")).expect("raw bytes");
            let snapshot_before = saved.snapshot().expect("snapshot before");
            let workflow_before = workflow.config.clone();
            let mut session = Session {
                workflow: Some(workflow),
                ..Session::default()
            };
            session.attach(saved.clone()).expect("attach saved session");
            let mut input = std::io::Cursor::new(answer);
            let mut output = Vec::new();

            let error = apply_cli_workflow_override_with_confirmation(
                &mut session,
                WorkflowModeArg::Adaptive,
                &[],
                &project,
                &state_dir,
                &data,
                "test-project",
                stdin_is_terminal,
                stderr_is_terminal,
                &mut input,
                &mut output,
            )
            .expect_err("F15 must stop before its owner executes");

            assert_eq!(error, expected, "{label}");
            assert_eq!(
                session.workflow.as_ref().expect("workflow").config,
                workflow_before,
                "{label}"
            );
            let snapshot_after = saved.snapshot().expect("snapshot after");
            assert_eq!(snapshot_after.state, snapshot_before.state, "{label}");
            assert_eq!(
                snapshot_after.raw_offset, snapshot_before.raw_offset,
                "{label}"
            );
            assert_eq!(snapshot_after.raw_hash, snapshot_before.raw_hash, "{label}");
            assert_eq!(
                std::fs::read(session_dir.join("state.json")).unwrap(),
                state_before,
                "{label}"
            );
            assert_eq!(
                std::fs::read(session_dir.join("raw.jsonl")).unwrap(),
                raw_before,
                "{label}"
            );
            assert_eq!(
                std::fs::read(&journal).unwrap(),
                management_before,
                "{label}"
            );
        });
    }
}

#[cfg(unix)]
#[test]
fn existing_v2_workflow_override_confirmation_executes_completed_f15() {
    use polaris_core::{
        conversation_state::HistoryMode,
        management_effect::{ManagementEffectStateV1, ManagementEffectStoreV1},
        project_mutation::ProjectMutationOwnerV1,
        session_store::PersistedSession,
        workflow::{Phase, SessionWorkflow, WorkflowConfig, WorkflowMode},
    };

    let root = tempfile::tempdir().expect("temp root");
    let home = root.path().join("home");
    let project = root.path().join("project");
    let data = root.path().join("data");
    std::fs::create_dir(&home).expect("home");
    std::fs::create_dir(&project).expect("project");
    std::fs::create_dir(project.join(".polaris")).expect("project marker");
    std::fs::create_dir(&data).expect("data");
    let saved_project = project.canonicalize().unwrap().to_str().unwrap().to_owned();
    with_home(&home, || {
        let state_dir = polaris_core::project::state_dir(&project).expect("state dir");
        let workflow = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            initial_phase: Phase::Review,
            ..WorkflowConfig::default()
        });
        let saved = PersistedSession::create(
            &data,
            &data.join("memory.sqlite3"),
            &saved_project,
            HistoryMode::Legacy,
            Some(&workflow),
        )
        .expect("create saved v2 session");
        let mut session = Session {
            workflow: Some(workflow),
            ..Session::default()
        };
        session.attach(saved.clone()).expect("attach saved session");
        let mut input = std::io::Cursor::new(b"y\n");
        let mut output = Vec::new();

        apply_cli_workflow_override_with_confirmation(
            &mut session,
            WorkflowModeArg::Adaptive,
            &[],
            &project,
            &state_dir,
            &data,
            &saved_project,
            true,
            true,
            &mut input,
            &mut output,
        )
        .expect("confirmed F15");

        assert_eq!(
            session.workflow.as_ref().expect("workflow").config.mode,
            WorkflowMode::Adaptive
        );
        assert_eq!(
            saved
                .snapshot()
                .expect("saved snapshot")
                .state
                .workflow_config
                .expect("saved config")
                .config
                .mode,
            WorkflowMode::Adaptive
        );
        let mutation_owner = ProjectMutationOwnerV1::for_project(&project).expect("owner");
        let lease = mutation_owner.try_mutation().expect("lease");
        let effects = ManagementEffectStoreV1::open_at_state_dir(&project, &state_dir, &lease)
            .expect("effect store");
        let effects = effects.effects().expect("effects");
        assert!(matches!(
            effects.as_slice(),
            [effect]
                if effect.request.inventory.encoded() == "F15"
                    && matches!(&effect.state, ManagementEffectStateV1::Completed { .. })
        ));
    });
}

#[test]
fn prepared_continuation_resolves_resume_and_fork_without_fresh_fallback() {
    use polaris_core::{
        conversation_state::HistoryMode,
        session_store::PersistedSession,
        workflow::{Phase, SessionWorkflow, WorkflowConfig},
    };
    let dir = tempfile::tempdir().expect("temp dir");
    let database = dir.path().join("memory.sqlite3");
    let workflow = SessionWorkflow::new(WorkflowConfig {
        enabled: true,
        initial_phase: Phase::Review,
        ..Default::default()
    });
    let source = PersistedSession::create(
        dir.path(),
        &database,
        "project",
        HistoryMode::Legacy,
        Some(&workflow),
    )
    .expect("create source");
    let source_id = source.snapshot().expect("source snapshot").state.session_id;

    let resumed = resolve_continuation(
        Some(&source_id),
        None,
        dir.path(),
        "project",
        None,
        HistoryMode::Legacy,
        &dir.path().join("sessions"),
    )
    .expect("resolve resume")
    .expect("resume target");
    let resumed =
        prepare_continuation(resumed, dir.path(), &database, "project").expect("prepare resume");
    assert_eq!(resumed.kind, polaris_tui::ContinuationKind::Resume);
    assert_eq!(
        resumed.persistence.snapshot().unwrap().state.session_id,
        source_id
    );

    let before_parent = source.snapshot().expect("parent before fork");
    let forked = resolve_continuation(
        None,
        Some(&source_id),
        dir.path(),
        "project",
        None,
        HistoryMode::Legacy,
        &dir.path().join("sessions"),
    )
    .expect("resolve fork")
    .expect("fork target");
    let forked =
        prepare_continuation(forked, dir.path(), &database, "project").expect("prepare fork");
    assert_eq!(forked.kind, polaris_tui::ContinuationKind::Fork);
    assert_ne!(
        forked.persistence.snapshot().unwrap().state.session_id,
        source_id
    );
    let after_parent = source.snapshot().expect("parent after fork");
    assert_eq!(after_parent.state, before_parent.state);
    assert_eq!(after_parent.raw_hash, before_parent.raw_hash);
    assert_eq!(after_parent.raw_offset, before_parent.raw_offset);
    assert_eq!(after_parent.events.len(), before_parent.events.len());
}

#[test]
fn missing_v2_target_can_import_a_valid_legacy_session() {
    use polaris_core::conversation_state::HistoryMode;
    let dir = tempfile::tempdir().expect("temp dir");
    let database = dir.path().join("memory.sqlite3");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir(&sessions).expect("legacy sessions directory");
    let id = "00000000-0000-4000-8000-000000000001";
    polaris_tui::persist::append_message(
        &sessions.join(format!("{id}.jsonl")),
        &polaris_provider::Message::user("legacy turn"),
    )
    .expect("legacy fixture");

    let resolved = resolve_continuation(
        Some(id),
        None,
        dir.path(),
        "project",
        None,
        HistoryMode::Legacy,
        &sessions,
    )
    .expect("resolve legacy")
    .expect("legacy target");
    let prepared =
        prepare_continuation(resolved, dir.path(), &database, "project").expect("import legacy");
    assert_eq!(prepared.kind, polaris_tui::ContinuationKind::Resume);
    assert_eq!(
        prepared.persistence.snapshot().unwrap().events[0]
            .message
            .content,
        "legacy turn"
    );
}

#[test]
fn resolver_rejects_corrupt_legacy_and_incomplete_v2_without_repairing() {
    use polaris_core::conversation_state::{ConversationStateV2, ConversationStore, HistoryMode};
    let dir = tempfile::tempdir().expect("temp dir");
    let database = dir.path().join("memory.sqlite3");
    let sessions = dir.path().join("sessions");
    std::fs::create_dir(&sessions).expect("legacy sessions directory");
    let corrupt_id = "00000000-0000-4000-8000-000000000001";
    let corrupt = sessions.join(format!("{corrupt_id}.jsonl"));
    std::fs::write(&corrupt, b"{not json}\n").expect("corrupt fixture");
    let corrupt_before = std::fs::read(&corrupt).unwrap();
    assert!(
        resolve_continuation(
            Some(corrupt_id),
            None,
            dir.path(),
            "project",
            None,
            HistoryMode::Legacy,
            &sessions,
        )
        .is_err()
    );
    assert_eq!(std::fs::read(&corrupt).unwrap(), corrupt_before);
    assert!(!database.exists());

    let id = "00000000-0000-4000-8000-000000000002";
    let state = ConversationStateV2::new("project".into(), id.into()).expect("state");
    let store = ConversationStore::create(dir.path(), state).expect("v2 fixture");
    std::fs::remove_file(store.directory().join("writer.lock")).expect("remove lock fixture");
    let colliding = sessions.join(format!("{id}.jsonl"));
    polaris_tui::persist::append_message(
        &colliding,
        &polaris_provider::Message::user("must not import"),
    )
    .expect("collision fixture");
    let state_before = std::fs::read(store.directory().join("state.json")).unwrap();
    let raw_before = std::fs::read(store.directory().join("raw.jsonl")).unwrap();
    let colliding_before = std::fs::read(&colliding).unwrap();
    assert!(
        resolve_continuation(
            Some(id),
            None,
            dir.path(),
            "project",
            None,
            HistoryMode::Legacy,
            &sessions,
        )
        .is_err()
    );
    assert_eq!(
        std::fs::read(store.directory().join("state.json")).unwrap(),
        state_before
    );
    assert_eq!(
        std::fs::read(store.directory().join("raw.jsonl")).unwrap(),
        raw_before
    );
    assert_eq!(std::fs::read(&colliding).unwrap(), colliding_before);
    assert!(!database.exists());
}

#[test]
fn resolver_does_not_open_missing_or_old_database() {
    use polaris_core::conversation_state::{ConversationStateV2, ConversationStore, HistoryMode};
    let dir = tempfile::tempdir().expect("temp dir");
    let database = dir.path().join("memory.sqlite3");
    let id = "00000000-0000-4000-8000-000000000003";
    let state = ConversationStateV2::new("project".into(), id.into()).expect("state");
    ConversationStore::create(dir.path(), state).expect("v2 fixture");

    let missing = resolve_continuation(
        Some(id),
        None,
        dir.path(),
        "project",
        None,
        HistoryMode::Legacy,
        &dir.path().join("sessions"),
    )
    .expect("resolve without database")
    .expect("target");
    assert_eq!(missing.history_mode, HistoryMode::Legacy);
    assert!(!database.exists());

    std::fs::write(&database, b"not a sqlite database").expect("old database fixture");
    let before = std::fs::read(&database).unwrap();
    assert!(
        resolve_continuation(
            Some(id),
            None,
            dir.path(),
            "project",
            None,
            HistoryMode::Legacy,
            &dir.path().join("sessions"),
        )
        .is_ok()
    );
    assert_eq!(std::fs::read(&database).unwrap(), before);
}

#[test]
fn rejected_continuation_targets_do_not_create_a_fresh_session() {
    use polaris_core::{
        conversation_state::{ConversationStateV2, ConversationStore, HistoryMode},
        session_store::PersistedSession,
    };
    let dir = tempfile::tempdir().expect("temp dir");
    let database = dir.path().join("memory.sqlite3");
    let source = PersistedSession::create(
        dir.path(),
        &database,
        "other-project",
        HistoryMode::Legacy,
        None,
    )
    .expect("create foreign source");
    let foreign_id = source.snapshot().unwrap().state.session_id;
    let foreign_dir = dir.path().join("sessions-v2").join(&foreign_id);
    let foreign_state_before = std::fs::read(foreign_dir.join("state.json")).expect("state");
    let foreign_raw_before = std::fs::read(foreign_dir.join("raw.jsonl")).expect("raw");
    let legacy_dir = dir.path().join("sessions");
    std::fs::create_dir(&legacy_dir).expect("legacy sessions directory");
    let colliding_legacy = legacy_dir.join(format!("{foreign_id}.jsonl"));
    polaris_tui::persist::append_message(
        &colliding_legacy,
        &polaris_provider::Message::user("legacy collision"),
    )
    .expect("legacy collision fixture");
    let legacy_before = std::fs::read(&colliding_legacy).expect("legacy bytes");
    let allowed_id = "b".repeat(128);
    let allowed_legacy = legacy_dir.join(format!("{allowed_id}.jsonl"));
    polaris_tui::persist::append_message(
        &allowed_legacy,
        &polaris_provider::Message::user("allowed legacy"),
    )
    .expect("128-byte legacy fixture");
    let allowed_before = std::fs::read(&allowed_legacy).expect("allowed bytes");
    let rejected_id = "a".repeat(129);
    let rejected_legacy = legacy_dir.join(format!("{rejected_id}.jsonl"));
    polaris_tui::persist::append_message(
        &rejected_legacy,
        &polaris_provider::Message::user("rejected legacy"),
    )
    .expect("129-byte legacy fixture");
    let rejected_before = std::fs::read(&rejected_legacy).expect("rejected bytes");
    let traversal_legacy = dir.path().join("traversal.jsonl");
    polaris_tui::persist::append_message(
        &traversal_legacy,
        &polaris_provider::Message::user("traversal legacy"),
    )
    .expect("traversal legacy fixture");
    let traversal_before = std::fs::read(&traversal_legacy).expect("traversal bytes");
    let bytes_before = std::fs::read(&database).expect("database bytes");
    let sessions_before = std::fs::read_dir(dir.path()).expect("data root").count();
    let v2_before = std::fs::read_dir(dir.path().join("sessions-v2"))
        .expect("v2 sessions")
        .count();
    assert!(
        resolve_continuation(
            Some(&allowed_id),
            None,
            dir.path(),
            "project",
            None,
            HistoryMode::Legacy,
            &legacy_dir,
        )
        .is_ok()
    );
    for id in [
        "missing".to_string(),
        foreign_id,
        "../traversal".to_string(),
        rejected_id,
    ] {
        assert!(
            resolve_continuation(
                Some(&id),
                None,
                dir.path(),
                "project",
                None,
                HistoryMode::Legacy,
                &dir.path().join("sessions"),
            )
            .is_err()
        );
    }
    assert_eq!(std::fs::read(&database).unwrap(), bytes_before);
    assert_eq!(
        std::fs::read_dir(dir.path()).unwrap().count(),
        sessions_before
    );
    assert_eq!(
        std::fs::read_dir(dir.path().join("sessions-v2"))
            .unwrap()
            .count(),
        v2_before
    );
    assert_eq!(
        std::fs::read(foreign_dir.join("state.json")).unwrap(),
        foreign_state_before
    );
    assert_eq!(
        std::fs::read(foreign_dir.join("raw.jsonl")).unwrap(),
        foreign_raw_before
    );
    assert_eq!(std::fs::read(&colliding_legacy).unwrap(), legacy_before);
    assert_eq!(std::fs::read(&allowed_legacy).unwrap(), allowed_before);
    assert_eq!(std::fs::read(&rejected_legacy).unwrap(), rejected_before);
    assert_eq!(std::fs::read(&traversal_legacy).unwrap(), traversal_before);

    let structured_id = polaris_core::session_store::new_session_id().expect("id");
    let mut state = ConversationStateV2::new("project".into(), structured_id.clone())
        .expect("structured state");
    state.history_mode = HistoryMode::Structured10;
    ConversationStore::create(dir.path(), state).expect("write structured fixture");
    let raw_before = std::fs::read(
        dir.path()
            .join("sessions-v2")
            .join(&structured_id)
            .join("raw.jsonl"),
    )
    .expect("structured raw");
    let state_before = std::fs::read(
        dir.path()
            .join("sessions-v2")
            .join(&structured_id)
            .join("state.json"),
    )
    .expect("structured state");
    let database_before_structured = std::fs::read(&database).expect("database bytes");
    // An explicit structured target resolves without writing anything.
    assert_eq!(
        resolve_continuation(
            Some(&structured_id),
            None,
            dir.path(),
            "project",
            None,
            HistoryMode::Legacy,
            &dir.path().join("sessions"),
        )
        .expect("structured target")
        .expect("continuation")
        .history_mode,
        HistoryMode::Structured10
    );
    assert_eq!(
        std::fs::read(
            dir.path()
                .join("sessions-v2")
                .join(&structured_id)
                .join("raw.jsonl"),
        )
        .unwrap(),
        raw_before
    );
    assert_eq!(
        std::fs::read(
            dir.path()
                .join("sessions-v2")
                .join(&structured_id)
                .join("state.json"),
        )
        .unwrap(),
        state_before
    );
    assert_eq!(
        std::fs::read(&database).unwrap(),
        database_before_structured
    );
}

#[test]
fn saved_history_mode_overrides_config_and_explicit_conflicts_do_not_write() {
    use polaris_core::{conversation_state::HistoryMode, session_store::PersistedSession};
    let dir = tempfile::tempdir().expect("temp dir");
    let database = dir.path().join("memory.sqlite3");
    let legacy =
        PersistedSession::create(dir.path(), &database, "project", HistoryMode::Legacy, None)
            .expect("legacy source");
    let strict = PersistedSession::create(
        dir.path(),
        &database,
        "project",
        HistoryMode::Strict10,
        None,
    )
    .expect("strict source");
    let legacy_id = legacy.snapshot().unwrap().state.session_id;
    let strict_id = strict.snapshot().unwrap().state.session_id;
    let strict_resume = resolve_continuation(
        Some(&strict_id),
        None,
        dir.path(),
        "project",
        None,
        HistoryMode::Legacy,
        &dir.path().join("sessions"),
    )
    .unwrap()
    .unwrap();
    assert_eq!(strict_resume.history_mode, HistoryMode::Strict10);
    let strict_fork = resolve_continuation(
        None,
        Some(&strict_id),
        dir.path(),
        "project",
        None,
        HistoryMode::Legacy,
        &dir.path().join("sessions"),
    )
    .unwrap()
    .unwrap();
    assert_eq!(strict_fork.history_mode, HistoryMode::Strict10);
    let legacy_resume = resolve_continuation(
        Some(&legacy_id),
        None,
        dir.path(),
        "project",
        None,
        HistoryMode::Strict10,
        &dir.path().join("sessions"),
    )
    .unwrap()
    .unwrap();
    assert_eq!(legacy_resume.history_mode, HistoryMode::Legacy);
    let legacy_fork = resolve_continuation(
        None,
        Some(&legacy_id),
        dir.path(),
        "project",
        None,
        HistoryMode::Strict10,
        &dir.path().join("sessions"),
    )
    .unwrap()
    .unwrap();
    assert_eq!(legacy_fork.history_mode, HistoryMode::Legacy);

    let database_before = std::fs::read(&database).expect("database bytes");
    let sessions_before = std::fs::read_dir(dir.path().join("sessions-v2"))
        .expect("v2 sessions")
        .count();
    for (resume, fork, requested) in [
        (Some(legacy_id.as_str()), None, HistoryMode::Strict10),
        (None, Some(legacy_id.as_str()), HistoryMode::Strict10),
        (Some(strict_id.as_str()), None, HistoryMode::Legacy),
        (None, Some(strict_id.as_str()), HistoryMode::Legacy),
    ] {
        assert!(
            resolve_continuation(
                resume,
                fork,
                dir.path(),
                "project",
                Some(requested),
                HistoryMode::Legacy,
                &dir.path().join("sessions"),
            )
            .is_err()
        );
    }
    assert_eq!(std::fs::read(&database).unwrap(), database_before);
    assert_eq!(
        std::fs::read_dir(dir.path().join("sessions-v2"))
            .unwrap()
            .count(),
        sessions_before
    );
    assert_eq!(legacy.snapshot().unwrap().state.session_id, legacy_id);
    assert_eq!(strict.snapshot().unwrap().state.session_id, strict_id);
}

use super::*;

fn routine_project() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("crate-a/src")).unwrap();
    std::fs::write(
        directory.path().join("Cargo.toml"),
        "[workspace]\nmembers = [\"crate-a\"]\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("crate-a/Cargo.toml"),
        "[package]\nname = \"crate-a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("crate-a/src/lib.rs"),
        "pub fn value() {}\n",
    )
    .unwrap();
    let status = std::process::Command::new(resolve_cargo_program().unwrap())
        .current_dir(directory.path())
        .args(["generate-lockfile", "--offline"])
        .status()
        .unwrap();
    assert!(status.success());
    directory
}

#[cfg(unix)]
fn routine_path_dependency_project() -> tempfile::TempDir {
    let directory = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(directory.path().join("crate-a/src")).unwrap();
    std::fs::create_dir_all(directory.path().join("crate-b/src")).unwrap();
    std::fs::write(
        directory.path().join("Cargo.toml"),
        "[workspace]\nresolver = \"3\"\nmembers = [\"crate-a\", \"crate-b\"]\n",
    )
    .unwrap();
    std::fs::write(
            directory.path().join("crate-a/Cargo.toml"),
            "[package]\nname = \"crate-a\"\nversion = \"0.1.0\"\nedition = \"2024\"\n\n[dependencies]\ncrate-b = { path = \"../crate-b\" }\n",
        )
        .unwrap();
    std::fs::write(
        directory.path().join("crate-a/src/lib.rs"),
        "pub fn value() -> usize { crate_b::value() }\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("crate-b/Cargo.toml"),
        "[package]\nname = \"crate-b\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )
    .unwrap();
    std::fs::write(
        directory.path().join("crate-b/src/lib.rs"),
        "pub fn value() -> usize { 1 }\n",
    )
    .unwrap();
    let status = std::process::Command::new(resolve_cargo_program().unwrap())
        .current_dir(directory.path())
        .args(["generate-lockfile", "--offline"])
        .status()
        .unwrap();
    assert!(status.success());
    directory
}

#[cfg(unix)]
fn static_owner_for_path_dependency_project(
    project: &Path,
    session_id: &str,
) -> (
    polaris_core::session_store::PersistedSession,
    polaris_core::verification::SessionVerificationToolOwnerV1,
) {
    use polaris_core::{
        conversation_state::HistoryMode,
        session_store::PersistedSession,
        workflow::{ChangeContractV1, Phase, SessionWorkflow, WorkflowConfig},
    };

    // This helper tests graph binding, not the production analyzer. The unit
    // test executable runs the test harness rather than the CLI subcommand.
    let analyzer = project.join("crate-a/static-analyzer-fixture");
    let analyzer_bytes = b"#!/bin/sh\nprintf '%s\\n' '{\"schema_version\":1,\"state\":\"satisfied\",\"findings\":[],\"error_kind\":null}'\n";
    std::fs::write(&analyzer, analyzer_bytes).unwrap();
    std::fs::set_permissions(
        &analyzer,
        <std::fs::Permissions as std::os::unix::fs::PermissionsExt>::from_mode(0o500),
    )
    .unwrap();

    let sandbox = build_sandbox_policy(project, SandboxMode::WorkspaceWrite).unwrap();
    let prompt = "Validate the component manifest.";
    let mut prepared =
        prepare_routine_authority(&static_authority_json(), prompt, project, &sandbox, project)
            .unwrap();
    for adapter in &mut prepared.static_adapters {
        adapter.program = analyzer.canonicalize().unwrap();
        adapter.capability.executable_hash =
            polaris_core::conversation_state::content_hash(analyzer_bytes);
    }
    let workflow = SessionWorkflow::new(WorkflowConfig {
        enabled: true,
        initial_phase: Phase::Specify,
        ..WorkflowConfig::default()
    });
    let saved = PersistedSession::create(
        project,
        &project.join(format!("{session_id}.sqlite3")),
        session_id,
        HistoryMode::Legacy,
        Some(&workflow),
    )
    .unwrap();
    let contract = ChangeContractV1::draft_typed(
        format!("change-{session_id}"),
        prepared.eligibility.clone(),
        prompt,
        "The component manifest may be invalid.",
        "The component manifest is valid.",
        prepared.verification.clone(),
    )
    .unwrap();
    saved
        .publish_and_approve_routine_contract(workflow.state().revision, contract)
        .unwrap();
    let observer = Arc::new(CliVerificationObserverV1 {
        project_root: project.canonicalize().unwrap(),
        component_root: prepared.component_root.clone(),
        subject_hash: prepared.subject_hash.clone(),
        cargo_program: resolve_cargo_program().unwrap(),
    });
    let owner = polaris_core::verification::SessionVerificationToolOwnerV1::new(
        saved.clone(),
        observer,
        prepared.runners,
    )
    .unwrap()
    .with_preparation(prepared.verification_preparation)
    .unwrap()
    .with_static_adapters(prepared.static_adapters)
    .unwrap();
    (saved, owner)
}

fn routine_authority_json(program: &str) -> String {
    serde_json::json!({
        "schema_version": 1,
        "component_root": "crate-a",
        "security": false,
        "migration": false,
        "public_api": false,
        "dependency": false,
        "distribution": false,
        "major_user_contract": false,
        "unknown_impact": false,
        "criteria": [{
            "description": "The focused case passes.",
            "observable_failure": "The focused case fails.",
            "evidence_kind": "common_case",
            "common_runner_plan": {
                "version": 1,
                "argv": [program],
                "stdin": null,
                "cases": [{"case":"exits_with","status":0}]
            },
            "timeout_secs": 5
        }]
    })
    .to_string()
}

fn existing_test_authority_json(program: Option<&str>) -> String {
    use polaris_core::verification::{TestCostClassV1, TestEntryV1, TestInventoryV1};
    let inventory = TestInventoryV1::complete_rust(vec![TestEntryV1 {
        id: "rust-crate-a-lib-focused".into(),
        framework: "rust".into(),
        package: "crate-a".into(),
        target: "lib".into(),
        exact_name: "tests::focused".into(),
        source_location: None,
        dynamic_or_unknown: false,
        cost: TestCostClassV1::Fast,
        symbols: Default::default(),
        common_case_ids: Default::default(),
    }])
    .unwrap();
    let mut value = serde_json::json!({
        "schema_version": 1,
        "component_root": "crate-a",
        "security": false,
        "migration": false,
        "public_api": false,
        "dependency": false,
        "distribution": false,
        "major_user_contract": false,
        "unknown_impact": false,
        "criteria": [{
            "description": "The registered Rust test passes.",
            "observable_failure": "The registered Rust test fails.",
            "evidence_kind": "existing_test"
        }]
    });
    if let Some(program) = program {
        value["existing_test"] = serde_json::json!({
            "cargo_program": program,
            "timeout_secs": 30,
            "inventory": inventory
        });
    }
    value.to_string()
}

fn static_authority_json() -> String {
    serde_json::json!({
        "schema_version": 1,
        "component_root": "crate-a",
        "security": false,
        "migration": false,
        "public_api": false,
        "dependency": false,
        "distribution": false,
        "major_user_contract": false,
        "unknown_impact": false,
        "criteria": [{
            "description": STATIC_COMPONENT_MANIFEST_DESCRIPTION,
            "observable_failure": STATIC_COMPONENT_MANIFEST_FAILURE,
            "evidence_kind": "static"
        }]
    })
    .to_string()
}

fn review_authority_json(revision: u64, checkpoint_id: &str) -> String {
    serde_json::json!({
        "schema_version": 1,
        "expected_workflow_revision": revision,
        "checkpoint_id": checkpoint_id,
        "reviewer_id": "cli-review-owner",
        "findings": [{
            "finding_id": "finding-1",
            "disposition": "resolved",
            "resolution_ref": "change:focused-fix"
        }]
    })
    .to_string()
}

#[test]
fn m27_review_authority_is_strict_and_requires_closed_dispositions() {
    let valid = review_authority_json(7, "checkpoint-1");
    assert!(prepare_review_authority(&valid).is_ok());

    let mut unknown: serde_json::Value = serde_json::from_str(&valid).unwrap();
    unknown["binding"] = serde_json::json!({"source_hash":"untrusted"});
    assert!(prepare_review_authority(&unknown.to_string()).is_err());

    let mut open: serde_json::Value = serde_json::from_str(&valid).unwrap();
    open["findings"][0]["disposition"] = serde_json::json!("open");
    open["findings"][0]
        .as_object_mut()
        .unwrap()
        .remove("resolution_ref");
    assert!(prepare_review_authority(&open.to_string()).is_err());

    let mut missing_ref: serde_json::Value = serde_json::from_str(&valid).unwrap();
    missing_ref["findings"][0]
        .as_object_mut()
        .unwrap()
        .remove("resolution_ref");
    assert!(prepare_review_authority(&missing_ref.to_string()).is_err());
}

#[test]
fn m27_routine_authority_is_strict_and_builds_one_host_fixed_component() {
    let project = routine_project();
    let sandbox = build_sandbox_policy(project.path(), SandboxMode::WorkspaceWrite).unwrap();
    let prepared = prepare_routine_authority(
        &routine_authority_json("/usr/bin/true"),
        "Fix the focused case.",
        project.path(),
        &sandbox,
        project.path(),
    )
    .unwrap();
    assert_eq!(prepared.eligibility.affected_components, ["crate-a"]);
    assert_eq!(prepared.verification.len(), 1);
    assert_eq!(prepared.runners.len(), 1);

    let mut value: serde_json::Value =
        serde_json::from_str(&routine_authority_json("/usr/bin/true")).unwrap();
    value["unexpected"] = serde_json::json!(true);
    assert!(
        prepare_routine_authority(
            &value.to_string(),
            "Fix the focused case.",
            project.path(),
            &sandbox,
            project.path(),
        )
        .is_err()
    );
}

#[test]
fn m27_routine_authority_refuses_cross_component_or_non_routine_scope() {
    let project = routine_project();
    let sandbox = build_sandbox_policy(project.path(), SandboxMode::WorkspaceWrite).unwrap();
    for (field, value) in [
        ("component_root", serde_json::json!(".")),
        ("public_api", serde_json::json!(true)),
    ] {
        let mut authority: serde_json::Value =
            serde_json::from_str(&routine_authority_json("/usr/bin/true")).unwrap();
        authority[field] = value;
        assert!(
            prepare_routine_authority(
                &authority.to_string(),
                "Fix the focused case.",
                project.path(),
                &sandbox,
                project.path(),
            )
            .is_err()
        );
    }
}

#[test]
fn m27_routine_authority_fails_closed_when_fixed_runner_is_absent() {
    use polaris_core::{
        conversation_state::HistoryMode,
        session_store::PersistedSession,
        workflow::{Phase, SessionWorkflow, WorkflowConfig},
    };
    let project = routine_project();
    let sandbox = build_sandbox_policy(project.path(), SandboxMode::WorkspaceWrite).unwrap();
    let prompt = "Fix the focused case.";
    let prepared = prepare_routine_authority(
        &routine_authority_json("/path/that/does/not/exist"),
        prompt,
        project.path(),
        &sandbox,
        project.path(),
    )
    .unwrap();
    let workflow = SessionWorkflow::new(WorkflowConfig {
        enabled: true,
        initial_phase: Phase::Specify,
        ..WorkflowConfig::default()
    });
    let persistence = PersistedSession::create(
        project.path(),
        &project.path().join("memory.sqlite3"),
        "routine-test",
        HistoryMode::Legacy,
        Some(&workflow),
    )
    .unwrap();
    let mut session = Session {
        workflow: Some(workflow),
        ..Session::default()
    };
    session.attach(persistence).unwrap();
    assert!(install_routine_authority(&mut session, prompt, prepared, project.path()).is_err());
    assert!(session.verification_owner.is_none());
}

#[test]
fn m27_routine_authority_resume_reuses_matching_durable_contract_and_owner() {
    use polaris_core::{
        conversation_state::HistoryMode,
        session_store::PersistedSession,
        workflow::{ChangeContractV1, Phase, SessionWorkflow, WorkflowConfig},
    };
    let project = routine_project();
    let sandbox = build_sandbox_policy(project.path(), SandboxMode::WorkspaceWrite).unwrap();
    let prompt = "Fix the focused case.";
    let prepared = prepare_routine_authority(
        &routine_authority_json("/usr/bin/true"),
        prompt,
        project.path(),
        &sandbox,
        project.path(),
    )
    .unwrap();
    let workflow = SessionWorkflow::new(WorkflowConfig {
        enabled: true,
        initial_phase: Phase::Specify,
        ..WorkflowConfig::default()
    });
    let persistence = PersistedSession::create(
        project.path(),
        &project.path().join("memory.sqlite3"),
        "routine-resume-test",
        HistoryMode::Legacy,
        Some(&workflow),
    )
    .unwrap();
    let contract = ChangeContractV1::draft_typed(
        "change-routine-resume",
        prepared.eligibility.clone(),
        prompt,
        "A bounded defect remains.",
        "The bounded defect is fixed.",
        prepared.verification.clone(),
    )
    .unwrap();
    let restored = persistence
        .publish_and_approve_routine_contract(workflow.state().revision, contract)
        .unwrap();
    let mut session = Session {
        workflow: Some(restored),
        ..Session::default()
    };
    session.attach(persistence).unwrap();
    install_routine_authority(&mut session, prompt, prepared, project.path()).unwrap();
    assert!(session.routine_change_authorization.is_none());
    assert!(session.verification_owner.is_some());
}

#[test]
fn m27_existing_test_requires_and_installs_one_host_executor() {
    let project = routine_project();
    let sandbox = build_sandbox_policy(project.path(), SandboxMode::WorkspaceWrite).unwrap();
    let prompt = "Fix the registered Rust behavior.";
    assert!(
        prepare_routine_authority(
            &existing_test_authority_json(None),
            prompt,
            project.path(),
            &sandbox,
            project.path(),
        )
        .is_err()
    );
    let prepared = prepare_routine_authority(
        &existing_test_authority_json(Some("/usr/bin/true")),
        prompt,
        project.path(),
        &sandbox,
        project.path(),
    )
    .unwrap();
    assert!(prepared.runners.is_empty());
    assert!(prepared.existing_test_executor.is_some());
    assert!(prepared.verification_preparation.inventory.is_some());
    assert_eq!(
        prepared.verification[0].evidence_kinds,
        [polaris_core::workflow::CriterionEvidenceKindV1::ExistingTest]
            .into_iter()
            .collect()
    );
}

#[test]
fn m26_static_authority_registers_only_the_fixed_component_manifest_adapter() {
    let project = routine_project();
    let sandbox = build_sandbox_policy(project.path(), SandboxMode::WorkspaceWrite).unwrap();
    let prepared = prepare_routine_authority(
        &static_authority_json(),
        "Validate the component manifest.",
        project.path(),
        &sandbox,
        project.path(),
    )
    .unwrap();
    assert!(prepared.runners.is_empty());
    assert!(prepared.existing_test_executor.is_none());
    assert_eq!(prepared.static_adapters.len(), 1);
    assert_eq!(
        prepared.verification[0].evidence_kinds,
        [polaris_core::workflow::CriterionEvidenceKindV1::Static]
            .into_iter()
            .collect()
    );

    let mut arbitrary: serde_json::Value = serde_json::from_str(&static_authority_json()).unwrap();
    arbitrary["criteria"][0]["description"] = serde_json::json!("Any arbitrary claim.");
    assert!(
        prepare_routine_authority(
            &arbitrary.to_string(),
            "Validate anything.",
            project.path(),
            &sandbox,
            project.path(),
        )
        .is_err()
    );
    let mut partial: serde_json::Value = serde_json::from_str(&static_authority_json()).unwrap();
    partial["criteria"][0]["timeout_secs"] = serde_json::json!(5);
    assert!(
        prepare_routine_authority(
            &partial.to_string(),
            "Validate the manifest.",
            project.path(),
            &sandbox,
            project.path(),
        )
        .is_err()
    );
}

#[test]
#[cfg(unix)]
fn cargo_lock_change_after_prepare_rejects_the_static_run() {
    use polaris_core::verification::VerificationToolOwnerV1;

    let project = routine_path_dependency_project();
    let (saved, owner) =
        static_owner_for_path_dependency_project(project.path(), "static-lock-binding");
    let prepared = owner.prepare().unwrap();
    let generation = saved.snapshot().unwrap().state.generation;
    saved
        .publish_verification_checkpoint(generation, prepared.clone())
        .unwrap();
    let lock = project.path().join("Cargo.lock");
    let mut contents = std::fs::read_to_string(&lock).unwrap();
    contents.push_str("\n# binding changed\n");
    std::fs::write(lock, contents).unwrap();
    assert!(owner.begin_run(&prepared.checkpoint_id).is_err());
}

#[test]
#[cfg(unix)]
fn sibling_path_dependency_change_after_run_is_unknown_and_cannot_publish() {
    use polaris_core::verification::{VerificationCheckpointStageV1, VerificationToolOwnerV1};

    let project = routine_path_dependency_project();
    let (saved, owner) =
        static_owner_for_path_dependency_project(project.path(), "static-path-binding");
    let prepared = owner.prepare().unwrap();
    let generation = saved.snapshot().unwrap().state.generation;
    saved
        .publish_verification_checkpoint(generation, prepared.clone())
        .unwrap();
    owner.begin_run(&prepared.checkpoint_id).unwrap();
    std::fs::write(
        project.path().join("crate-b/src/lib.rs"),
        "pub fn value() -> usize { 2 }\n",
    )
    .unwrap();
    let observed = owner.poll_run(&prepared.checkpoint_id).unwrap();
    assert_eq!(
        observed.stage,
        VerificationCheckpointStageV1::OutcomeUnknown
    );
    assert!(owner.publish(&prepared.checkpoint_id).is_err());
}

#[test]
fn m27_existing_test_executor_identity_change_is_rejected_after_prepare() {
    use polaris_core::{
        conversation_state::HistoryMode,
        session_store::PersistedSession,
        workflow::{ChangeContractV1, Phase, SessionWorkflow, WorkflowConfig},
    };
    let project = routine_project();
    let sandbox = build_sandbox_policy(project.path(), SandboxMode::WorkspaceWrite).unwrap();
    let prompt = "Fix the registered Rust behavior.";
    let first = prepare_routine_authority(
        &existing_test_authority_json(Some("/usr/bin/true")),
        prompt,
        project.path(),
        &sandbox,
        project.path(),
    )
    .unwrap();
    let workflow = SessionWorkflow::new(WorkflowConfig {
        enabled: true,
        initial_phase: Phase::Specify,
        ..WorkflowConfig::default()
    });
    let persistence = PersistedSession::create(
        project.path(),
        &project.path().join("memory.sqlite3"),
        "existing-test-identity",
        HistoryMode::Legacy,
        Some(&workflow),
    )
    .unwrap();
    let contract = ChangeContractV1::draft_typed(
        "change-existing-test-identity",
        first.eligibility.clone(),
        prompt,
        "A registered behavior regressed.",
        "The registered behavior is restored.",
        first.verification.clone(),
    )
    .unwrap();
    let restored = persistence
        .publish_and_approve_routine_contract(workflow.state().revision, contract)
        .unwrap();
    let mut first_session = Session {
        workflow: Some(restored.clone()),
        ..Session::default()
    };
    first_session.attach(persistence.clone()).unwrap();
    install_routine_authority(&mut first_session, prompt, first, project.path()).unwrap();
    let checkpoint = first_session
        .verification_owner
        .as_ref()
        .unwrap()
        .prepare()
        .unwrap();

    let second = prepare_routine_authority(
        &existing_test_authority_json(Some("/bin/echo")),
        prompt,
        project.path(),
        &sandbox,
        project.path(),
    )
    .unwrap();
    let mut resumed = Session {
        workflow: Some(restored),
        ..Session::default()
    };
    resumed.attach(persistence).unwrap();
    install_routine_authority(&mut resumed, prompt, second, project.path()).unwrap();
    assert!(
        resumed
            .verification_owner
            .as_ref()
            .unwrap()
            .begin_run(&checkpoint.checkpoint_id)
            .is_err()
    );
}

#[test]
fn default_provider_prefers_openai_when_its_key_is_available() {
    assert_eq!(default_provider_name(true, true), "openai");
    assert_eq!(default_provider_name(true, false), "openai");
}

#[test]
fn default_provider_falls_back_to_saved_codex_credentials() {
    // The bug this pins down: signing in with ChatGPT through
    // onboarding saves `~/.polaris/auth.json` but nothing sets
    // `POLARIS_PROVIDER`. Without this fallback, the next launch
    // defaults straight back to "openai", finds no key, and reopens
    // onboarding even though the user already signed in.
    assert_eq!(default_provider_name(false, true), "codex");
}

#[test]
fn defaults_and_explicit_model_effort_have_the_documented_precedence() {
    let args = Args::try_parse_from(["polaris", "-p", "hello"]).expect("args");
    assert_eq!(selected_model(args.model, None, false), "gpt-6-astra");
    assert_eq!(selected_model(None, None, true), "gpt-6.1-sol");
    assert_eq!(
        selected_model(None, None, true),
        polaris_tui::render::RECOMMENDED_MODEL
    );
    // One-shot automation retains its explicit historical model/effort.
    // Delegations are priced by `[tier.*]`, which is where the cheaper
    // models live; the root run is the one party whose own reasoning
    // nothing downstream can make up for.
    assert_eq!(args.effort, None);
    assert_eq!(selected_effort(None, true), Some("xhigh"));
    assert_eq!(selected_effort(None, false), None);
    // The TUI marks a row "(default)" by display name, so the row it
    // marks has to be the one this default actually selects.
    assert_eq!(
        polaris_tui::render::effort_wire_value(polaris_tui::render::DEFAULT_EFFORT),
        "xhigh"
    );
    for interactive in [false, true] {
        assert_eq!(
            selected_model(None, Some("env-model".into()), interactive),
            "env-model"
        );
        assert_eq!(
            selected_model(
                Some("flag-model".into()),
                Some("env-model".into()),
                interactive
            ),
            "flag-model"
        );
    }
    let args = Args::try_parse_from(["polaris", "--effort", "high"]).expect("args");
    assert_eq!(args.effort.as_deref(), Some("high"));
    // `max` is real on `/responses`; `ultra` is not an effort the API
    // has, and offering it only meant sending something else instead.
    assert_eq!(
        Args::try_parse_from(["polaris", "--effort", "max"])
            .expect("args")
            .effort
            .as_deref(),
        Some("max")
    );
    assert!(Args::try_parse_from(["polaris", "--effort", "ultra"]).is_err());
}

#[test]
fn workflow_history_and_durable_session_flags_parse_with_their_contract() {
    let args = Args::try_parse_from([
        "polaris",
        "--phase",
        "implement",
        "--history-mode",
        "strict10",
        "-p",
        "continue",
    ])
    .expect("v0.11 flags");
    assert_eq!(args.phase, Some(PhaseArg::Implement));
    assert_eq!(args.history_mode, Some(HistoryModeArg::Strict10));
    assert!(args.resume.is_none());
    assert!(
        Args::try_parse_from([
            "polaris", "--resume", "one", "--fork", "two", "-p", "continue",
        ])
        .is_err()
    );
    for flag in ["--resume", "--fork"] {
        assert!(
            Args::try_parse_from([
                "polaris",
                flag,
                "00000000-0000-4000-8000-000000000001",
                "--phase",
                "review",
            ])
            .is_err()
        );
    }
    let structured = Args::try_parse_from([
        "polaris",
        "--history-mode",
        "structured10",
        "-p",
        "continue",
    ])
    .unwrap();
    assert_eq!(structured.history_mode, Some(HistoryModeArg::Structured10));
}

#[test]
fn default_provider_falls_back_to_openai_when_neither_credential_exists() {
    // openai is the arm that owns onboarding; codex's own arm just
    // fails lazily on first use. With nothing saved anywhere, we want
    // onboarding to fire, so this must stay "openai".
    assert_eq!(default_provider_name(false, false), "openai");
}

#[test]
fn confined_read_parses_without_prompt_and_conflicts_with_mutation() {
    let args = Args::try_parse_from(["polaris", "--confined-read"]).unwrap();
    assert!(args.confined_read);
    assert!(Args::try_parse_from(["polaris", "--confined-read", "--confined-apply"]).is_err());
}

#[test]
fn confined_apply_parses_without_a_prompt() {
    // --confined-apply is the path that reads a mutation operation from
    // stdin, and doesn't need an instruction. If this stayed required,
    // nobody could reach the helper entry point Task 7 built.
    let args = Args::try_parse_from(["polaris", "--confined-apply"])
        .expect("could not parse with just --confined-apply");
    assert!(args.confined_apply);
    assert!(args.prompt.is_none());
}

#[test]
fn prompt_parses_as_optional_at_the_clap_level() {
    // We don't use `required_unless_present`. It only looks at argument
    // names, not whether a subcommand is present, so adding a subcommand
    // here would get `polaris login` rejected for "missing --prompt"
    // (the same trap as M2 Task 8). At the clap level, `prompt` is
    // always optional; that it's "required on the normal path" is
    // verified by launching the real binary in
    // `tests/subcommands.rs::the_normal_path_still_requires_a_prompt`.
    // Here we only pin down clap's parse result.
    let args = Args::try_parse_from(["polaris"]).expect("clap should not make prompt required");
    assert!(args.prompt.is_none());
}

#[test]
fn tui_can_defer_routine_and_review_authority_until_its_first_input() {
    let args = Args::try_parse_from([
        "polaris",
        "--routine-authority",
        "{}",
        "--review-authority",
        "{}",
    ])
    .expect("interactive authority must not require --prompt");
    assert!(args.prompt.is_none());
    assert_eq!(args.routine_authority.as_deref(), Some("{}"));
    assert_eq!(args.review_authority.as_deref(), Some("{}"));
    assert!(
        Args::try_parse_from(["polaris", "--review-authority", "{}"]).is_err(),
        "review authority still requires routine authority"
    );
}

#[test]
fn skipped_skills_are_reported_one_line_each_naming_directory_and_cause() {
    // Directly verifies the formatting logic that hands the skipped
    // entries discover collected to stderr one line each, without
    // swallowing them. Verifying stderr by launching the real process
    // would mean going all the way down the path that requires an API
    // key, so here we pull out and verify just the formatting function.
    //
    // Looking only at the directory name, formatting that discards the
    // cause entirely (printing just `{s.dir_name}`) would also pass.
    // This function is the only thing standing between "a skill silently
    // vanished" and the user, so we also check that the cause is
    // present, and that "couldn't be read" isn't confused with "failed
    // validation". The two entries use different SkipCause variants.
    let skipped = vec![
        polaris_skills::Skipped {
            dir_name: "unreadable-one".into(),
            cause: polaris_skills::SkipCause::Unreadable(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                "no permission",
            )),
        },
        polaris_skills::Skipped {
            dir_name: "invalid-two".into(),
            cause: polaris_skills::SkipCause::Invalid(polaris_skills::SkillError::InvalidName {
                name: "Invalid-Two".into(),
            }),
        },
    ];

    let lines = format_skipped_skills(&skipped);

    assert_eq!(lines.len(), 2, "not one line each: {lines:?}");
    assert!(
        lines[0].contains("unreadable-one"),
        "directory name is missing: {}",
        lines[0]
    );
    assert!(
        lines[0].contains("no permission"),
        "the reason it couldn't be read is missing: {}",
        lines[0]
    );
    assert!(
        lines[1].contains("invalid-two"),
        "directory name is missing: {}",
        lines[1]
    );
    assert!(
        lines[1].contains("naming rules"),
        "the reason validation failed is missing: {}",
        lines[1]
    );
    // Each line carries only its own cause. Formatting that writes every
    // cause into every line would ultimately fail to convey which skill
    // vanished and why.
    assert!(
        !lines[0].contains("naming rules") && !lines[1].contains("no permission"),
        "causes are mixed across lines: {lines:?}"
    );
}

#[test]
fn nothing_is_printed_when_no_skill_was_skipped() {
    // If even one line came out when nothing was skipped, the user would
    // end up chasing a failure that doesn't exist. This pins down empty
    // input producing empty output directly, rather than as a side
    // effect of `.map().collect()`.
    assert!(
        format_skipped_skills(&[]).is_empty(),
        "output exists even though no skill was skipped"
    );
}

/// If tests that swap out HOME run concurrently in the same process,
/// they step on each other's HOME. Serialize the swap here.
static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Swaps HOME without a custom data directory, runs `f`, and restores both.
///
/// SAFETY: This only swaps environment variables within this process; it
/// doesn't affect other processes. `std::env::set_var`'s safety
/// condition is concurrent reads/writes from multiple threads, and
/// that's serialized by `HOME_LOCK` above.
fn with_home<T>(home: &Path, f: impl FnOnce() -> T) -> T {
    let _guard = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let prev = std::env::var_os("HOME");
    let previous_data_dir = std::env::var_os("POLARIS_DATA_DIR");
    unsafe {
        std::env::set_var("HOME", home);
        std::env::remove_var("POLARIS_DATA_DIR");
    }
    let out = f();
    match prev {
        Some(p) => unsafe { std::env::set_var("HOME", p) },
        None => unsafe { std::env::remove_var("HOME") },
    }
    match previous_data_dir {
        Some(p) => unsafe { std::env::set_var("POLARIS_DATA_DIR", p) },
        None => unsafe { std::env::remove_var("POLARIS_DATA_DIR") },
    }
    out
}

#[cfg(target_os = "macos")]
#[test]
fn managed_runtime_home_is_independent_from_custom_data_dir() {
    let state = tempfile::tempdir().expect("state");
    let fake_home = tempfile::tempdir().expect("fake home");
    with_home(fake_home.path(), || {
        let previous = std::env::var_os("POLARIS_DATA_DIR");
        unsafe { std::env::set_var("POLARIS_DATA_DIR", state.path()) };
        let resolved = managed_runtime_home().expect("runtime home");
        match previous {
            Some(value) => unsafe { std::env::set_var("POLARIS_DATA_DIR", value) },
            None => unsafe { std::env::remove_var("POLARIS_DATA_DIR") },
        }
        let actual = polaris_core::os_home::trusted_user_home().expect("actual OS home");
        assert_eq!(resolved, actual);
        assert_eq!(
            resolved.join(".polaris/runtime-images/python-e5-cat"),
            actual.join(".polaris/runtime-images/python-e5-cat")
        );
        assert!(!resolved.starts_with(state.path()));
        assert!(!resolved.starts_with(fake_home.path()));
    });
}

#[test]
fn one_project_has_one_state_dir_no_matter_how_deep_you_start() {
    // The writable root is derived from `project::resolve_root`, but the
    // state directory alone used to hash the working directory
    // directly. Launching the same project from deep inside it would
    // produce separate locations for the audit log and the helper's
    // staging area. The audit log is the record of reconstruction, so
    // if it splits, the history never comes together anywhere.
    let home = tempfile::tempdir().expect("temp directory");
    let project = tempfile::tempdir().expect("temp directory");
    std::fs::create_dir(project.path().join(".git")).expect("mkdir");
    let deep = project.path().join("crates/polaris-core/src");
    std::fs::create_dir_all(&deep).expect("mkdir");
    let other = tempfile::tempdir().expect("temp directory");
    std::fs::create_dir(other.path().join(".git")).expect("mkdir");

    let (from_root, from_deep, from_other) = with_home(home.path(), || {
        (
            polaris_core::project::state_dir(project.path())
                .expect("could not determine the default path"),
            polaris_core::project::state_dir(&deep).expect("could not determine the default path"),
            polaris_core::project::state_dir(other.path())
                .expect("could not determine the default path"),
        )
    });

    assert_eq!(
        from_root,
        from_deep,
        "the state directory splits based on launch depth: {} and {}",
        from_root.display(),
        from_deep.display()
    );
    // Reject a degenerate "fix" that just funnels everything into the
    // same place. Different projects must get different directories.
    assert_ne!(
        from_root,
        from_other,
        "different projects are sharing the same state directory: {}",
        from_root.display()
    );
}

/// The arm exists and names itself in the error a person will see.
/// Until 2026-09-19 the CLI had no local provider at all — local
/// models were reachable only from the desktop's role bindings — so
/// the measurement harnesses, which drive the CLI, could not run one.
#[test]
fn the_unknown_provider_message_lists_every_arm_that_exists() {
    // Read out of the source rather than written here twice: a new
    // arm that the message does not mention is a provider nobody can
    // find, and that is exactly how `llamacpp` would have shipped.
    let source = include_str!("../provider_startup.rs");
    let arms: std::collections::BTreeSet<&str> = source
        .lines()
        .filter_map(|line| line.trim().strip_suffix(" => {"))
        .filter_map(|name| name.strip_prefix('"'))
        .filter_map(|name| name.strip_suffix('"'))
        .filter(|name| ["openai", "codex", "llamacpp"].contains(name))
        .collect();
    assert_eq!(
        arms,
        ["codex", "llamacpp", "openai"].into_iter().collect(),
        "an arm was added or removed without this test noticing"
    );
    let message = source
        .lines()
        .find(|line| line.contains("POLARIS_PROVIDER is an unknown value"))
        .expect("the unknown-provider message is gone");
    for arm in &arms {
        assert!(
            source
                .lines()
                .skip_while(|line| !line.contains("POLARIS_PROVIDER is an unknown value"))
                .take(3)
                .any(|line| line.contains(arm)),
            "{arm} is not named in the unknown-provider message: {message}"
        );
    }
}

#[test]
fn default_audit_path_lives_under_home_state_not_cwd() {
    let home = tempfile::tempdir().expect("temp directory");
    let project = tempfile::tempdir().expect("temp directory");

    let got = with_home(home.path(), || {
        default_audit_path(project.path()).expect("could not determine the default path")
    });

    assert!(
        got.starts_with(home.path()),
        "audit log is not under the home directory: {}",
        got.display()
    );
    assert!(
        !got.starts_with(project.path()),
        "audit log is inside the working directory: {}",
        got.display()
    );
    assert_eq!(got.file_name().unwrap(), "audit.jsonl");
    assert!(got.parent().unwrap().is_dir(), "directory was not created");
}
