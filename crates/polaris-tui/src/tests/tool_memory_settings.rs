//! Saved tool-memory settings and lineage regression tests.
use super::*;
use polaris_core::{session::Session, tool_memory::RetentionMode};

#[test]
fn fork_with_unknown_memory_origin_is_rejected_without_changing_source_or_writing_files() {
    for tool_argument in [false, true] {
        let logs = tempfile::tempdir().unwrap();
        let mut session = Session::new();
        session.push_user("keep instructions");
        if tool_argument {
            session.push_assistant_tool_calls(
                "",
                vec![polaris_provider::ToolCall {
                    id: "pending".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path": "memory://original"}),
                }],
                Vec::new(),
            );
        } else {
            session.push_tool_result(
                "completed",
                "[Stored tool result original] memory://original",
            );
        }
        let before = serde_json::to_string(&session.messages).unwrap();
        let mut path = logs.path().join("original.jsonl");
        let mut meta = path.with_extension("meta.json");
        let old_path = path.clone();
        let old_meta = meta.clone();
        let mut started = 42;
        let mut status = Status::Idle;
        handle_fork(
            logs.path(),
            "/project",
            &mut session,
            &mut path,
            &mut meta,
            &mut started,
            &mut status,
        );
        assert_eq!(path, old_path);
        assert_eq!(meta, old_meta);
        assert_eq!(started, 42);
        assert_eq!(serde_json::to_string(&session.messages).unwrap(), before);
        assert_eq!(std::fs::read_dir(logs.path()).unwrap().count(), 0);
        assert!(matches!(status, Status::Notice(message) if message.contains("Cannot fork")));
    }
}

#[test]
fn tool_memory_lineage_rejects_unverified_ancestors_and_excessive_depth() {
    let root = tempfile::tempdir().unwrap();
    let foreign = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    let cwd = root
        .path()
        .canonicalize()
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    let write = |id: &str, origin: Option<&str>| {
        let path = logs.path().join(format!("{id}.jsonl"));
        persist::clear_session(&path).unwrap();
        if path.with_extension("meta.json").exists() {
            std::fs::remove_file(path.with_extension("meta.json")).unwrap();
        }
        persist::write_fork_meta(
            &path.with_extension("meta.json"),
            &persist::SessionMeta {
                cwd: cwd.clone(),
                started_at_millis: 1,
            },
            origin,
        )
        .unwrap();
        path
    };
    let a = write("A", None);
    let b = write("B", Some("A"));
    let c = write("C", Some("B"));
    assert_eq!(
        saved_tool_memory_namespace(root.path(), &c).unwrap(),
        ("C".into(), vec!["B".into(), "A".into()])
    );
    for bad in [
        serde_json::json!({"cwd":cwd,"started_at_millis":1,"tool_memory_origin":"missing"}),
        serde_json::json!({"cwd":cwd,"started_at_millis":1,"tool_memory_origin":"C"}),
        serde_json::json!({"cwd":cwd,"started_at_millis":1,"tool_memory_origin":"B"}),
        serde_json::json!({"cwd":cwd,"started_at_millis":1,"tool_memory_origin":"../A"}),
        serde_json::json!({"cwd":cwd,"started_at_millis":1,"tool_memory_origin":null}),
        serde_json::json!({"cwd":foreign.path(),"started_at_millis":1}),
        serde_json::json!({"cwd":"relative","started_at_millis":1}),
        serde_json::json!({"tool_memory_origin":"A"}),
        serde_json::json!([]),
    ] {
        std::fs::write(b.with_extension("meta.json"), bad.to_string()).unwrap();
        assert!(
            saved_tool_memory_namespace(root.path(), &c).is_err(),
            "{bad}"
        );
    }
    std::fs::write(b.with_extension("meta.json"), b"{broken").unwrap();
    assert!(saved_tool_memory_namespace(root.path(), &c).is_err());
    std::fs::remove_file(b.with_extension("meta.json")).unwrap();
    assert!(saved_tool_memory_namespace(root.path(), &c).is_err());
    write("B", Some("A"));
    std::fs::remove_file(&a).unwrap();
    assert!(saved_tool_memory_namespace(root.path(), &c).is_err());
    write("A", None);
    std::fs::remove_file(&b).unwrap();
    assert!(saved_tool_memory_namespace(root.path(), &c).is_err());

    let mut previous = "A".to_owned();
    for depth in 1..=MAX_TOOL_MEMORY_ORIGINS + 1 {
        let id = format!("depth-{depth}");
        let path = write(&id, Some(&previous));
        assert_eq!(
            saved_tool_memory_namespace(root.path(), &path).is_ok(),
            depth <= MAX_TOOL_MEMORY_ORIGINS
        );
        previous = id;
    }
}

#[tokio::test]
async fn two_generation_fork_resume_writes_own_session_and_origin_forget_invalidates() {
    for forget_child in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let logs = tempfile::tempdir().unwrap();
        let state = logs.path().canonicalize().unwrap().join("state");
        let path = logs.path().join("original.jsonl");
        let mut session = Session::new();
        configure_saved_tool_memory(
            &mut session,
            root.path(),
            &state,
            &path,
            Some(RetentionMode::History),
            None,
        )
        .unwrap();
        let saved = session
            .tool_memory
            .as_ref()
            .unwrap()
            .backend
            .save(
                &polaris_provider::ToolCall {
                    id: "call".into(),
                    name: "read".into(),
                    arguments: serde_json::json!({"path":"source.txt"}),
                },
                "original evidence before file changes",
            )
            .await
            .unwrap();
        session.push_tool_result("call", &format!("memory://{}", saved.id));
        persist::write_meta_if_absent(
            &path.with_extension("meta.json"),
            &persist::SessionMeta {
                cwd: root.path().canonicalize().unwrap().to_str().unwrap().into(),
                started_at_millis: 1,
            },
        )
        .unwrap();
        persist::append_message(&path, &session.messages[0]).unwrap();
        let (mut loaded, _) = persist::load_session(&path).unwrap();
        configure_saved_tool_memory(
            &mut loaded,
            root.path(),
            &state,
            &path,
            Some(RetentionMode::History),
            None,
        )
        .unwrap();
        let original = loaded
            .tool_memory
            .as_ref()
            .unwrap()
            .backend
            .read(&format!("memory://{}", saved.id), 0, 100)
            .await
            .unwrap();
        assert!(original.contains("original evidence before file changes"));
        let mut fork_path = path.clone();
        let mut meta_path = path.with_extension("meta.json");
        let mut started = 1;
        let mut status = Status::Idle;
        let mut descendants = Vec::new();
        let mut ancestor_ids = vec!["original".to_owned()];
        for generation in 0..2 {
            let previous = fork_path.clone();
            handle_fork(
                logs.path(),
                root.path().to_str().unwrap(),
                &mut loaded,
                &mut fork_path,
                &mut meta_path,
                &mut started,
                &mut status,
            );
            assert_ne!(fork_path, previous);
            assert_eq!(
                persist::read_tool_memory_origin(&meta_path)
                    .unwrap()
                    .as_deref(),
                previous.file_stem().and_then(|id| id.to_str())
            );
            assert!(matches!(&status, Status::Notice(message) if message.contains("Deleting")));
            loaded = persist::load_session(&fork_path).unwrap().0;
            configure_saved_tool_memory(
                &mut loaded,
                root.path(),
                &state,
                &fork_path,
                Some(RetentionMode::History),
                None,
            )
            .unwrap();
            let (current, origins) = saved_tool_memory_namespace(root.path(), &fork_path).unwrap();
            assert_eq!(origins, ancestor_ids);
            ancestor_ids.insert(0, current.clone());
            let backend = loaded.tool_memory.as_ref().unwrap().backend.clone();
            if let Some((_, parent_record, _)) = descendants.last() {
                assert!(
                    backend
                        .read(&format!("memory://{parent_record}"), 0, 100)
                        .await
                        .unwrap()
                        .contains("fork evidence")
                );
            }
            let record = backend
                .save(
                    &polaris_provider::ToolCall {
                        id: format!("fork-{generation}"),
                        name: "read".into(),
                        arguments: serde_json::json!({"path":"fork.txt"}),
                    },
                    "fork evidence",
                )
                .await
                .unwrap();
            let store = polaris_memory::MemoryStore::open(state.join("memory.sqlite3")).unwrap();
            let project = persist::project_identity(root.path()).unwrap();
            assert!(store.get(&project, &current, &record.id).unwrap().is_some());
            for ancestor in &origins {
                assert!(store.get(&project, ancestor, &record.id).unwrap().is_none());
            }
            assert!(
                session
                    .tool_memory
                    .as_ref()
                    .unwrap()
                    .backend
                    .read(&format!("memory://{}", record.id), 0, 100)
                    .await
                    .is_err()
            );
            descendants.push((current, record.id, backend));
            assert!(
                loaded
                    .tool_memory
                    .as_ref()
                    .unwrap()
                    .backend
                    .read(&format!("memory://{}", saved.id), 0, 100)
                    .await
                    .unwrap()
                    .contains("original evidence before file changes")
            );
        }
        let foreign = tempfile::tempdir().unwrap();
        configure_saved_tool_memory(
            &mut loaded,
            foreign.path(),
            &state,
            &fork_path,
            Some(RetentionMode::History),
            None,
        )
        .unwrap();
        assert!(loaded.tool_memory.is_none());
        configure_saved_tool_memory(
            &mut loaded,
            root.path(),
            &state,
            &fork_path,
            Some(RetentionMode::History),
            None,
        )
        .unwrap();
        {
            let _lock = memory::lock_memory(&state).unwrap();
            let mut store =
                polaris_memory::MemoryStore::open(state.join("memory.sqlite3")).unwrap();
            store
                .delete_session(
                    &persist::project_identity(root.path()).unwrap(),
                    if forget_child {
                        &descendants[0].0
                    } else {
                        "original"
                    },
                )
                .unwrap();
        }
        let store = polaris_memory::MemoryStore::open(state.join("memory.sqlite3")).unwrap();
        let project = persist::project_identity(root.path()).unwrap();
        assert_eq!(
            store
                .get(&project, "original", &saved.id)
                .unwrap()
                .is_some(),
            forget_child
        );
        if forget_child {
            assert!(
                store
                    .get(&project, &descendants[0].0, &descendants[0].1)
                    .unwrap()
                    .is_none()
            );
            assert!(
                session
                    .tool_memory
                    .as_ref()
                    .unwrap()
                    .backend
                    .read(&format!("memory://{}", saved.id), 0, 100)
                    .await
                    .unwrap()
                    .contains("original evidence")
            );
            assert!(
                session
                    .tool_memory
                    .as_ref()
                    .unwrap()
                    .backend
                    .save(
                        &polaris_provider::ToolCall {
                            id: "parent-later".into(),
                            name: "read".into(),
                            arguments: serde_json::json!({}),
                        },
                        "parent remains writable"
                    )
                    .await
                    .is_ok()
            );
        }
        for (_, record, backend) in &descendants {
            assert!(
                backend
                    .read(&format!("memory://{record}"), 0, 100)
                    .await
                    .is_err()
            );
            assert!(
                backend
                    .save(
                        &polaris_provider::ToolCall {
                            id: "blocked".into(),
                            name: "read".into(),
                            arguments: serde_json::json!({}),
                        },
                        "cannot resurrect"
                    )
                    .await
                    .is_err()
            );
        }
        let backend = &loaded.tool_memory.as_ref().unwrap().backend;
        assert!(
            backend
                .read(&format!("memory://{}", saved.id), 0, 100)
                .await
                .is_err()
        );
        assert!(
            backend
                .save(
                    &polaris_provider::ToolCall {
                        id: "later".into(),
                        name: "read".into(),
                        arguments: serde_json::json!({"path":"source.txt"})
                    },
                    "cannot resurrect"
                )
                .await
                .is_err()
        );
    }
}

#[test]
fn tool_memory_settings_preserve_messages_and_reject_foreign_or_unknown_origin() {
    let root = tempfile::tempdir().unwrap();
    let foreign = tempfile::tempdir().unwrap();
    let logs = tempfile::tempdir().unwrap();
    let state = logs.path().canonicalize().unwrap().join("state");
    let path = logs.path().join("saved.jsonl");
    let mut session = Session::new();
    configure_saved_tool_memory(
        &mut session,
        root.path(),
        &state,
        &path,
        Some(RetentionMode::History),
        None,
    )
    .unwrap();
    assert!(session.tool_memory.is_some());
    assert!(session.before_compact.is_none());
    session.push_user("keep user instructions");
    session.push_assistant_tool_calls(
        "pending",
        vec![polaris_provider::ToolCall {
            id: "pending-call".into(),
            name: "read".into(),
            arguments: r#"{"path":"file"}"#.into(),
        }],
        Vec::new(),
    );
    let before = serde_json::to_string(&session.messages).unwrap();
    configure_saved_tool_memory(
        &mut session,
        root.path(),
        &state,
        &path,
        Some(RetentionMode::History),
        None,
    )
    .unwrap();
    assert!(
        session.tool_memory.is_none(),
        "unknown nonempty origin is rejected"
    );
    persist::write_meta_if_absent(
        &path.with_extension("meta.json"),
        &persist::SessionMeta {
            cwd: root.path().canonicalize().unwrap().to_str().unwrap().into(),
            started_at_millis: 1,
        },
    )
    .unwrap();
    configure_saved_tool_memory(
        &mut session,
        root.path(),
        &state,
        &path,
        Some(RetentionMode::Retrieval),
        None,
    )
    .unwrap();
    assert_eq!(
        session.tool_memory.as_ref().unwrap().mode,
        RetentionMode::Retrieval
    );
    configure_saved_tool_memory(
        &mut session,
        foreign.path(),
        &state,
        &path,
        Some(RetentionMode::History),
        None,
    )
    .unwrap();
    assert!(
        session.tool_memory.is_none(),
        "foreign origin clears an existing backend"
    );
    assert_eq!(serde_json::to_string(&session.messages).unwrap(), before);
    configure_saved_tool_memory(&mut session, root.path(), &state, &path, None, None).unwrap();
    assert!(session.tool_memory.is_none());
}
