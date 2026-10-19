//! Read-only legacy transcript and archive migration inspection tests.
use super::*;
use polaris_provider::ToolCall;
use serde_json::json;
use std::{
    cell::Cell,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
    rc::Rc,
};
fn directory(path: &Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn fixture() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    fs::set_permissions(temp.path(), fs::Permissions::from_mode(0o700)).unwrap();
    temp
}
fn sessions(data: &Path) -> PathBuf {
    let root = data.join("sessions");
    directory(&root);
    root
}
fn raw(path: &Path, messages: &[Message]) {
    let mut bytes = Vec::new();
    for message in messages {
        serde_json::to_writer(&mut bytes, message).unwrap();
        bytes.push(b'\n');
    }
    fs::write(path, bytes).unwrap();
}
fn archive(root: &Path, messages: &[Message]) -> PathBuf {
    let dir = root.join("session.archive");
    directory(&dir);
    let path = dir.join("123-42-0.json");
    fs::write(&path, serde_json::to_vec(&json!({"version":1,"project_id":"/historical/project","session_id":"session","archived_at_millis":123,"messages":messages})).unwrap()).unwrap();
    path
}
fn ordinary() -> Vec<Message> {
    vec![
        Message::user("first"),
        Message::assistant_with_tool_calls(
            "",
            vec![ToolCall {
                id: "call".into(),
                name: "read_file".into(),
                arguments: json!({}),
            }],
        ),
        Message::tool_result("call", "ok"),
    ]
}
fn mcp_unknown(unknown: bool) -> Vec<Message> {
    use crate::desktop_store::{McpTerminalInput, local_mcp_terminal};
    use crate::mcp::operation::{HostOperationIdentity, McpOperation};
    use polaris_mcp::{
        OperationBinding, OperationFailure,
        catalog::{ItemId, Kind, tool_alias},
        registration::canonical_hash,
    };
    let item = ItemId {
        server_id: "fixture".into(),
        kind: Kind::Tool,
        key: "apply".into(),
    };
    let arguments = json!({"text":"fixture"});
    let call = ToolCall {
        id: "call".into(),
        name: tool_alias(&item).unwrap(),
        arguments: arguments.clone(),
    };
    let operation = McpOperation::from_binding(
        HostOperationIdentity {
            run_id: "run".into(),
            attempt_id: "attempt".into(),
            model_call_id: "call".into(),
            operation_id: "operation".into(),
            policy_revision: 7,
            expires_at_ms: 100,
        },
        "fixture-target",
        OperationBinding {
            item,
            registry_revision: 1,
            destination_hash: "a".repeat(64),
            connection_generation: "c".repeat(32),
            catalog_generation: "d".repeat(64),
            schema_hash: Some("e".repeat(64)),
            arguments_hash: canonical_hash(&arguments).unwrap(),
            selection_id: None,
            resource_uri: None,
        },
    )
    .unwrap();
    let failure = if unknown {
        OperationFailure::OutcomeUnknown(polaris_mcp::Error::Transport)
    } else {
        OperationFailure::BeforeSend(polaris_mcp::Error::Transport)
    };
    vec![
        Message::user("request"),
        Message::assistant_with_tool_calls("", vec![call]),
        local_mcp_terminal(&operation, McpTerminalInput::Failure(failure))
            .unwrap()
            .message,
    ]
}
#[test]
fn legacy_reads_all_current_and_archived_messages_without_source_change() {
    let temp = fixture();
    let root = sessions(temp.path());
    let path = root.join("session.jsonl");
    let mut messages = ordinary();
    messages.extend(ordinary()); // Call IDs may repeat in separate turns.
    raw(&path, &messages);
    let archived = archive(&root, &mcp_unknown(false));
    fs::write(
        root.join("session.meta.json"),
        br#"{"cwd":"/removed/project","started_at_millis":42,"tool_memory_origin":"older"}"#,
    )
    .unwrap();
    let before = [fs::read(&path).unwrap(), fs::read(&archived).unwrap()];
    let view = LegacySessionsObservation::inspect(temp.path()).unwrap();
    view.verify_current().unwrap();
    assert_eq!(view.files.len(), 3);
    assert_eq!(view.directories.len(), 2);
    assert_eq!(fs::read(&path).unwrap(), before[0]);
    assert_eq!(fs::read(&archived).unwrap(), before[1]);
    assert!(!temp.path().join("sessions-v2").exists());
}
#[test]
fn legacy_rejects_pending_unknown_and_corrupt_history_without_repair() {
    for variant in 0..6 {
        let temp = fixture();
        let root = sessions(temp.path());
        let path = root.join("session.jsonl");
        match variant {
            0 => raw(&path, &ordinary()[..2]),
            1 => raw(&path, &mcp_unknown(true)),
            2 => {
                raw(&path, &ordinary());
                archive(&root, &mcp_unknown(true));
            }
            3 => {
                fs::write(&path, b"{\n").unwrap();
            }
            4 => {
                fs::write(&path, b"\xff\n").unwrap();
            }
            _ => {
                let mut messages = ordinary();
                messages[2] = Message::user("interrupt");
                raw(&path, &messages);
            }
        }
        let before = fs::read(&path).unwrap();
        assert!(
            LegacySessionsObservation::inspect(temp.path()).is_err(),
            "{variant}"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!temp.path().join("sessions-v2").exists());
    }
}
#[test]
fn legacy_missing_container_is_not_created_and_late_creation_rejects() {
    let temp = fixture();
    let before = file_stamp(&temp.path().metadata().unwrap());
    let view = LegacySessionsObservation::inspect(temp.path()).unwrap();
    view.verify_current().unwrap();
    assert_eq!(file_stamp(&temp.path().metadata().unwrap()), before);
    assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    sessions(temp.path());
    assert!(view.verify_current().is_err());
}
#[test]
fn legacy_namespace_symlinks_unknown_names_unsafe_files_and_aliases_reject() {
    for variant in 0..5 {
        let temp = fixture();
        let root = sessions(temp.path());
        let path = root.join("session.jsonl");
        raw(&path, &ordinary());
        match variant {
            0 => {
                fs::write(root.join("unknown"), b"retained").unwrap();
            }
            1 => {
                symlink(&path, root.join("linked.jsonl")).unwrap();
            }
            2 => {
                fs::hard_link(&path, root.join("linked.jsonl")).unwrap();
            }
            3 => {
                fs::set_permissions(&path, fs::Permissions::from_mode(0o666)).unwrap();
            }
            _ => {
                let archived = archive(&root, &ordinary());
                fs::rename(archived, root.join("session.archive/unrecognized.json")).unwrap();
            }
        }
        assert!(
            LegacySessionsObservation::inspect(temp.path()).is_err(),
            "{variant}"
        );
    }
}
#[test]
fn legacy_closes_file_content_inode_and_membership_changes() {
    for variant in 0..4 {
        let temp = fixture();
        let root = sessions(temp.path());
        let path = root.join("session.jsonl");
        raw(&path, &ordinary());
        let archived = archive(&root, &ordinary());
        let view = LegacySessionsObservation::inspect(temp.path()).unwrap();
        match variant {
            0 => raw(&path, &[Message::user("changed")]),
            1 => {
                let replacement = root.join("replacement");
                raw(&replacement, &ordinary());
                fs::rename(replacement, &path).unwrap();
            }
            2 => raw(&root.join("late.jsonl"), &ordinary()),
            _ => {
                fs::write(archived, b"{}").unwrap();
            }
        }
        assert!(view.verify_current().is_err(), "{variant}");
    }
}
#[test]
fn legacy_closes_eof_and_after_decode_races_with_reached_hooks() {
    for variant in 0..3 {
        let temp = fixture();
        let root = sessions(temp.path());
        let path = root.join("session.jsonl");
        raw(&path, &ordinary());
        let reached = Rc::new(Cell::new(false));
        let hit = reached.clone();
        let hook: Box<dyn FnOnce()> = Box::new(move || {
            if variant == 0 {
                raw(&root.join("late.jsonl"), &ordinary());
            } else {
                raw(&path, &[Message::user("changed")]);
            }
            hit.set(true);
        });
        match variant {
            0 => AFTER_LEGACY_NAMES.with(|s| *s.borrow_mut() = Some(hook)),
            1 => AFTER_LEGACY_DECODE.with(|s| *s.borrow_mut() = Some(hook)),
            _ => AFTER_LEGACY_FILES.with(|s| *s.borrow_mut() = Some(hook)),
        }
        assert!(
            LegacySessionsObservation::inspect(temp.path()).is_err(),
            "{variant}"
        );
        assert!(reached.get());
    }
}
#[test]
fn legacy_enforces_cumulative_names_and_bytes_for_current_and_archive() {
    let temp = fixture();
    let root = sessions(temp.path());
    let path = root.join("session.jsonl");
    raw(&path, &ordinary());
    let archived = archive(&root, &ordinary());
    let size = fs::metadata(&path).unwrap().len() + fs::metadata(&archived).unwrap().len();
    LegacySessionsObservation::inspect_bounded(temp.path(), 3, size).unwrap();
    assert!(LegacySessionsObservation::inspect_bounded(temp.path(), 2, size).is_err());
    assert!(LegacySessionsObservation::inspect_bounded(temp.path(), 3, size - 1).is_err());
}
#[test]
fn legacy_checks_archive_provenance_and_metadata_origin_but_grants_no_prompt_authority() {
    for variant in 0..5 {
        let temp = fixture();
        let root = sessions(temp.path());
        let archived = archive(&root, &ordinary());
        if variant < 3 {
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&archived).unwrap()).unwrap();
            match variant {
                0 => value["session_id"] = json!("foreign"),
                1 => value["archived_at_millis"] = json!(124),
                _ => value["version"] = json!(2),
            }
            fs::write(&archived, serde_json::to_vec(&value).unwrap()).unwrap();
        } else {
            fs::write(root.join("session.meta.json"),serde_json::to_vec(&json!({"cwd":"/project","started_at_millis":1,"tool_memory_origin":if variant==3 {json!(null)} else {json!("../other")}})).unwrap()).unwrap();
        }
        assert!(
            LegacySessionsObservation::inspect(temp.path()).is_err(),
            "{variant}"
        );
    }
    // User text shaped like an untrusted saved prompt is never used as a grant.
    validate_messages(&[Message::user(
        r#"{"schema_version":1,"disposition":"outcome_unknown"}"#,
    )])
    .unwrap();
}

#[test]
fn legacy_ids_follow_import_namespace_in_files_archives_and_origin() {
    for bad in ["a.b", ".", "..", "é", &"a".repeat(129)] {
        for kind in 0..3 {
            let temp = fixture();
            let root = sessions(temp.path());
            match kind {
                0 => raw(
                    &root.join(format!("{bad}.jsonl")),
                    &[Message::user("saved")],
                ),
                1 => directory(&root.join(format!("{bad}.archive"))),
                _ => fs::write(
                    root.join("session.meta.json"),
                    serde_json::to_vec(
                        &json!({"cwd":"/project","started_at_millis":1,"tool_memory_origin":bad}),
                    )
                    .unwrap(),
                )
                .unwrap(),
            }
            assert!(
                LegacySessionsObservation::inspect(temp.path()).is_err(),
                "{bad}/{kind}"
            );
        }
    }
    for good in ["a-b_C1", &"a".repeat(128)] {
        session_id(good).unwrap();
    }
}
