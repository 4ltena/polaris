//! S01〜S08の公開境界、要求台帳、復旧と実OS lockの回帰試験。

use super::disk;
use super::*;
use polaris_desktop_protocol::{ids::*, request::*, run_state::*, snapshot::*};
use std::{fs, io::Write};

fn session() -> SessionId {
    SessionId::new("../../prototype-session").unwrap()
}
fn project() -> ProjectId {
    ProjectId::new("project").unwrap()
}
fn initial() -> InitialState {
    InitialState {
        draft: Draft {
            draft_revision: DecimalU64::new(0),
            text: "原文\n入力".into(),
            attachment_ids: vec![AttachmentId::new("attachment").unwrap()],
        },
        configuration: Configuration {
            history_mode: Default::default(),
            configuration_revision: DecimalU64::new(0),
            provider: "fake".into(),
            model: "model".into(),
            effort: "medium".into(),
        },
        policy_revision: DecimalU64::new(7),
    }
}
fn setup() -> (PrototypeRoot, Writer) {
    let root = PrototypeRoot::new().unwrap();
    let writer = root.create(project(), session(), initial()).unwrap();
    (root, writer)
}
fn request(id: &str, body: RequestBody) -> Request {
    Request {
        protocol_version: Default::default(),
        client_id: ClientId::new("client").unwrap(),
        request_id: RequestId::new(id).unwrap(),
        body,
    }
}
fn draft(id: &str, revision: u64, text: &str) -> Request {
    request(
        id,
        RequestBody::DraftUpdate(
            session(),
            DraftUpdate {
                expected_draft_revision: DecimalU64::new(revision),
                text: text.into(),
                attachment_ids: Vec::new(),
            },
        ),
    )
}
fn configure(id: &str, revision: u64) -> Request {
    request(
        id,
        RequestBody::SessionConfigure(
            session(),
            SessionConfigure {
                history_mode: Default::default(),
                expected_configuration_revision: DecimalU64::new(revision),
                provider: "fake2".into(),
                model: "model2".into(),
                effort: "high".into(),
            },
        ),
    )
}
fn start(id: &str, revision: u64) -> Request {
    request(
        id,
        RequestBody::RunStart(
            session(),
            RunStart {
                expected_draft_revision: DecimalU64::new(revision),
                expected_configuration_revision: DecimalU64::new(0),
                expected_policy_revision: DecimalU64::new(7),
            },
        ),
    )
}
fn target() -> RunTarget {
    RunTarget {
        run_id: RunId::new("run").unwrap(),
        attempt_id: AttemptId::new("attempt").unwrap(),
    }
}
fn op() -> OperationId {
    OperationId::new("operation").unwrap()
}
fn result() -> ResultId {
    ResultId::new("result").unwrap()
}
fn dir(writer: &Writer) -> std::path::PathBuf {
    // 試験のみ: 非公開内部のpathは外部APIへ漏らさない。
    writer.test_directory()
}
fn inject(point: Option<&'static str>) {
    disk::FAIL.with(|f| f.set(point));
}
fn unchanged(before: &Published, after: &Published) {
    assert_eq!(before.marker, after.marker);
    assert_eq!(before.state, after.state);
    assert_eq!(
        serde_json::to_vec(&before.raw).unwrap(),
        serde_json::to_vec(&after.raw).unwrap()
    );
}

#[test]
fn s01_sidecar_changes_preserve_content_and_publish_together() {
    let (root, mut writer) = setup();
    let before = writer.snapshot().unwrap();
    writer.apply(&draft("d", 0, "new"), None).unwrap();
    writer.apply(&configure("c", 0), None).unwrap();
    let after = writer.snapshot().unwrap();
    let configured = Configuration {
        history_mode: Default::default(),
        configuration_revision: DecimalU64::new(1),
        provider: "fake2".into(),
        model: "model2".into(),
        effort: "high".into(),
    };
    assert_eq!(after.state.configuration, configured);
    assert_eq!(after.marker.session_revision.get(), 2);
    assert_eq!(
        after.marker.content_revision,
        before.marker.content_revision
    );
    assert_eq!(after.marker.raw_hash, before.marker.raw_hash);
    assert_ne!(after.marker.sidecar_hash, before.marker.sidecar_hash);
    drop(writer);
    let mut reopened = root.open(&project(), &session()).unwrap();
    unchanged(&after, &reopened.snapshot().unwrap());
    assert_eq!(reopened.snapshot().unwrap().state.configuration, configured);
    let mut req = start("configured-start", 1);
    if let RequestBody::RunStart(_, params) = &mut req.body {
        params.expected_configuration_revision = DecimalU64::new(1);
    }
    assert_eq!(
        reopened
            .apply(&req, Some(target()))
            .unwrap()
            .run
            .unwrap()
            .configuration,
        configured
    );
    drop(reopened);
    let snapshot = root
        .open(&project(), &session())
        .unwrap()
        .snapshot()
        .unwrap();
    assert_eq!(snapshot.state.runs[0].configuration, configured);
}

#[test]
fn s02_dedup_precedes_cas_and_conflicts_do_not_publish() {
    let (_, mut writer) = setup();
    let req = draft("d", 0, "edited");
    let accepted = writer.apply(&req, None).unwrap();
    let before = writer.snapshot().unwrap();
    assert_eq!(writer.apply(&req, None).unwrap(), accepted);
    assert!(matches!(
        writer.apply(&draft("d", 0, "different"), None),
        Err(StoreError::RequestConflict)
    ));
    assert!(matches!(
        writer.apply(&draft("other", 0, "different"), None),
        Err(StoreError::CasConflict("draft"))
    ));
    assert!(matches!(
        writer.apply(&configure("d", 0), None),
        Err(StoreError::RequestConflict)
    ));
    unchanged(&before, &writer.snapshot().unwrap());
    writer.apply(&configure("c", 0), None).unwrap();
    let before = writer.snapshot().unwrap();
    assert!(matches!(
        writer.apply(&configure("stale", 0), None),
        Err(StoreError::CasConflict("configuration"))
    ));
    unchanged(&before, &writer.snapshot().unwrap());
}

#[test]
fn s02_keys_include_client_and_session_and_targets_are_checked() {
    let (root, mut writer) = setup();
    writer.apply(&draft("same", 0, "a"), None).unwrap();
    let mut second = draft("same", 1, "b");
    second.client_id = ClientId::new("other-client").unwrap();
    writer.apply(&second, None).unwrap();
    assert_eq!(writer.snapshot().unwrap().state.requests.len(), 2);
    let other = SessionId::new("other-session").unwrap();
    let mut other_writer = root.create(project(), other.clone(), initial()).unwrap();
    let before = writer.snapshot().unwrap();
    assert!(matches!(
        other_writer.apply(&draft("same", 0, "c"), None),
        Err(StoreError::TargetMismatch)
    ));
    let mut own = draft("same", 0, "c");
    if let RequestBody::DraftUpdate(s, _) = &mut own.body {
        *s = other.clone();
    }
    other_writer.apply(&own, None).unwrap();
    assert!(matches!(
        writer.request_status(&other, &own.client_id, &own.request_id),
        Err(StoreError::TargetMismatch)
    ));
    unchanged(&before, &writer.snapshot().unwrap());
}

#[test]
fn s03_run_start_is_atomic_and_second_parent_is_busy_after_latest_draft() {
    let (_, mut writer) = setup();
    let accepted = writer.apply(&start("start", 0), Some(target())).unwrap();
    let snapshot = writer.snapshot().unwrap();
    assert_eq!(snapshot.raw.len(), 1);
    assert_eq!(snapshot.raw[0].message.content, initial().draft.text);
    assert_eq!(snapshot.raw[0].message.role, polaris_provider::Role::User);
    assert!(snapshot.state.draft.text.is_empty());
    assert!(snapshot.state.draft.attachment_ids.is_empty());
    assert_eq!(snapshot.marker.content_revision.get(), 1);
    assert_eq!(accepted.run.unwrap().input.attachment_ids.len(), 1);
    assert_eq!(snapshot.state.requests.len(), 1);
    assert_eq!(
        writer.apply(&start("start", 0), None).unwrap().record,
        snapshot.state.requests[0]
    );
    unchanged(&snapshot, &writer.snapshot().unwrap());
    writer.apply(&draft("next", 1, "next input"), None).unwrap();
    let before = writer.snapshot().unwrap();
    let another = RunTarget {
        run_id: RunId::new("second").unwrap(),
        attempt_id: AttemptId::new("second").unwrap(),
    };
    assert!(matches!(
        writer.apply(&start("second", 2), Some(another)),
        Err(StoreError::Busy)
    ));
    unchanged(&before, &writer.snapshot().unwrap());
    writer.apply(&configure("newconfig", 0), None).unwrap();
    assert_eq!(
        writer.snapshot().unwrap().state.runs[0].configuration,
        initial().configuration
    );
}

#[test]
fn s03_run_start_checks_all_three_revisions() {
    let (_, mut writer) = setup();
    for field in ["draft", "configuration", "policy"] {
        let mut req = start(field, 0);
        if let RequestBody::RunStart(_, p) = &mut req.body {
            match field {
                "draft" => p.expected_draft_revision = DecimalU64::new(99),
                "configuration" => p.expected_configuration_revision = DecimalU64::new(99),
                _ => p.expected_policy_revision = DecimalU64::new(99),
            }
        }
        let before = writer.snapshot().unwrap();
        assert!(matches!(
            writer.apply(&req, Some(target())),
            Err(StoreError::CasConflict(_))
        ));
        unchanged(&before, &writer.snapshot().unwrap());
    }
}

fn write_boundary(point: &'static str) {
    let (root, mut writer) = setup();
    let before = writer.snapshot().unwrap();
    let req = start("start", 0);
    inject(Some(point));
    assert!(writer.apply(&req, Some(target())).is_err());
    inject(None);
    assert!(matches!(
        writer.snapshot(),
        Err(StoreError::RecoveryRequired)
    ));
    assert!(matches!(
        writer.apply(&req, Some(target())),
        Err(StoreError::RecoveryRequired)
    ));
    assert!(matches!(
        writer.request_status(&session(), &req.client_id, &req.request_id),
        Err(StoreError::RecoveryRequired)
    ));
    let visible = disk::load(
        &disk::Directory::open_owned(&dir(&writer)).unwrap(),
        &project(),
        &session(),
    )
    .unwrap();
    if point == "directory_sync" {
        assert_eq!(visible.raw.len(), 1);
        assert_eq!(visible.state.requests.len(), 1);
        assert!(visible.state.draft.text.is_empty());
    } else {
        unchanged(&before, &visible);
    }
    drop(writer);
    let mut reopened = root.open(&project(), &session()).unwrap();
    let status = reopened
        .request_status(&session(), &req.client_id, &req.request_id)
        .unwrap();
    if point == "directory_sync" {
        assert_eq!(
            status.unwrap().run.unwrap().run.state,
            RunState::Interrupted
        );
        let before = reopened.snapshot().unwrap();
        reopened.apply(&req, None).unwrap();
        unchanged(&before, &reopened.snapshot().unwrap());
    } else {
        assert!(status.is_none());
        reopened.apply(&req, Some(target())).unwrap();
    }
    assert_eq!(reopened.snapshot().unwrap().raw.len(), 1);
    assert_eq!(reopened.snapshot().unwrap().state.requests.len(), 1);
}

macro_rules! boundary_tests {
    ($($name:ident => $point:literal),* $(,)?) => { $(
        #[test]
        fn $name() { write_boundary($point); }
    )* };
}
boundary_tests! {
    s04_raw_partial => "raw_partial",
    s04_raw_sync => "raw_sync",
    s04_sidecar_partial => "sidecar_partial",
    s04_sidecar_sync => "sidecar_sync",
    s04_marker_partial => "marker_partial",
    s04_marker_sync => "marker_sync",
    s04_marker_rename => "marker_rename",
    s04_directory_sync => "directory_sync",
}

#[test]
fn s05_recovery_blocks_reads_dedup_and_writes_until_sync_is_confirmed() {
    let (root, mut writer) = setup();
    let req = draft("d", 0, "published");
    inject(Some("directory_sync"));
    assert!(writer.apply(&req, None).is_err());
    inject(Some("recovery_sync"));
    assert!(writer.recover().is_err());
    assert!(matches!(
        writer.snapshot(),
        Err(StoreError::RecoveryRequired)
    ));
    assert!(matches!(
        writer.apply(&req, None),
        Err(StoreError::RecoveryRequired)
    ));
    drop(writer);
    assert!(root.open(&project(), &session()).is_err());
    inject(None);
    let mut reopened = root.open(&project(), &session()).unwrap();
    let before = reopened.snapshot().unwrap();
    assert_eq!(before.state.draft.text, "published");
    assert_eq!(before.state.requests.len(), 1);
    assert!(
        reopened
            .request_status(&session(), &req.client_id, &req.request_id)
            .unwrap()
            .is_some()
    );
    reopened.apply(&req, None).unwrap();
    unchanged(&before, &reopened.snapshot().unwrap());
}

#[test]
fn s05_same_handle_recovery_reconciles_both_sides_of_rename() {
    for point in ["raw_partial", "directory_sync"] {
        let (_, mut writer) = setup();
        let req = start("start", 0);
        inject(Some(point));
        assert!(writer.apply(&req, Some(target())).is_err());
        inject(None);
        writer.recover().unwrap();
        let exists = writer
            .request_status(&session(), &req.client_id, &req.request_id)
            .unwrap()
            .is_some();
        assert_eq!(exists, point == "directory_sync");
        writer.apply(&req, Some(target())).unwrap();
        assert_eq!(writer.snapshot().unwrap().raw.len(), 1);
    }
}

#[test]
fn s05_complete_and_partial_unpublished_tails_never_leak() {
    for tail in [
        b"{\"complete\":true}\n".as_slice(),
        b"{\"partial\":".as_slice(),
    ] {
        let (root, mut writer) = setup();
        writer.apply(&draft("d", 0, "visible"), None).unwrap();
        let before = writer.snapshot().unwrap();
        let path = dir(&writer);
        drop(writer);
        for name in [disk::RAW, disk::SIDECAR] {
            fs::OpenOptions::new()
                .append(true)
                .open(path.join(name))
                .unwrap()
                .write_all(tail)
                .unwrap();
        }
        let mut reopened = root.open(&project(), &session()).unwrap();
        unchanged(&before, &reopened.snapshot().unwrap());
        reopened.apply(&draft("new", 1, "next"), None).unwrap();
        let published = reopened.snapshot().unwrap();
        assert!(published.raw.is_empty());
        assert_eq!(
            fs::metadata(path.join(disk::RAW)).unwrap().len(),
            published.marker.raw_offset.get()
        );
        assert_eq!(
            fs::metadata(path.join(disk::SIDECAR)).unwrap().len(),
            published.marker.sidecar_offset.get()
        );
    }
}

#[test]
fn s05_short_and_corrupt_published_prefixes_are_rejected() {
    for name in [disk::RAW, disk::SIDECAR] {
        for truncate in [false, true] {
            let (root, mut writer) = setup();
            writer.apply(&start("start", 0), Some(target())).unwrap();
            let path = dir(&writer).join(name);
            drop(writer);
            let mut bytes = fs::read(&path).unwrap();
            if truncate {
                bytes.pop();
            } else {
                bytes[0] = b'!';
            }
            fs::write(path, bytes).unwrap();
            assert!(matches!(
                root.open(&project(), &session()),
                Err(StoreError::Corrupt(_))
            ));
        }
    }
}

#[test]
fn s05_recovery_refuses_changed_marker_and_wrong_identity() {
    let (root, mut writer) = setup();
    let path = dir(&writer).join(disk::MARKER);
    let mut marker = writer.snapshot().unwrap().marker;
    marker.session_id = SessionId::new("wrong").unwrap();
    fs::write(&path, serde_json::to_vec(&marker).unwrap()).unwrap();
    assert!(writer.apply(&draft("d", 0, "new"), None).is_err());
    assert!(matches!(writer.recover(), Err(StoreError::TargetMismatch)));
    drop(writer);
    assert!(matches!(
        root.open(&project(), &session()),
        Err(StoreError::TargetMismatch)
    ));
}

#[test]
fn s06_reopen_before_intent_interrupts_and_after_intent_is_unknown() {
    for intended in [false, true] {
        let (root, mut writer) = setup();
        let req = start("start", 0);
        writer.apply(&req, Some(target())).unwrap();
        if intended {
            writer.record_intent(&target(), op()).unwrap();
        }
        let revision = writer.snapshot().unwrap().marker.session_revision;
        drop(writer);
        let mut reopened = root.open(&project(), &session()).unwrap();
        let snapshot = reopened.snapshot().unwrap();
        assert_eq!(
            snapshot.marker.session_revision,
            revision.checked_add(1).unwrap()
        );
        let state = if intended {
            RunState::OutcomeUnknown
        } else {
            RunState::Interrupted
        };
        assert_eq!(snapshot.state.runs[0].run.state, state);
        let status = reopened
            .request_status(&session(), &req.client_id, &req.request_id)
            .unwrap()
            .unwrap();
        assert_eq!(status.run.unwrap().run.state, state);
        assert!(
            reopened
                .request_status(
                    &session(),
                    &req.client_id,
                    &RequestId::new("missing").unwrap()
                )
                .unwrap()
                .is_none()
        );
        reopened.apply(&req, None).unwrap();
        assert!(matches!(
            reopened.record_intent(&target(), OperationId::new("new-operation").unwrap()),
            Err(StoreError::RunConflict)
        ));
        unchanged(&snapshot, &reopened.snapshot().unwrap());
        drop(reopened);
        unchanged(
            &snapshot,
            &root
                .open(&project(), &session())
                .unwrap()
                .snapshot()
                .unwrap(),
        );
    }
}

#[test]
fn s06_intent_and_result_failure_boundaries_do_not_grant_new_dispatch() {
    for point in ["marker_rename", "directory_sync"] {
        let (root, mut writer) = setup();
        writer.apply(&start("start", 0), Some(target())).unwrap();
        inject(Some(point));
        assert!(writer.record_intent(&target(), op()).is_err());
        inject(None);
        drop(writer);
        let mut reopened = root.open(&project(), &session()).unwrap();
        let state = reopened.snapshot().unwrap().state.runs[0].run.state;
        assert_eq!(
            state,
            if point == "marker_rename" {
                RunState::Interrupted
            } else {
                RunState::OutcomeUnknown
            }
        );
        if point == "directory_sync" {
            assert_eq!(
                reopened.record_intent(&target(), op()).unwrap(),
                IntentReceipt::AlreadyRecorded
            );
        }
    }
}

#[test]
fn s06_terminal_and_result_are_durable_idempotent_and_conflicts_rejected() {
    let (root, mut writer) = setup();
    let req = start("start", 0);
    writer.apply(&req, Some(target())).unwrap();
    assert_eq!(
        writer.record_intent(&target(), op()).unwrap(),
        IntentReceipt::NewlyPublished
    );
    let intent = writer.snapshot().unwrap();
    assert_eq!(
        writer.record_intent(&target(), op()).unwrap(),
        IntentReceipt::AlreadyRecorded
    );
    unchanged(&intent, &writer.snapshot().unwrap());
    assert!(matches!(
        writer.finish(&target(), Observation::Succeeded, result()),
        Err(StoreError::RunConflict)
    ));
    writer
        .record_operation_result(&target(), &op(), result())
        .unwrap();
    let before = writer.snapshot().unwrap();
    writer
        .record_operation_result(&target(), &op(), result())
        .unwrap();
    unchanged(&before, &writer.snapshot().unwrap());
    assert!(matches!(
        writer.record_operation_result(&target(), &op(), ResultId::new("conflict").unwrap()),
        Err(StoreError::RunConflict)
    ));
    writer
        .finish(&target(), Observation::Succeeded, result())
        .unwrap();
    let terminal = writer.snapshot().unwrap();
    writer
        .finish(&target(), Observation::Succeeded, result())
        .unwrap();
    assert!(matches!(
        writer.finish(&target(), Observation::Failed, result()),
        Err(StoreError::RunConflict)
    ));
    assert!(matches!(
        writer.finish(
            &target(),
            Observation::Succeeded,
            ResultId::new("conflict").unwrap()
        ),
        Err(StoreError::RunConflict)
    ));
    unchanged(&terminal, &writer.snapshot().unwrap());
    drop(writer);
    let mut reopened = root.open(&project(), &session()).unwrap();
    assert_eq!(
        reopened.apply(&req, None).unwrap().run.unwrap().result_id,
        Some(result())
    );
    unchanged(&terminal, &reopened.snapshot().unwrap());
}

#[test]
fn s06_terminal_publish_failure_before_and_after_rename() {
    for point in ["marker_rename", "directory_sync"] {
        let (root, mut writer) = setup();
        writer.apply(&start("start", 0), Some(target())).unwrap();
        writer.record_intent(&target(), op()).unwrap();
        writer
            .record_operation_result(&target(), &op(), result())
            .unwrap();
        inject(Some(point));
        assert!(
            writer
                .finish(&target(), Observation::Succeeded, result())
                .is_err()
        );
        inject(None);
        drop(writer);
        let reopened = root.open(&project(), &session()).unwrap();
        let run = &reopened.snapshot().unwrap().state.runs[0];
        assert_eq!(
            run.run.state,
            if point == "marker_rename" {
                RunState::Interrupted
            } else {
                RunState::Succeeded
            }
        );
        assert_eq!(run.result_id.is_some(), point == "directory_sync");
    }
}

#[test]
fn s07_writer_lifetime_and_distinct_sessions_use_real_locks() {
    let (root, writer) = setup();
    assert!(matches!(
        root.open(&project(), &session()),
        Err(StoreError::Busy)
    ));
    let other = root
        .create(project(), SessionId::new("other").unwrap(), initial())
        .unwrap();
    assert_eq!(other.snapshot().unwrap().marker.session_revision.get(), 0);
    let path = dir(&writer);
    drop(root);
    assert!(path.exists());
    assert!(writer.snapshot().is_ok());
    drop(writer);
    assert!(path.exists());
    drop(other);
    assert!(!path.exists());
}

#[test]
fn s07_tombstone_prevents_recreation_replay_and_reopen() {
    let (root, mut writer) = setup();
    let req = draft("d", 0, "saved");
    writer.apply(&req, None).unwrap();
    writer.tombstone().unwrap();
    writer.tombstone().unwrap();
    assert!(matches!(writer.apply(&req, None), Err(StoreError::Deleted)));
    let marker = disk::read_marker(&disk::Directory::open_owned(&dir(&writer)).unwrap()).unwrap();
    assert!(marker.deleted);
    assert_eq!(marker.session_revision.get(), 2);
    drop(writer);
    assert!(matches!(
        root.open(&project(), &session()),
        Err(StoreError::Deleted)
    ));
    assert!(root.create(project(), session(), initial()).is_err());
}

#[test]
fn s07_running_session_cannot_be_tombstoned() {
    let (_, mut writer) = setup();
    writer.apply(&start("start", 0), Some(target())).unwrap();
    let before = writer.snapshot().unwrap();
    assert!(matches!(writer.tombstone(), Err(StoreError::Busy)));
    unchanged(&before, &writer.snapshot().unwrap());
}

#[test]
fn s08_v3_rejects_v2_unknown_and_unknown_fields() {
    for version in [2, 4] {
        let (root, writer) = setup();
        let mut marker = writer.snapshot().unwrap().marker;
        marker.schema_version = version;
        let path = dir(&writer).join(disk::MARKER);
        drop(writer);
        fs::write(path, serde_json::to_vec(&marker).unwrap()).unwrap();
        assert!(
            matches!(root.open(&project(), &session()), Err(StoreError::UnsupportedVersion(v)) if v == version)
        );
    }
    let (root, writer) = setup();
    let mut marker = serde_json::to_value(writer.snapshot().unwrap().marker).unwrap();
    marker["future"] = true.into();
    let path = dir(&writer).join(disk::MARKER);
    drop(writer);
    fs::write(path, serde_json::to_vec(&marker).unwrap()).unwrap();
    assert!(matches!(
        root.open(&project(), &session()),
        Err(StoreError::Json(_))
    ));
}

#[test]
fn s08_existing_v2_reader_rejects_v3_at_its_actual_state_path() {
    use crate::conversation_state::{ConversationStateV2, ConversationStore};
    let temp = tempfile::tempdir().unwrap();
    let session_id = "01234567-89ab-cdef-0123-456789abcdef";
    let v2state = ConversationStateV2::new("project".into(), session_id.into()).unwrap();
    let store = ConversationStore::create(temp.path(), v2state.clone()).unwrap();
    assert!(ConversationStore::open(temp.path(), "project", session_id).is_ok());
    let (_, writer) = setup();
    let mut v3 = writer.snapshot().unwrap().marker;
    v3.session_id = SessionId::new(session_id).unwrap();
    fs::write(
        store.directory().join("state.json"),
        serde_json::to_vec(&v3).unwrap(),
    )
    .unwrap();
    assert!(ConversationStore::open(temp.path(), "project", session_id).is_err());
    let mut unknown = serde_json::to_value(v2state).unwrap();
    unknown["desktop_sidecar"] = "unknown".into();
    fs::write(
        store.directory().join("state.json"),
        serde_json::to_vec(&unknown).unwrap(),
    )
    .unwrap();
    assert!(ConversationStore::open(temp.path(), "project", session_id).is_err());
}

#[test]
fn s07_child_lock_worker() {
    use std::io::BufRead;
    let Some(path) = std::env::var_os("POLARIS_DESKTOP_TEST_LOCK") else {
        return;
    };
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .unwrap();
    if std::env::var_os("POLARIS_DESKTOP_TEST_EXPECT_BUSY").is_some() {
        assert!(matches!(
            file.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        return;
    }
    file.try_lock().unwrap();
    println!("DESKTOP_LOCK_HELD");
    std::io::stdout().flush().unwrap();
    let mut line = String::new();
    std::io::stdin().lock().read_line(&mut line).unwrap();
    assert_eq!(line.trim(), "exit");
    // RustのDropを通さず終了し、OSによる解放を親が確認する。
    std::process::exit(0);
}

#[test]
fn s07_child_process_contention_and_exit_release_real_writer_lock() {
    use std::{
        io::{BufRead, BufReader},
        process::{Command, Stdio},
    };
    let (root, writer) = setup();
    let path = dir(&writer).join("writer.lock");
    let executable = std::env::current_exe().unwrap();
    let child_args = [
        "--exact",
        "desktop_store::tests::s07_child_lock_worker",
        "--nocapture",
    ];
    let output = Command::new(&executable)
        .args(child_args)
        .env("POLARIS_DESKTOP_TEST_LOCK", &path)
        .env("POLARIS_DESKTOP_TEST_EXPECT_BUSY", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    drop(writer);
    let mut child = Command::new(executable)
        .args(child_args)
        .env("POLARIS_DESKTOP_TEST_LOCK", &path)
        .env_remove("POLARIS_DESKTOP_TEST_EXPECT_BUSY")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut reader = BufReader::new(child.stdout.take().unwrap());
    let mut line = String::new();
    loop {
        line.clear();
        assert!(
            reader.read_line(&mut line).unwrap() > 0,
            "子がlock取得前に終了"
        );
        if line.trim() == "DESKTOP_LOCK_HELD" {
            break;
        }
    }
    let blocked = matches!(root.open(&project(), &session()), Err(StoreError::Busy));
    child.stdin.take().unwrap().write_all(b"exit\n").unwrap();
    assert!(child.wait().unwrap().success());
    assert!(blocked);
    assert!(root.open(&project(), &session()).is_ok());
}

#[test]
fn s05_raw_blank_line_and_partial_line_rejected_even_with_matching_hash() {
    use crate::conversation_state::content_hash;
    for bytes in [b"\n".as_slice(), b"{".as_slice()] {
        let (root, writer) = setup();
        let path = dir(&writer);
        let mut marker = writer.snapshot().unwrap().marker;
        drop(writer);
        marker.raw_hash = content_hash(bytes);
        marker.raw_offset = DecimalU64::new(bytes.len() as u64);
        fs::write(path.join(disk::RAW), bytes).unwrap();
        fs::write(
            path.join(disk::MARKER),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
        assert!(root.open(&project(), &session()).is_err());
    }
}

#[test]
fn s05_wrong_project_and_symlink_are_rejected() {
    let (root, writer) = setup();
    let path = dir(&writer);
    drop(writer);
    assert!(matches!(
        root.open(&ProjectId::new("wrong").unwrap(), &session()),
        Err(StoreError::TargetMismatch)
    ));
    #[cfg(unix)]
    {
        let original = path.join(disk::RAW);
        let moved = path.join("raw-original");
        fs::rename(&original, &moved).unwrap();
        std::os::unix::fs::symlink(moved, original).unwrap();
        assert!(matches!(
            root.open(&project(), &session()),
            Err(StoreError::Corrupt(_))
        ));
    }
}

#[test]
fn s06_reopen_invalidates_old_approvals_in_new_generation() {
    let (root, mut writer) = setup();
    writer.apply(&start("start", 0), Some(target())).unwrap();
    let approval = PendingApproval {
        approval_id: ApprovalId::new("approval").unwrap(),
        run_id: target().run_id,
        attempt_id: target().attempt_id,
        operation_id: op(),
        operation: "fake".into(),
        scope: "fixture".into(),
        payload_hash: crate::conversation_state::content_hash(b"fixture"),
        policy_revision: DecimalU64::new(7),
        expires_at_unix_ms: DecimalU64::new(u64::MAX),
        state: PendingApprovalState::Pending,
        display: ApprovalDisplay {
            title: "fixture".into(),
            description: "fixture".into(),
            choices: vec![ApprovalDecision::Deny],
        },
    };
    writer.test_publish_approval(approval);
    let before = writer.snapshot().unwrap();
    assert_eq!(before.state.unresolved_approvals.len(), 1);
    drop(writer);
    let reopened = root
        .open(&project(), &session())
        .unwrap()
        .snapshot()
        .unwrap();
    assert!(reopened.state.unresolved_approvals.is_empty());
    assert_eq!(
        reopened.marker.session_revision,
        before.marker.session_revision.checked_add(1).unwrap()
    );
    assert_eq!(
        reopened.marker.content_revision,
        before.marker.content_revision
    );
}

#[test]
fn s07_tombstone_rename_ambiguity_cannot_revive_session() {
    let (root, mut writer) = setup();
    inject(Some("directory_sync"));
    assert!(writer.tombstone().is_err());
    inject(None);
    assert!(matches!(
        writer.snapshot(),
        Err(StoreError::RecoveryRequired)
    ));
    assert!(matches!(writer.recover(), Err(StoreError::Deleted)));
    drop(writer);
    assert!(matches!(
        root.open(&project(), &session()),
        Err(StoreError::Deleted)
    ));
    assert!(root.create(project(), session(), initial()).is_err());
}

#[test]
fn s02_revision_overflow_does_not_publish_or_consume_request() {
    let root = PrototypeRoot::new().unwrap();
    let mut initial = initial();
    initial.draft.draft_revision = DecimalU64::new(u64::MAX);
    let mut writer = root.create(project(), session(), initial).unwrap();
    let before = writer.snapshot().unwrap();
    assert!(matches!(
        writer.apply(&draft("d", u64::MAX, "overflow"), None),
        Err(StoreError::Overflow)
    ));
    unchanged(&before, &writer.snapshot().unwrap());
}

#[test]
fn s05_valid_complete_unpublished_events_are_not_recovered() {
    use crate::conversation_state::RawEventV2;
    let (root, mut writer) = setup();
    writer.apply(&draft("d", 0, "visible"), None).unwrap();
    let before = writer.snapshot().unwrap();
    let raw_event = RawEventV2 {
        schema_version: 2,
        sequence: 1,
        epoch: 1,
        turn_id: 1,
        starts_turn: true,
        message: polaris_provider::Message::user("never published"),
    };
    let mut state = before.state.clone();
    state.session_revision = state.session_revision.checked_add(1).unwrap();
    state.draft.text = "never published".into();
    let path = dir(&writer);
    drop(writer);
    for (name, bytes) in [
        (disk::RAW, disk::line(&raw_event).unwrap()),
        (disk::SIDECAR, disk::line(&state).unwrap()),
    ] {
        fs::OpenOptions::new()
            .append(true)
            .open(path.join(name))
            .unwrap()
            .write_all(&bytes)
            .unwrap();
    }
    let mut reopened = root.open(&project(), &session()).unwrap();
    unchanged(&before, &reopened.snapshot().unwrap());
    reopened.apply(&start("start", 1), Some(target())).unwrap();
    let after = reopened.snapshot().unwrap();
    assert_eq!(after.raw.len(), 1);
    assert_eq!(after.raw[0].message.content, "visible");
    assert_eq!(after.state.requests.len(), 2);
}

#[test]
fn s06_operation_result_publication_controls_recovery_uncertainty() {
    for point in ["marker_rename", "directory_sync"] {
        let (root, mut writer) = setup();
        let req = start("start", 0);
        writer.apply(&req, Some(target())).unwrap();
        writer.record_intent(&target(), op()).unwrap();
        inject(Some(point));
        assert!(
            writer
                .record_operation_result(&target(), &op(), result())
                .is_err()
        );
        assert!(matches!(
            writer.snapshot(),
            Err(StoreError::RecoveryRequired)
        ));
        assert!(matches!(
            writer.request_status(&session(), &req.client_id, &req.request_id),
            Err(StoreError::RecoveryRequired)
        ));
        assert!(matches!(
            writer.apply(&req, None),
            Err(StoreError::RecoveryRequired)
        ));
        inject(Some("recovery_sync"));
        drop(writer);
        assert!(root.open(&project(), &session()).is_err());
        inject(None);
        let reopened = root.open(&project(), &session()).unwrap();
        let snapshot = reopened.snapshot().unwrap();
        let record = &snapshot.state.runs[0];
        assert_eq!(
            record.run.state,
            if point == "marker_rename" {
                RunState::OutcomeUnknown
            } else {
                RunState::Interrupted
            }
        );
        assert_eq!(
            record.operations[0].result_id.is_some(),
            point == "directory_sync"
        );
    }
}

#[test]
fn p3_configuration_replay_returns_original_values_after_later_update_and_reopen() {
    let (root, mut writer) = setup();
    let a = configure("a", 0);
    let accepted = writer.apply(&a, None).unwrap();
    let mut b = configure("b", 1);
    if let RequestBody::SessionConfigure(_, params) = &mut b.body {
        params.model = "later".into();
    }
    writer.apply(&b, None).unwrap();
    drop(writer);
    let mut writer = root.open(&project(), &session()).unwrap();
    let before = writer.snapshot().unwrap();
    assert_eq!(writer.apply(&a, None).unwrap(), accepted);
    assert!(
        matches!(accepted.record.result, RequestResult::Configured { configuration, .. } if configuration.model == "model2")
    );
    unchanged(&before, &writer.snapshot().unwrap());
    assert_eq!(before.state.configuration.model, "later");
}

#[test]
fn p3_cancel_ledger_is_atomic_idempotent_and_prevents_start() {
    for started in [false, true] {
        let (root, mut writer) = setup();
        writer.apply(&start("s", 0), Some(target())).unwrap();
        if started {
            writer.record_intent(&target(), op()).unwrap();
        }
        let cancel = request("c", RequestBody::RunCancel(session(), target()));
        writer.apply(&cancel, None).unwrap();
        let before = writer.snapshot().unwrap();
        assert_eq!(
            before.state.runs[0].run.state,
            if started {
                RunState::Cancelling
            } else {
                RunState::Cancelled
            }
        );
        assert_eq!(
            before.state.requests.last().unwrap().accepted_revision,
            before.marker.session_revision
        );
        assert!(
            writer
                .record_intent(&target(), OperationId::new("late").unwrap())
                .is_err()
        );
        let ack = writer.apply(&cancel, None).unwrap().record;
        unchanged(&before, &writer.snapshot().unwrap());
        drop(writer);
        let mut writer = root.open(&project(), &session()).unwrap();
        assert_eq!(writer.apply(&cancel, None).unwrap().record, ack);
        assert_eq!(writer.snapshot().unwrap().raw.len(), 1);
    }
}

#[test]
fn p3_cancel_faults_do_not_publish_half_a_ledger() {
    for point in ["sidecar_partial", "marker_rename", "directory_sync"] {
        let (root, mut writer) = setup();
        writer.apply(&start("s", 0), Some(target())).unwrap();
        writer.record_intent(&target(), op()).unwrap();
        let cancel = request("c", RequestBody::RunCancel(session(), target()));
        inject(Some(point));
        assert!(writer.apply(&cancel, None).is_err(), "{point}");
        assert!(matches!(
            writer.snapshot(),
            Err(StoreError::RecoveryRequired)
        ));
        inject(None);
        writer.recover().unwrap();
        let p = writer.snapshot().unwrap();
        let accepted = writer
            .request_status(&session(), &cancel.client_id, &cancel.request_id)
            .unwrap()
            .is_some();
        assert_eq!(accepted, point == "directory_sync");
        assert_eq!(p.state.runs[0].run.state == RunState::Cancelling, accepted);
        drop(writer);
        drop(root);
    }
}

#[test]
fn p3_assistant_body_result_and_terminal_share_one_publication() {
    let (root, mut writer) = setup();
    writer.apply(&start("s", 0), Some(target())).unwrap();
    writer.record_intent(&target(), op()).unwrap();
    let before = writer.snapshot().unwrap();
    writer
        .finish_with_text(
            &target(),
            &op(),
            Observation::Succeeded,
            result(),
            "保存した回答🦀".into(),
        )
        .unwrap();
    let saved = writer.snapshot().unwrap();
    assert_eq!(
        saved.marker.session_revision.get(),
        before.marker.session_revision.get() + 1
    );
    assert_eq!(
        saved.marker.content_revision.get(),
        before.marker.content_revision.get() + 1
    );
    assert_eq!(saved.raw[1].message.content, "保存した回答🦀");
    assert_eq!(saved.state.runs[0].result_id, Some(result()));
    assert_eq!(saved.state.runs[0].operations[0].result_id, Some(result()));
    assert_eq!(saved.state.runs[0].run.state, RunState::Succeeded);
    writer
        .finish_with_text(
            &target(),
            &op(),
            Observation::Succeeded,
            result(),
            "保存した回答🦀".into(),
        )
        .unwrap();
    unchanged(&saved, &writer.snapshot().unwrap());
    assert!(
        writer
            .finish_with_text(
                &target(),
                &op(),
                Observation::Succeeded,
                result(),
                "違う".into()
            )
            .is_err()
    );
    drop(writer);
    let writer = root.open(&project(), &session()).unwrap();
    unchanged(&saved, &writer.snapshot().unwrap());
}

#[test]
fn p3_final_body_faults_never_expose_success_without_text() {
    for point in [
        "raw_partial",
        "raw_sync",
        "sidecar_partial",
        "sidecar_sync",
        "marker_partial",
        "marker_sync",
        "marker_rename",
        "directory_sync",
    ] {
        let (root, mut writer) = setup();
        writer.apply(&start("s", 0), Some(target())).unwrap();
        writer.record_intent(&target(), op()).unwrap();
        inject(Some(point));
        assert!(
            writer
                .finish_with_text(
                    &target(),
                    &op(),
                    Observation::Succeeded,
                    result(),
                    "本文".into()
                )
                .is_err(),
            "{point}"
        );
        assert!(matches!(
            writer.snapshot(),
            Err(StoreError::RecoveryRequired)
        ));
        inject(None);
        drop(writer);
        let writer = root.open(&project(), &session()).unwrap();
        let saved = writer.snapshot().unwrap();
        let published = point == "directory_sync";
        assert_eq!(saved.raw.len(), if published { 2 } else { 1 });
        assert_eq!(saved.state.runs[0].result_id.is_some(), published);
        assert_eq!(
            saved.state.runs[0].run.state,
            if published {
                RunState::Succeeded
            } else {
                RunState::OutcomeUnknown
            }
        );
    }
}

#[test]
fn p3_terminal_replay_requires_the_same_saved_operation() {
    let (_, mut writer) = setup();
    writer.apply(&start("s", 0), Some(target())).unwrap();
    writer.record_intent(&target(), op()).unwrap();
    writer
        .finish_with_text(
            &target(),
            &op(),
            Observation::Succeeded,
            result(),
            "本文".into(),
        )
        .unwrap();
    let before = writer.snapshot().unwrap();
    assert!(
        writer
            .finish_with_text(
                &target(),
                &OperationId::new("other").unwrap(),
                Observation::Succeeded,
                result(),
                "本文".into()
            )
            .is_err()
    );
    unchanged(&before, &writer.snapshot().unwrap());
}

fn desktop_tool_turn() -> Vec<polaris_provider::Message> {
    use polaris_provider::{Message, ToolCall};
    vec![
        Message::assistant_with_tool_calls(
            "確認します",
            vec![ToolCall {
                id: "call-1".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "example.txt"}),
            }],
        ),
        Message::tool_result("call-1", "dummy result"),
        Message::assistant("確認できました"),
    ]
}

#[test]
fn desktop_tool_suffix_survives_reopen_and_replay_checks_all_fields() {
    let (root, mut writer) = setup();
    writer.apply(&start("s", 0), Some(target())).unwrap();
    writer.record_intent(&target(), op()).unwrap();
    let messages = desktop_tool_turn();
    writer
        .finish_with_messages(
            &target(),
            &op(),
            Observation::Succeeded,
            result(),
            messages.clone(),
        )
        .unwrap();
    let saved = writer.snapshot().unwrap();
    assert_eq!(saved.raw.len(), 4);
    assert_eq!(saved.raw[2].message.tool_call_id.as_deref(), Some("call-1"));
    assert_eq!(saved.state.runs[0].run.state, RunState::Succeeded);
    writer
        .finish_with_messages(
            &target(),
            &op(),
            Observation::Succeeded,
            result(),
            messages.clone(),
        )
        .unwrap();
    unchanged(&saved, &writer.snapshot().unwrap());
    let mut changed = messages;
    changed[0].tool_calls[0].arguments = serde_json::json!({"path":"different.txt"});
    assert!(
        writer
            .finish_with_messages(&target(), &op(), Observation::Succeeded, result(), changed)
            .is_err()
    );
    unchanged(&saved, &writer.snapshot().unwrap());
    drop(writer);
    unchanged(
        &saved,
        &root
            .open(&project(), &session())
            .unwrap()
            .snapshot()
            .unwrap(),
    );
}

#[test]
fn desktop_tool_suffix_rejects_bad_order_user_injection_and_incomplete_success() {
    let (_, mut writer) = setup();
    writer.apply(&start("s", 0), Some(target())).unwrap();
    writer.record_intent(&target(), op()).unwrap();
    let before = writer.snapshot().unwrap();
    let turn = desktop_tool_turn();
    for messages in [
        vec![turn[1].clone(), turn[0].clone()],
        vec![polaris_provider::Message::user("injected")],
        vec![turn[0].clone()],
        vec![turn[0].clone(), turn[1].clone(), turn[1].clone()],
        vec![polaris_provider::Message::assistant(
            "x".repeat(4 * 1024 * 1024),
        )],
    ] {
        assert!(
            writer
                .finish_with_messages(&target(), &op(), Observation::Succeeded, result(), messages)
                .is_err()
        );
        unchanged(&before, &writer.snapshot().unwrap());
    }
    writer
        .finish_with_messages(
            &target(),
            &op(),
            Observation::Failed,
            result(),
            vec![turn[0].clone()],
        )
        .unwrap();
    assert_eq!(
        writer.snapshot().unwrap().raw[1].message.tool_calls.len(),
        1
    );
}

#[test]
fn desktop_tool_suffix_publication_faults_preserve_whole_turn_or_unknown() {
    for point in [
        "raw_partial",
        "raw_sync",
        "sidecar_partial",
        "sidecar_sync",
        "marker_partial",
        "marker_sync",
        "marker_rename",
        "directory_sync",
    ] {
        let (root, mut writer) = setup();
        writer.apply(&start("s", 0), Some(target())).unwrap();
        writer.record_intent(&target(), op()).unwrap();
        inject(Some(point));
        assert!(
            writer
                .finish_with_messages(
                    &target(),
                    &op(),
                    Observation::Succeeded,
                    result(),
                    desktop_tool_turn()
                )
                .is_err(),
            "{point}"
        );
        inject(None);
        drop(writer);
        let saved = root
            .open(&project(), &session())
            .unwrap()
            .snapshot()
            .unwrap();
        let published = point == "directory_sync";
        assert_eq!(saved.raw.len(), if published { 4 } else { 1 }, "{point}");
        assert_eq!(
            saved.state.runs[0].run.state,
            if published {
                RunState::Succeeded
            } else {
                RunState::OutcomeUnknown
            },
            "{point}"
        );
    }
}

#[test]
fn desktop_tool_suffix_bounds_escaped_history_text_without_truncation() {
    let (_, mut writer) = setup();
    writer.apply(&start("s", 0), Some(target())).unwrap();
    writer.record_intent(&target(), op()).unwrap();
    let before = writer.snapshot().unwrap();
    // Small UTF-8 payload, but every NUL needs six JSON bytes.
    assert!(
        writer
            .finish_with_text(
                &target(),
                &op(),
                Observation::Succeeded,
                result(),
                "\0".repeat(22_000)
            )
            .is_err()
    );
    unchanged(&before, &writer.snapshot().unwrap());
    let text = "\0".repeat(20_000);
    writer
        .finish_with_text(
            &target(),
            &op(),
            Observation::Succeeded,
            result(),
            text.clone(),
        )
        .unwrap();
    assert_eq!(writer.snapshot().unwrap().raw[1].message.content, text);
}

fn workflow_fixture() -> SavedWorkflow {
    SavedWorkflow::capture(&crate::workflow::SessionWorkflow::new(
        crate::workflow::WorkflowConfig {
            enabled: true,
            always: vec!["builtin:workflow-core".into()],
            ..Default::default()
        },
    ))
}

#[test]
fn workflow_checkpoint_reopens_and_old_run_replays_after_configuration_changes() {
    let (root, mut writer) = setup();
    let initial_workflow = workflow_fixture();
    writer
        .configure_workflow(DecimalU64::new(0), Some(initial_workflow.clone()))
        .unwrap();
    writer
        .apply(&start("workflow-start", 0), Some(target()))
        .unwrap();
    let before = writer.snapshot().unwrap();
    assert_eq!(
        before.state.runs[0].workflow,
        Some(initial_workflow.clone())
    );
    assert!(matches!(
        writer.configure_workflow(before.marker.session_revision, None),
        Err(StoreError::Busy)
    ));
    writer.record_intent(&target(), op()).unwrap();
    let mut finished = initial_workflow;
    finished.state.phase = crate::workflow::Phase::Review;
    finished.state.revision = 1;
    writer
        .finish_with_messages_and_workflow(
            &target(),
            &op(),
            Observation::Succeeded,
            result(),
            desktop_tool_turn(),
            Some(finished.clone()),
        )
        .unwrap();
    let revision = writer.snapshot().unwrap().marker.session_revision;
    writer.configure_workflow(revision, None).unwrap();
    // A later setting must not alter idempotency for the completed run.
    writer
        .finish_with_messages_and_workflow(
            &target(),
            &op(),
            Observation::Succeeded,
            result(),
            desktop_tool_turn(),
            Some(finished.clone()),
        )
        .unwrap();
    assert!(
        writer
            .finish_with_messages_and_workflow(
                &target(),
                &op(),
                Observation::Succeeded,
                result(),
                desktop_tool_turn(),
                None
            )
            .is_err()
    );
    drop(writer);
    let reopened = root
        .open(&project(), &session())
        .unwrap()
        .snapshot()
        .unwrap();
    assert!(reopened.state.workflow.is_none());
    assert_eq!(reopened.state.runs[0].workflow, Some(finished));
}

#[test]
fn workflow_invalid_schema_is_rejected_before_publishing_terminal() {
    let (_, mut writer) = setup();
    writer
        .apply(&start("workflow-invalid", 0), Some(target()))
        .unwrap();
    writer.record_intent(&target(), op()).unwrap();
    let before = writer.snapshot().unwrap();
    for state in [true, false] {
        let mut saved = workflow_fixture();
        if state {
            saved.state.schema_version = 99;
        } else {
            saved.gates.schema_version = 99;
        }
        assert!(
            writer
                .finish_with_messages_and_workflow(
                    &target(),
                    &op(),
                    Observation::Succeeded,
                    result(),
                    desktop_tool_turn(),
                    Some(saved)
                )
                .is_err()
        );
        unchanged(&before, &writer.snapshot().unwrap());
    }
}

#[test]
fn workflow_restore_does_not_revalidate_unknown_or_stale_artifacts() {
    let mut saved = workflow_fixture();
    saved.gates.verification = Some(crate::workflow::VerificationEvidence {
        artifact_hash: "artifact-a".into(),
        passed: true,
        stale: false,
    });
    assert!(
        !saved
            .restore(Some("artifact-a"))
            .unwrap()
            .gates
            .verification
            .unwrap()
            .stale
    );
    assert!(
        saved
            .restore(None)
            .unwrap()
            .gates
            .verification
            .unwrap()
            .stale
    );
    assert!(
        saved
            .restore(Some("artifact-b"))
            .unwrap()
            .gates
            .verification
            .unwrap()
            .stale
    );
    saved.gates.verification.as_mut().unwrap().stale = true;
    assert!(
        saved
            .restore(Some("artifact-a"))
            .unwrap()
            .gates
            .verification
            .unwrap()
            .stale
    );
}

#[test]
fn rejected_output_is_durable_and_cannot_be_reported_as_success() {
    let (root, mut writer) = setup();
    writer
        .apply(&start("gap-start", 0), Some(target()))
        .unwrap();
    writer.record_intent(&target(), op()).unwrap();
    writer
        .record_history_gap(&target(), HistoryGap::OutputRejected)
        .unwrap();
    assert!(
        writer
            .finish_with_messages(
                &target(),
                &op(),
                Observation::Succeeded,
                result(),
                desktop_tool_turn()
            )
            .is_err()
    );
    writer
        .finish_with_messages(&target(), &op(), Observation::Failed, result(), vec![])
        .unwrap();
    drop(writer);
    let saved = root
        .open(&project(), &session())
        .unwrap()
        .snapshot()
        .unwrap();
    assert_eq!(
        saved.state.runs[0].history_gap,
        Some(HistoryGap::OutputRejected)
    );
    assert_eq!(saved.state.runs[0].run.state, RunState::Failed);
    assert_eq!(saved.raw.len(), 1);
}

fn pending() -> PendingApproval {
    PendingApproval {
        approval_id: ApprovalId::new("approval").unwrap(),
        run_id: target().run_id,
        attempt_id: target().attempt_id,
        operation_id: op(),
        operation: "fake".into(),
        scope: "fixture".into(),
        payload_hash: crate::conversation_state::content_hash(b"fixture"),
        policy_revision: DecimalU64::new(7),
        expires_at_unix_ms: DecimalU64::new(100),
        state: PendingApprovalState::Pending,
        display: ApprovalDisplay {
            title: "fixture".into(),
            description: "fixture".into(),
            choices: vec![ApprovalDecision::Allow, ApprovalDecision::Deny],
        },
    }
}
fn answer() -> Request {
    request(
        "answer",
        RequestBody::ApprovalResolve(
            session(),
            ApprovalResolve {
                approval_id: pending().approval_id,
                run_id: target().run_id,
                attempt_id: target().attempt_id,
                policy_revision: DecimalU64::new(7),
                decision: ApprovalDecision::Allow,
            },
        ),
    )
}
fn consume(writer: &mut Writer, now: u64) -> StoreResult<IntentReceipt> {
    let a = pending();
    writer.consume_approval_intent(
        &target(),
        &a.approval_id,
        &op(),
        "fake",
        &a.scope,
        &a.payload_hash,
        now,
    )
}
#[test]
fn p4_approval_is_published_answered_and_consumed_once() {
    let (_, mut w) = setup();
    w.apply(&start("s", 0), Some(target())).unwrap();
    w.publish_approval(pending(), 1).unwrap();
    assert_eq!(
        w.snapshot().unwrap().state.unresolved_approvals,
        vec![pending()]
    );
    let ack = w.resolve_approval(&answer(), 2).unwrap();
    let saved = w.snapshot().unwrap();
    assert!(saved.state.unresolved_approvals.is_empty());
    assert_eq!(w.resolve_approval(&answer(), 100).unwrap(), ack);
    unchanged(&saved, &w.snapshot().unwrap());
    assert_eq!(consume(&mut w, 3).unwrap(), IntentReceipt::NewlyPublished);
    assert_eq!(consume(&mut w, 4).unwrap(), IntentReceipt::AlreadyRecorded);
    assert_eq!(w.snapshot().unwrap().state.runs[0].operations.len(), 1);
}

#[test]
fn p4_binding_and_answer_conflicts_leave_ledger_unchanged() {
    let (_, mut w) = setup();
    w.apply(&start("s", 0), Some(target())).unwrap();
    for hash in [
        "".into(),
        "a".repeat(63),
        "a".repeat(65),
        "g".repeat(64),
        "A".repeat(64),
        "é".repeat(32),
    ] {
        let mut a = pending();
        a.payload_hash = hash;
        assert!(w.publish_approval(a, 1).is_err());
    }
    w.publish_approval(pending(), 1).unwrap();
    let before = w.snapshot().unwrap();
    assert!(w.publish_approval(pending(), 1).is_err());
    let mut duplicate = pending();
    duplicate.approval_id = ApprovalId::new("other").unwrap();
    assert!(w.publish_approval(duplicate, 1).is_err());
    for kind in 0..4 {
        let mut req = answer();
        if let RequestBody::ApprovalResolve(_, p) = &mut req.body {
            match kind {
                0 => p.run_id = RunId::new("wrong").unwrap(),
                1 => p.attempt_id = AttemptId::new("wrong").unwrap(),
                2 => p.policy_revision = DecimalU64::new(8),
                _ => p.approval_id = ApprovalId::new("wrong").unwrap(),
            }
        }
        assert!(w.resolve_approval(&req, 2).is_err());
    }
    assert!(w.resolve_approval(&answer(), 100).is_err());
    unchanged(&before, &w.snapshot().unwrap());
    w.resolve_approval(&answer(), 2).unwrap();
    let before = w.snapshot().unwrap();
    let mut req = answer();
    req.request_id = RequestId::new("other-answer").unwrap();
    assert!(w.resolve_approval(&req, 2).is_err());
    let mut req = answer();
    if let RequestBody::ApprovalResolve(_, p) = &mut req.body {
        p.decision = ApprovalDecision::Deny;
    }
    assert!(matches!(
        w.resolve_approval(&req, 2),
        Err(StoreError::RequestConflict)
    ));
    let a = pending();
    for kind in 0..6 {
        let mut t = target();
        let mut id = op();
        let mut operation = "fake";
        let mut scope = a.scope.as_str();
        let mut hash = a.payload_hash.as_str();
        match kind {
            0 => t.run_id = RunId::new("other").unwrap(),
            1 => t.attempt_id = AttemptId::new("other").unwrap(),
            2 => id = OperationId::new("other").unwrap(),
            3 => operation = "other",
            4 => scope = "other",
            _ => hash = "other",
        }
        assert!(
            w.consume_approval_intent(&t, &a.approval_id, &id, operation, scope, hash, 3)
                .is_err()
        );
    }
    assert!(w.record_intent(&target(), op()).is_err());
    unchanged(&before, &w.snapshot().unwrap());
    assert_eq!(consume(&mut w, 3).unwrap(), IntentReceipt::NewlyPublished);
}

#[test]
fn p4_invalidation_never_restores_old_allow() {
    for action in 0..7 {
        let (root, mut w) = setup();
        w.apply(&start("s", 0), Some(target())).unwrap();
        w.publish_approval(pending(), 1).unwrap();
        w.resolve_approval(&answer(), 2).unwrap();
        match action {
            0 => {
                w.set_policy_revision(DecimalU64::new(8)).unwrap();
                assert!(w.set_policy_revision(DecimalU64::new(7)).is_err());
                let mut a = pending();
                a.policy_revision = DecimalU64::new(8);
                a.approval_id = ApprovalId::new("new").unwrap();
                a.operation_id = OperationId::new("new").unwrap();
                assert!(w.publish_approval(a, 3).is_err());
            }
            1 => {
                w.apply(
                    &request("cancel", RequestBody::RunCancel(session(), target())),
                    None,
                )
                .unwrap();
            }
            2 => {
                assert_eq!(
                    w.expire_approvals(100).unwrap(),
                    vec![pending().approval_id]
                );
                assert!(w.expire_approvals(101).unwrap().is_empty());
            }
            3 => {
                w.invalidate_approval(&pending().approval_id).unwrap();
                let before = w.snapshot().unwrap();
                w.invalidate_approval(&pending().approval_id).unwrap();
                unchanged(&before, &w.snapshot().unwrap());
            }
            4 => {
                drop(w);
                w = root.open(&project(), &session()).unwrap();
            }
            5 => {
                w.finish(&target(), Observation::Interrupted, result())
                    .unwrap();
            }
            _ => {
                assert!(consume(&mut w, 100).is_err());
            }
        }
        assert!(consume(&mut w, if action == 6 { 100 } else { 3 }).is_err());
        assert!(w.snapshot().unwrap().state.runs[0].operations.is_empty());
        assert!(w.publish_approval(pending(), 3).is_err());
    }
}

#[test]
fn p4_all_approval_save_faults_return_no_execution_permission() {
    for stage in 0..4 {
        for point in [
            "sidecar_partial",
            "sidecar_sync",
            "marker_partial",
            "marker_sync",
            "marker_rename",
            "directory_sync",
        ] {
            let (_, mut w) = setup();
            w.apply(&start("s", 0), Some(target())).unwrap();
            if stage > 0 {
                w.publish_approval(pending(), 1).unwrap();
            }
            if stage > 1 {
                w.resolve_approval(&answer(), 2).unwrap();
            }
            inject(Some(point));
            let failed = match stage {
                0 => w.publish_approval(pending(), 1).is_err(),
                1 => w.resolve_approval(&answer(), 2).is_err(),
                2 => consume(&mut w, 3).is_err(),
                _ => w.invalidate_approval(&pending().approval_id).is_err(),
            };
            assert!(failed, "{stage}/{point}");
            assert!(matches!(
                consume(&mut w, 3),
                Err(StoreError::RecoveryRequired)
            ));
            inject(None);
            w.recover().unwrap();
            let saved = w.snapshot().unwrap();
            if stage == 1 {
                let answered = point == "directory_sync";
                assert_eq!(saved.state.approval_records[0].decision.is_some(), answered);
                assert_eq!(saved.state.unresolved_approvals.is_empty(), answered);
                assert_eq!(
                    w.request_status(&session(), &answer().client_id, &answer().request_id)
                        .unwrap()
                        .is_some(),
                    answered
                );
            }
            if stage == 2 {
                assert_eq!(
                    saved.state.approval_records[0].consumed,
                    point == "directory_sync"
                );
                assert_eq!(
                    saved.state.runs[0].operations.len(),
                    usize::from(point == "directory_sync")
                );
                if point == "directory_sync" {
                    assert_eq!(consume(&mut w, 3).unwrap(), IntentReceipt::AlreadyRecorded);
                }
            }
        }
    }
}

#[test]
fn p4_deny_and_pending_invalidation_cannot_start() {
    for deny in [false, true] {
        let (_, mut w) = setup();
        w.apply(&start("s", 0), Some(target())).unwrap();
        w.publish_approval(pending(), 1).unwrap();
        if deny {
            let mut req = answer();
            if let RequestBody::ApprovalResolve(_, p) = &mut req.body {
                p.decision = ApprovalDecision::Deny;
            }
            let ack = w.resolve_approval(&req, 2).unwrap();
            assert_eq!(w.resolve_approval(&req, 3).unwrap(), ack);
        } else {
            w.invalidate_approval(&pending().approval_id).unwrap();
            assert!(w.resolve_approval(&answer(), 2).is_err());
        }
        assert!(w.snapshot().unwrap().state.unresolved_approvals.is_empty());
        assert!(consume(&mut w, 3).is_err());
        assert!(w.snapshot().unwrap().state.runs[0].operations.is_empty());
    }
}

fn usage_report() -> polaris_provider::UsageReport {
    polaris_provider::UsageReport {
        usage: polaris_provider::Usage {
            input_tokens: 20,
            output_tokens: 10,
            total_tokens: 30,
            cached_tokens: 5,
        },
        reported_responses: 3,
        missing_responses: 2,
        failed_requests: 1,
    }
}

#[test]
fn p4_usage_none_and_observed_zero_remain_distinct_after_reopen() {
    for observed in [None, Some(polaris_provider::UsageReport::default())] {
        let (root, mut writer) = setup();
        writer
            .apply(&start("usage-start", 0), Some(target()))
            .unwrap();
        let before = writer.snapshot().unwrap();
        assert_eq!(before.state.runs[0].usage, None);
        if let Some(zero) = observed {
            writer.record_usage(&target(), zero).unwrap();
            let saved = writer.snapshot().unwrap();
            assert_eq!(saved.state.runs[0].usage, Some(zero));
            assert!(saved.marker.session_revision > before.marker.session_revision);
            assert_eq!(
                saved.marker.content_revision,
                before.marker.content_revision
            );
        }
        drop(writer);
        let reopened = root.open(&project(), &session()).unwrap();
        assert_eq!(reopened.snapshot().unwrap().state.runs[0].usage, observed);
    }
}

#[test]
fn p4_usage_cumulative_checkpoints_replace_and_equal_replay_is_noop() {
    let (root, mut writer) = setup();
    writer
        .apply(&start("usage-start", 0), Some(target()))
        .unwrap();
    let prior = usage_report();
    writer.record_usage(&target(), prior).unwrap();
    let before = writer.snapshot().unwrap();
    writer.record_usage(&target(), prior).unwrap();
    unchanged(&before, &writer.snapshot().unwrap());

    let next = polaris_provider::UsageReport {
        usage: polaris_provider::Usage {
            input_tokens: 25,
            output_tokens: 12,
            total_tokens: 37,
            cached_tokens: 6,
        },
        reported_responses: 4,
        missing_responses: 3,
        failed_requests: 2,
    };
    writer.record_usage(&target(), next).unwrap();
    let saved = writer.snapshot().unwrap();
    assert_eq!(saved.state.runs[0].usage, Some(next));
    assert_eq!(
        saved.marker.content_revision,
        before.marker.content_revision
    );
    assert_eq!(saved.marker.raw_hash, before.marker.raw_hash);
    drop(writer);
    let mut reopened = root.open(&project(), &session()).unwrap();
    let recovered = reopened.snapshot().unwrap();
    assert_eq!(recovered.state.runs[0].usage, Some(next));
    reopened.record_usage(&target(), next).unwrap();
    unchanged(&recovered, &reopened.snapshot().unwrap());
}

#[test]
fn p4_usage_each_counter_decrease_is_rejected_without_publication() {
    let (_root, mut writer) = setup();
    writer
        .apply(&start("usage-start", 0), Some(target()))
        .unwrap();
    let prior = usage_report();
    writer.record_usage(&target(), prior).unwrap();
    let before = writer.snapshot().unwrap();
    for field in 0..7 {
        let mut older = prior;
        match field {
            0 => older.reported_responses -= 1,
            1 => older.missing_responses -= 1,
            2 => older.failed_requests -= 1,
            3 => older.usage.input_tokens -= 1,
            4 => older.usage.output_tokens -= 1,
            5 => older.usage.total_tokens -= 1,
            _ => older.usage.cached_tokens -= 1,
        }
        assert!(
            matches!(
                writer.record_usage(&target(), older),
                Err(StoreError::RunConflict)
            ),
            "counter {field}"
        );
        unchanged(&before, &writer.snapshot().unwrap());
    }
}

#[test]
fn p4_usage_save_faults_require_recovery_and_preserve_publication_boundary() {
    for point in [
        "sidecar_partial",
        "sidecar_sync",
        "marker_partial",
        "marker_sync",
        "marker_rename",
        "directory_sync",
    ] {
        for prior in [None, Some(polaris_provider::UsageReport::default())] {
            let (root, mut writer) = setup();
            writer
                .apply(&start("usage-start", 0), Some(target()))
                .unwrap();
            if let Some(report) = prior {
                writer.record_usage(&target(), report).unwrap();
            }
            let next = usage_report();
            inject(Some(point));
            let failed = writer.record_usage(&target(), next);
            inject(None);
            assert!(failed.is_err(), "{point}");
            assert!(
                matches!(writer.snapshot(), Err(StoreError::RecoveryRequired)),
                "{point}"
            );
            assert!(
                matches!(
                    writer.record_usage(&target(), next),
                    Err(StoreError::RecoveryRequired)
                ),
                "{point}"
            );
            writer.recover().unwrap();
            let recovered = writer.snapshot().unwrap();
            assert_eq!(
                recovered.state.runs[0].usage,
                if point == "directory_sync" {
                    Some(next)
                } else {
                    prior
                },
                "{point}"
            );
            drop(writer);
            let mut reopened = root.open(&project(), &session()).unwrap();
            assert_eq!(
                reopened.snapshot().unwrap().state.runs[0].usage,
                recovered.state.runs[0].usage,
                "{point}"
            );
            reopened.record_usage(&target(), next).unwrap();
            let saved = reopened.snapshot().unwrap();
            assert_eq!(saved.state.runs[0].usage, Some(next));
            reopened.record_usage(&target(), next).unwrap();
            unchanged(&saved, &reopened.snapshot().unwrap());
        }
    }
}

#[test]
fn durable_root_reopens_without_deleting_and_preserves_raw_and_revisions() {
    use super::writer::DesktopRoot;
    let temp = tempfile::tempdir().unwrap();
    let path = fs::canonicalize(temp.path()).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let root = DesktopRoot::open_owned(&path).unwrap();
    let mut writer = root.create(project(), session(), initial()).unwrap();
    writer
        .apply(&start("durable-run", 0), Some(target()))
        .unwrap();
    let before = writer.snapshot().unwrap();
    assert!(!before.raw.is_empty());
    assert!(matches!(
        root.open(&project(), &session()),
        Err(StoreError::Busy)
    ));
    drop(writer);
    drop(root);
    assert!(path.is_dir());
    let reopened = DesktopRoot::open_owned(&path)
        .unwrap()
        .open(&project(), &session())
        .unwrap();
    let after = reopened.snapshot().unwrap();
    assert_eq!(
        serde_json::to_value(&after.raw).unwrap(),
        serde_json::to_value(&before.raw).unwrap()
    );
    assert_eq!(
        after.marker.content_revision,
        before.marker.content_revision
    );
    assert!(after.marker.session_revision > before.marker.session_revision);
    assert_eq!(after.state.runs[0].run.state, RunState::Interrupted);
}

#[test]
fn durable_root_rejects_unsafe_paths_permissions_and_partial_session() {
    use super::writer::DesktopRoot;
    use std::os::unix::fs::{PermissionsExt, symlink};
    let temp = tempfile::tempdir().unwrap();
    let path = fs::canonicalize(temp.path()).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    assert!(DesktopRoot::open_owned(std::path::Path::new("relative")).is_err());
    let link = path.join("link");
    symlink(&path, &link).unwrap();
    assert!(DesktopRoot::open_owned(&link).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    assert!(DesktopRoot::open_owned(&path).is_err());
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    let root = DesktopRoot::open_owned(&path).unwrap();
    let session_dir = path.join(crate::conversation_state::content_hash(
        session().as_str().as_bytes(),
    ));
    fs::create_dir(&session_dir).unwrap();
    fs::set_permissions(&session_dir, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(
        root.open_or_create(project(), session(), initial())
            .is_err()
    );
    assert!(!session_dir.join("state.json").exists());
}

#[test]
fn durable_writer_rejects_root_replacement_and_lock_replacement() {
    use super::writer::DesktopRoot;
    let temp = tempfile::tempdir().unwrap();
    let path = fs::canonicalize(temp.path()).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let root_path = path.join("owned");
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&root_path)
        .unwrap();
    let root = DesktopRoot::open_owned(&root_path).unwrap();
    let writer = root.create(project(), session(), initial()).unwrap();
    let lock = dir(&writer).join("writer.lock");
    fs::rename(&lock, lock.with_extension("old")).unwrap();
    use std::os::unix::fs::OpenOptionsExt;
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lock)
        .unwrap();
    assert!(matches!(
        writer.snapshot(),
        Err(StoreError::RecoveryRequired)
    ));
    drop(writer);
    let writer = root.open(&project(), &session()).unwrap();
    fs::rename(&root_path, path.join("moved")).unwrap();
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&root_path)
        .unwrap();
    assert!(writer.snapshot().is_err());
    assert!(
        root.create(project(), SessionId::new("new").unwrap(), initial())
            .is_err()
    );
    assert_eq!(fs::read_dir(&root_path).unwrap().count(), 0);
}

#[test]
fn durable_writer_nofollow_rejects_symlink_and_hardlinked_log() {
    use super::writer::DesktopRoot;
    use std::os::unix::fs::symlink;
    let temp = tempfile::tempdir().unwrap();
    let path = fs::canonicalize(temp.path()).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let root = DesktopRoot::open_owned(&path).unwrap();
    let writer = root.create(project(), session(), initial()).unwrap();
    let raw = dir(&writer).join(disk::RAW);
    let original = path.join("original");
    drop(writer);
    fs::rename(&raw, &original).unwrap();
    symlink(&original, &raw).unwrap();
    assert!(root.open(&project(), &session()).is_err());
    fs::remove_file(&raw).unwrap();
    fs::hard_link(&original, &raw).unwrap();
    assert!(root.open(&project(), &session()).is_err());
}

fn role_catalog() -> Vec<polaris_skills::AgentType> {
    vec![polaris_skills::AgentType {
        name: "reviewer".into(),
        description: String::new(),
        body: String::new(),
        path: "/never-read".into(),
        allowed_tools: vec![],
        access: polaris_skills::AgentAccess::Read,
        tier: String::new(),
        wall_seconds: 1,
        max_turns: 1,
        workflow_phase: None,
        continuation: false,
        output_schema: "/never-read/schema.json".into(),
    }]
}
fn saved_roles_fixture() -> SavedRoleBindings {
    SavedRoleBindings::new(vec![SavedRoleBinding {
        role: "reviewer".into(),
        runtime: SavedRuntime::Ollama,
        endpoint: "http://127.0.0.1:11434".into(),
        model: "fixture".into(),
        observed_tool_support: ToolSupport::Unknown,
    }])
    .unwrap()
}
fn start_with_configuration(id: &str, revision: u64) -> Request {
    let mut request = start(id, 0);
    if let RequestBody::RunStart(_, params) = &mut request.body {
        params.expected_configuration_revision = DecimalU64::new(revision);
    }
    request
}

#[test]
fn saved_roles_configuration_cas_validation_and_content_preservation() {
    let (_, mut writer) = setup();
    let before = writer.snapshot().unwrap();
    let roles = saved_roles_fixture();
    assert!(
        writer
            .configure_role_bindings(DecimalU64::new(0), Some(roles.clone()), &[])
            .is_err()
    );
    unchanged(&before, &writer.snapshot().unwrap());
    writer
        .configure_role_bindings(DecimalU64::new(0), Some(roles.clone()), &role_catalog())
        .unwrap();
    let saved = writer.snapshot().unwrap();
    assert_eq!(saved.state.role_bindings, Some(roles));
    assert_eq!(saved.state.configuration.configuration_revision.get(), 1);
    assert_eq!(
        saved.marker.session_revision.get(),
        before.marker.session_revision.get() + 1
    );
    assert_eq!(
        saved.marker.content_revision,
        before.marker.content_revision
    );
    assert_eq!(saved.marker.raw_hash, before.marker.raw_hash);
    assert_eq!(saved.state.draft, before.state.draft);
    assert!(matches!(
        writer.configure_role_bindings(DecimalU64::new(0), None, &[]),
        Err(StoreError::CasConflict("configuration"))
    ));
    assert!(matches!(
        writer.apply(&start("stale-role-config", 0), Some(target())),
        Err(StoreError::CasConflict("configuration"))
    ));
    unchanged(&saved, &writer.snapshot().unwrap());
    let mut invalid = saved_roles_fixture();
    invalid.schema_version = 2;
    assert!(
        writer
            .configure_role_bindings(DecimalU64::new(1), Some(invalid), &role_catalog())
            .is_err()
    );
    unchanged(&saved, &writer.snapshot().unwrap());
}

#[test]
fn saved_roles_active_update_freezes_run_and_terminal_reopen_preserves_both() {
    let (root, mut writer) = setup();
    let roles = saved_roles_fixture();
    writer
        .configure_role_bindings(DecimalU64::new(0), Some(roles.clone()), &role_catalog())
        .unwrap();
    let accepted = start_with_configuration("role-run", 1);
    writer.apply(&accepted, Some(target())).unwrap();
    writer.record_intent(&target(), op()).unwrap();
    let empty = SavedRoleBindings::new(vec![]).unwrap();
    writer
        .configure_role_bindings(DecimalU64::new(1), Some(empty.clone()), &[])
        .unwrap();
    let active = writer.snapshot().unwrap();
    assert_eq!(active.state.configuration.configuration_revision.get(), 2);
    assert_eq!(
        active.state.runs[0]
            .configuration
            .configuration_revision
            .get(),
        1
    );
    assert_eq!(active.state.runs[0].role_bindings, Some(roles.clone()));
    writer
        .finish_with_messages_and_workflow(
            &target(),
            &op(),
            Observation::Succeeded,
            result(),
            desktop_tool_turn(),
            None,
        )
        .unwrap();
    // Replayed acceptance must retain the original roles despite a newer next-run setting.
    assert_eq!(
        writer
            .apply(&accepted, Some(target()))
            .unwrap()
            .run
            .unwrap()
            .role_bindings,
        Some(roles.clone())
    );
    let terminal = writer.snapshot().unwrap();
    assert_eq!(terminal.state.role_bindings, Some(empty.clone()));
    assert_eq!(terminal.state.runs[0].role_bindings, Some(roles.clone()));
    drop(writer);
    let mut reopened = root.open(&project(), &session()).unwrap();
    let saved = reopened.snapshot().unwrap();
    assert_eq!(saved.state.role_bindings, Some(empty));
    assert_eq!(saved.state.runs[0].role_bindings, Some(roles.clone()));
    reopened
        .configure_role_bindings(DecimalU64::new(2), None, &[])
        .unwrap();
    reopened
        .finish_with_messages_and_workflow(
            &target(),
            &op(),
            Observation::Succeeded,
            result(),
            desktop_tool_turn(),
            None,
        )
        .unwrap();
    assert!(reopened.snapshot().unwrap().state.role_bindings.is_none());
    assert_eq!(
        reopened.snapshot().unwrap().state.runs[0].role_bindings,
        Some(roles)
    );
}

#[test]
fn saved_roles_none_and_explicit_empty_round_trip_and_legacy_missing_fields() {
    for roles in [None, Some(SavedRoleBindings::new(vec![]).unwrap())] {
        let (root, mut writer) = setup();
        writer
            .configure_role_bindings(DecimalU64::new(0), roles.clone(), &[])
            .unwrap();
        writer
            .apply(
                &start_with_configuration("empty-role-run", 1),
                Some(target()),
            )
            .unwrap();
        let saved = writer.snapshot().unwrap();
        let mut legacy = serde_json::to_value(&saved.state).unwrap();
        legacy.as_object_mut().unwrap().remove("role_bindings");
        for run in legacy["runs"].as_array_mut().unwrap() {
            run.as_object_mut().unwrap().remove("role_bindings");
        }
        let legacy: Sidecar = serde_json::from_value(legacy).unwrap();
        assert!(legacy.role_bindings.is_none());
        assert!(legacy.runs[0].role_bindings.is_none());
        drop(writer);
        let saved = root
            .open(&project(), &session())
            .unwrap()
            .snapshot()
            .unwrap();
        assert_eq!(saved.state.role_bindings, roles);
        assert_eq!(saved.state.runs[0].role_bindings, roles);
    }
}

#[test]
fn saved_roles_disk_rejects_invalid_current_and_historical_sidecar_and_run_metadata() {
    use crate::conversation_state::content_hash;
    // Keep hashes valid so failures exercise role validation, not checksum rejection.
    for historical in [false, true] {
        for in_run in [false, true] {
            let (root, mut writer) = setup();
            writer
                .configure_role_bindings(
                    DecimalU64::new(0),
                    Some(saved_roles_fixture()),
                    &role_catalog(),
                )
                .unwrap();
            writer
                .apply(
                    &start_with_configuration("corrupt-role-run", 1),
                    Some(target()),
                )
                .unwrap();
            writer
                .configure_role_bindings(DecimalU64::new(1), None, &[])
                .unwrap();
            let path = dir(&writer);
            let mut marker = writer.snapshot().unwrap().marker;
            drop(writer);
            let bytes = fs::read(path.join(disk::SIDECAR)).unwrap();
            let mut states: Vec<serde_json::Value> = bytes
                .split(|b| *b == b'\n')
                .filter(|line| !line.is_empty())
                .map(|line| serde_json::from_slice(line).unwrap())
                .collect();
            let index = if historical {
                states.len() - 2
            } else {
                states.len() - 1
            };
            let invalid = serde_json::json!({"schema_version": 99, "bindings": []});
            if in_run {
                states[index]["runs"][0]["role_bindings"] = invalid;
            } else {
                states[index]["role_bindings"] = invalid;
            }
            let rewritten: Vec<u8> = states
                .iter()
                .flat_map(|state| disk::line(state).unwrap())
                .collect();
            marker.sidecar_offset = DecimalU64::new(rewritten.len() as u64);
            marker.sidecar_hash = content_hash(&rewritten);
            fs::write(path.join(disk::SIDECAR), rewritten).unwrap();
            fs::write(
                path.join(disk::MARKER),
                serde_json::to_vec(&marker).unwrap(),
            )
            .unwrap();
            assert!(matches!(
                root.open(&project(), &session()),
                Err(StoreError::UnsupportedVersion(99))
            ));
        }
    }
}

fn saved_child_fixture(n: usize) -> Child {
    Child {
        run_id: RunId::new(format!("child-{n}")).unwrap(),
        attempt_id: AttemptId::new(format!("child-attempt-{n}")).unwrap(),
        parent_run_id: target().run_id,
        state: RunState::Running,
        task_ids: vec![],
    }
}
fn child_identity(child: &Child) -> crate::desktop_events::ChildIdentity {
    crate::desktop_events::ChildIdentity {
        run_id: child.run_id.clone(),
        attempt_id: child.attempt_id.clone(),
        parent_run_id: child.parent_run_id.clone(),
    }
}
#[test]
fn child_ledger_replay_is_idempotent_and_terminal_is_immutable() {
    let (_root, mut writer) = setup();
    writer.apply(&start("start", 0), Some(target())).unwrap();
    writer.record_intent(&target(), op()).unwrap();
    writer
        .record_operation_result(&target(), &op(), result())
        .unwrap();
    let child = saved_child_fixture(0);
    assert_eq!(
        writer
            .record_child_start(&target(), child.clone(), "reviewer".into(), "review".into())
            .unwrap(),
        IntentReceipt::NewlyPublished
    );
    let before = writer.snapshot().unwrap();
    assert_eq!(
        writer
            .record_child_start(&target(), child.clone(), "reviewer".into(), "review".into())
            .unwrap(),
        IntentReceipt::AlreadyRecorded
    );
    unchanged(&before, &writer.snapshot().unwrap());
    assert!(
        writer
            .record_child_start(&target(), child.clone(), "other".into(), "review".into())
            .is_err()
    );
    writer
        .record_child_finish(&target(), &child_identity(&child), Observation::Succeeded)
        .unwrap();
    let terminal = writer.snapshot().unwrap();
    assert_eq!(terminal.state.tasks, before.state.tasks);
    assert_eq!(terminal.state.runs, before.state.runs);
    assert_eq!(
        writer
            .record_child_finish(&target(), &child_identity(&child), Observation::Succeeded)
            .unwrap(),
        IntentReceipt::AlreadyRecorded
    );
    assert_eq!(
        writer
            .record_child_start(&target(), child.clone(), "reviewer".into(), "review".into())
            .unwrap(),
        IntentReceipt::AlreadyRecorded
    );
    assert!(
        writer
            .record_child_finish(&target(), &child_identity(&child), Observation::Failed)
            .is_err()
    );
    unchanged(&terminal, &writer.snapshot().unwrap());
}
#[test]
fn child_ledger_cancel_and_reopen_preserve_unknown_without_task_success() {
    for reopen in [false, true] {
        let (root, mut writer) = setup();
        writer.apply(&start("start", 0), Some(target())).unwrap();
        writer.record_intent(&target(), op()).unwrap();
        writer
            .record_operation_result(&target(), &op(), result())
            .unwrap();
        writer
            .record_child_start(
                &target(),
                saved_child_fixture(0),
                "worker".into(),
                "work".into(),
            )
            .unwrap();
        let before = writer.snapshot().unwrap();
        assert!(
            writer
                .finish(&target(), Observation::Succeeded, result())
                .is_err()
        );
        unchanged(&before, &writer.snapshot().unwrap());
        if !reopen {
            writer
                .finish(&target(), Observation::Cancelled, result())
                .unwrap();
        }
        drop(writer);
        let writer = root.open(&project(), &session()).unwrap();
        let saved = writer.snapshot().unwrap();
        assert_eq!(
            saved.state.children[0].child.state,
            RunState::OutcomeUnknown
        );
        assert_eq!(saved.state.tasks, before.state.tasks);
        assert!(saved.state.runs[0].run.state.is_terminal());
    }
}
#[test]
fn child_ledger_rejects_wrong_ownership_tasks_and_bounds() {
    let (_root, mut writer) = setup();
    writer.apply(&start("start", 0), Some(target())).unwrap();
    writer.record_intent(&target(), op()).unwrap();
    writer
        .record_operation_result(&target(), &op(), result())
        .unwrap();
    let before = writer.snapshot().unwrap();
    let mut wrong = saved_child_fixture(0);
    wrong.parent_run_id = RunId::new("unknown-parent").unwrap();
    assert!(
        writer
            .record_child_start(&target(), wrong, "worker".into(), "work".into())
            .is_err()
    );
    let mut wrong = saved_child_fixture(0);
    wrong.task_ids.push(TaskId::new("unfrozen-task").unwrap());
    assert!(
        writer
            .record_child_start(&target(), wrong, "worker".into(), "work".into())
            .is_err()
    );
    let wrong_target = RunTarget {
        attempt_id: AttemptId::new("wrong").unwrap(),
        ..target()
    };
    assert!(
        writer
            .record_child_start(
                &wrong_target,
                saved_child_fixture(0),
                "worker".into(),
                "work".into()
            )
            .is_err()
    );
    assert!(
        writer
            .record_child_start(
                &target(),
                saved_child_fixture(0),
                "worker".into(),
                "x".repeat(4097)
            )
            .is_err()
    );
    unchanged(&before, &writer.snapshot().unwrap());
    for n in 0..32 {
        writer
            .record_child_start(
                &target(),
                saved_child_fixture(n),
                "worker".into(),
                "work".into(),
            )
            .unwrap();
    }
    let full = writer.snapshot().unwrap();
    assert!(
        writer
            .record_child_start(
                &target(),
                saved_child_fixture(32),
                "worker".into(),
                "work".into()
            )
            .is_err()
    );
    unchanged(&full, &writer.snapshot().unwrap());
}
#[test]
fn child_ledger_failed_publication_requires_recovery() {
    let (root, mut writer) = setup();
    writer.apply(&start("start", 0), Some(target())).unwrap();
    writer.record_intent(&target(), op()).unwrap();
    writer
        .record_operation_result(&target(), &op(), result())
        .unwrap();
    inject(Some("directory_sync"));
    let published = writer.record_child_start(
        &target(),
        saved_child_fixture(0),
        "worker".into(),
        "work".into(),
    );
    inject(None);
    assert!(published.is_err());
    assert!(matches!(
        writer.snapshot(),
        Err(StoreError::RecoveryRequired)
    ));
    drop(writer);
    let writer = root.open(&project(), &session()).unwrap();
    assert!(
        writer
            .snapshot()
            .unwrap()
            .state
            .children
            .iter()
            .all(|c| c.child.state == RunState::OutcomeUnknown)
    );
}

#[test]
fn child_ledger_raw_finish_seals_children_and_preserves_gap() {
    let (_root, mut writer) = setup();
    writer.apply(&start("start", 0), Some(target())).unwrap();
    writer.record_intent(&target(), op()).unwrap();
    writer
        .record_child_start(
            &target(),
            saved_child_fixture(0),
            "worker".into(),
            "work".into(),
        )
        .unwrap();
    writer
        .record_history_gap(&target(), HistoryGap::OutputRejected)
        .unwrap();
    writer
        .finish_with_messages(
            &target(),
            &op(),
            Observation::Failed,
            result(),
            vec![polaris_provider::Message::assistant("partial")],
        )
        .unwrap();
    let saved = writer.snapshot().unwrap();
    assert_eq!(
        saved.state.children[0].child.state,
        RunState::OutcomeUnknown
    );
    assert_eq!(
        saved.state.runs[0].history_gap,
        Some(HistoryGap::OutputRejected)
    );
    assert_eq!(saved.raw.len(), 2);
    writer
        .finish_with_messages(
            &target(),
            &op(),
            Observation::Failed,
            result(),
            vec![polaris_provider::Message::assistant("partial")],
        )
        .unwrap();
    unchanged(&saved, &writer.snapshot().unwrap());
}

#[test]
fn child_ledger_legacy_and_session_bound_validation() {
    let (_root, mut writer) = setup();
    writer.apply(&start("start", 0), Some(target())).unwrap();
    let mut state = writer.snapshot().unwrap().state;
    let mut legacy = serde_json::to_value(&state).unwrap();
    legacy.as_object_mut().unwrap().remove("children");
    assert!(
        serde_json::from_value::<Sidecar>(legacy)
            .unwrap()
            .children
            .is_empty()
    );
    let template = state.runs[0].clone();
    state.runs.clear();
    for r in 0..5 {
        let root = RunTarget {
            run_id: RunId::new(format!("root-{r}")).unwrap(),
            attempt_id: AttemptId::new(format!("root-attempt-{r}")).unwrap(),
        };
        let mut record = template.clone();
        record.run.run_id = root.run_id.clone();
        record.run.attempt_id = root.attempt_id.clone();
        state.runs.push(record);
        for n in 0..if r == 4 { 1 } else { 32 } {
            let mut child = saved_child_fixture(r * 32 + n);
            child.parent_run_id = root.run_id.clone();
            state.children.push(SavedChild {
                root: root.clone(),
                child,
                agent_type: "worker".into(),
                task: "work".into(),
            });
        }
    }
    assert!(super::children::validate(&state).is_err());
    state.children.pop();
    super::children::validate(&state).unwrap();
}

fn source_apply_fixture() -> (SourceApplyCandidate, SourceApplyIdentityProof) {
    let identity = |inode| SourceApplyIdentity {
        device: DecimalU64::new(1),
        inode: DecimalU64::new(inode),
    };
    let payload = SourceApplyPayload {
        source_path: "/source".into(),
        source_identity: identity(10),
        recovery_parent_path: "/private/recovery/operation".into(),
        recovery_parent_identity: identity(20),
        entries: vec![SourceApplyEntry {
            relative_path: "file.txt".into(),
            before: None,
            after: Some(SourceApplyVersion {
                hash: "a".repeat(64),
                mode: 0o644,
            }),
        }],
    };
    let proof = SourceApplyIdentityProof {
        source: payload.source_identity,
        recovery_parent: payload.recovery_parent_identity,
    };
    (
        SourceApplyCandidate {
            run_id: target().run_id,
            attempt_id: target().attempt_id,
            approval_id: ApprovalId::new("source-approval").unwrap(),
            operation_id: OperationId::new("source-operation").unwrap(),
            policy_revision: DecimalU64::new(7),
            expires_at_unix_ms: DecimalU64::new(100),
            payload_hash: payload.payload_hash().unwrap(),
            payload,
        },
        proof,
    )
}
fn source_guard(writer: &Writer) -> SourceApplyGuard {
    let p = writer.snapshot().unwrap();
    SourceApplyGuard {
        expected_session_revision: p.marker.session_revision,
        expected_policy_revision: p.state.policy_revision,
        now_ms: 1,
    }
}
fn source_terminal(writer: &mut Writer) {
    writer
        .apply(&start("source-start", 0), Some(target()))
        .unwrap();
    source_finish(writer);
}
fn source_finish(writer: &mut Writer) {
    // A queued run cannot report success: publish execution intent, then its
    // result, before the terminal observation, just like the production owner.
    writer.record_intent(&target(), op()).unwrap();
    writer
        .record_operation_result(&target(), &op(), result())
        .unwrap();
    writer
        .finish(&target(), Observation::Succeeded, result())
        .unwrap();
}
fn source_allow(writer: &mut Writer) -> (SourceApplyCandidate, SourceApplyIdentityProof) {
    let (c, proof) = source_apply_fixture();
    writer
        .publish_source_apply(&target(), c.clone(), source_guard(writer), proof)
        .unwrap();
    writer
        .resolve_source_apply(
            &target(),
            &c.approval_id,
            ApprovalDecision::Allow,
            source_guard(writer),
        )
        .unwrap();
    (c, proof)
}
fn source_report() -> SourceApplyResult {
    SourceApplyResult {
        result_id: ResultId::new("source-result").unwrap(),
        report: serde_json::json!({"entries": [{"relative_path":"file.txt", "installed":true}], "failure":null}),
    }
}

#[test]
fn source_apply_terminal_cas_expiry_allow_and_identity_gates() {
    let (_, mut w) = setup();
    w.apply(&start("source-start", 0), Some(target())).unwrap();
    let (c, proof) = source_apply_fixture();
    assert!(
        w.publish_source_apply(&target(), c.clone(), source_guard(&w), proof)
            .is_err()
    );
    source_finish(&mut w);
    let before = w.snapshot().unwrap();
    let mut stale = source_guard(&w);
    stale.expected_session_revision = DecimalU64::new(0);
    assert!(
        w.publish_source_apply(&target(), c.clone(), stale, proof)
            .is_err()
    );
    let mut wrong = proof;
    wrong.recovery_parent.inode = DecimalU64::new(99);
    assert!(
        w.publish_source_apply(&target(), c.clone(), source_guard(&w), wrong)
            .is_err()
    );
    unchanged(&before, &w.snapshot().unwrap());
    w.publish_source_apply(&target(), c.clone(), source_guard(&w), proof)
        .unwrap();
    assert!(
        w.consume_source_apply_intent(
            &target(),
            &c.approval_id,
            &c.payload_hash,
            source_guard(&w),
            proof
        )
        .is_err()
    );
    w.resolve_source_apply(
        &target(),
        &c.approval_id,
        ApprovalDecision::Allow,
        source_guard(&w),
    )
    .unwrap();
    let before = w.snapshot().unwrap();
    let mut expired = source_guard(&w);
    expired.now_ms = 100;
    assert!(
        w.consume_source_apply_intent(&target(), &c.approval_id, &c.payload_hash, expired, proof)
            .is_err()
    );
    assert!(
        w.consume_source_apply_intent(
            &target(),
            &c.approval_id,
            &c.payload_hash,
            source_guard(&w),
            wrong
        )
        .is_err()
    );
    assert!(
        w.consume_source_apply_intent(
            &target(),
            &c.approval_id,
            &"b".repeat(64),
            source_guard(&w),
            proof
        )
        .is_err()
    );
    unchanged(&before, &w.snapshot().unwrap());
    assert_eq!(
        w.consume_source_apply_intent(
            &target(),
            &c.approval_id,
            &c.payload_hash,
            source_guard(&w),
            proof
        )
        .unwrap(),
        IntentReceipt::NewlyPublished
    );
}

#[test]
fn source_apply_reopen_policy_result_and_replay_preserve_run() {
    let (root, mut w) = setup();
    source_terminal(&mut w);
    let before = w.snapshot().unwrap();
    let (c, proof) = source_allow(&mut w);
    assert_eq!(
        w.consume_source_apply_intent(
            &target(),
            &c.approval_id,
            &c.payload_hash,
            source_guard(&w),
            proof
        )
        .unwrap(),
        IntentReceipt::NewlyPublished
    );
    w.set_policy_revision(DecimalU64::new(8)).unwrap();
    w.invalidate_source_apply(
        &target(),
        &c.approval_id,
        source_guard(&w).expected_session_revision,
    )
    .unwrap();
    drop(w);
    let mut w = root.open(&project(), &session()).unwrap();
    let saved = w.snapshot().unwrap();
    assert!(saved.state.source_applies[0].intent_revision.is_some());
    assert!(saved.state.source_applies[0].result.is_none());
    assert_eq!(
        w.consume_source_apply_intent(
            &target(),
            &c.approval_id,
            &c.payload_hash,
            source_guard(&w),
            proof
        )
        .unwrap(),
        IntentReceipt::AlreadyRecorded
    );
    w.record_source_apply_result(
        &target(),
        &c.operation_id,
        source_guard(&w).expected_session_revision,
        source_report(),
    )
    .unwrap();
    let after = w.snapshot().unwrap();
    w.record_source_apply_result(
        &target(),
        &c.operation_id,
        source_guard(&w).expected_session_revision,
        source_report(),
    )
    .unwrap();
    unchanged(&after, &w.snapshot().unwrap());
    let mut different = source_report();
    different.report["failure"] = serde_json::json!("partial");
    assert!(
        w.record_source_apply_result(
            &target(),
            &c.operation_id,
            source_guard(&w).expected_session_revision,
            different
        )
        .is_err()
    );
    assert_eq!(before.state.runs, after.state.runs);
    assert_eq!(before.state.configuration, after.state.configuration);
    assert_eq!(before.marker.raw_hash, after.marker.raw_hash);
    assert_eq!(
        before.marker.content_revision,
        after.marker.content_revision
    );
    drop(w);
    let w = root.open(&project(), &session()).unwrap();
    assert_eq!(
        w.snapshot().unwrap().state.source_applies[0].result,
        Some(source_report())
    );
}

#[test]
fn source_apply_unconsumed_reopen_policy_cancel_and_deny_never_grant_intent() {
    for reason in ["reopen", "policy", "cancel", "deny"] {
        let (root, mut w) = setup();
        source_terminal(&mut w);
        let (c, proof) = source_apply_fixture();
        w.publish_source_apply(&target(), c.clone(), source_guard(&w), proof)
            .unwrap();
        w.resolve_source_apply(
            &target(),
            &c.approval_id,
            if reason == "deny" {
                ApprovalDecision::Deny
            } else {
                ApprovalDecision::Allow
            },
            source_guard(&w),
        )
        .unwrap();
        match reason {
            "reopen" => {
                drop(w);
                w = root.open(&project(), &session()).unwrap();
            }
            "policy" => w.set_policy_revision(DecimalU64::new(8)).unwrap(),
            "cancel" => w
                .invalidate_source_apply(
                    &target(),
                    &c.approval_id,
                    source_guard(&w).expected_session_revision,
                )
                .unwrap(),
            _ => {}
        }
        assert!(
            w.consume_source_apply_intent(
                &target(),
                &c.approval_id,
                &c.payload_hash,
                source_guard(&w),
                proof
            )
            .is_err()
        );
        let r = &w.snapshot().unwrap().state.source_applies[0];
        assert!(r.intent_revision.is_none());
        assert_eq!(r.candidate, c);
    }
}

#[test]
fn source_apply_publication_failures_reconcile_without_new_permission() {
    for stage in ["pending", "answer", "intent", "result"] {
        for point in ["marker_rename", "directory_sync"] {
            let (root, mut w) = setup();
            source_terminal(&mut w);
            let (c, proof) = source_apply_fixture();
            if stage != "pending" {
                w.publish_source_apply(&target(), c.clone(), source_guard(&w), proof)
                    .unwrap();
            }
            if matches!(stage, "intent" | "result") {
                w.resolve_source_apply(
                    &target(),
                    &c.approval_id,
                    ApprovalDecision::Allow,
                    source_guard(&w),
                )
                .unwrap();
            }
            if stage == "result" {
                w.consume_source_apply_intent(
                    &target(),
                    &c.approval_id,
                    &c.payload_hash,
                    source_guard(&w),
                    proof,
                )
                .unwrap();
            }
            let guard = source_guard(&w);
            inject(Some(point));
            let failed = match stage {
                "pending" => w
                    .publish_source_apply(&target(), c.clone(), guard, proof)
                    .is_err(),
                "answer" => w
                    .resolve_source_apply(&target(), &c.approval_id, ApprovalDecision::Allow, guard)
                    .is_err(),
                "intent" => w
                    .consume_source_apply_intent(
                        &target(),
                        &c.approval_id,
                        &c.payload_hash,
                        guard,
                        proof,
                    )
                    .is_err(),
                _ => w
                    .record_source_apply_result(
                        &target(),
                        &c.operation_id,
                        guard.expected_session_revision,
                        source_report(),
                    )
                    .is_err(),
            };
            inject(None);
            assert!(failed);
            assert!(matches!(w.snapshot(), Err(StoreError::RecoveryRequired)));
            w.recover().unwrap();
            drop(w);
            let mut w = root.open(&project(), &session()).unwrap();
            let replay = w.consume_source_apply_intent(
                &target(),
                &c.approval_id,
                &c.payload_hash,
                source_guard(&w),
                proof,
            );
            assert!(!matches!(replay, Ok(IntentReceipt::NewlyPublished)));
            if stage == "result" || (stage == "intent" && point == "directory_sync") {
                assert!(
                    w.snapshot().unwrap().state.source_applies[0]
                        .intent_revision
                        .is_some()
                );
            }
        }
    }
}

#[test]
fn source_apply_bounds_and_legacy_metadata() {
    let (c, _) = source_apply_fixture();
    for kind in ["path", "entries", "bytes", "duplicate", "hash"] {
        let mut payload = c.payload.clone();
        match kind {
            "path" => payload.entries[0].relative_path = "../escape".into(),
            "entries" => {
                payload.entries = vec![payload.entries[0].clone(); MAX_SOURCE_APPLY_ENTRIES + 1]
            }
            "bytes" => {
                let entry = payload.entries[0].clone();
                payload.entries = (0..400)
                    .map(|i| SourceApplyEntry {
                        relative_path: format!("{i:04}{}", "x".repeat(3000)),
                        ..entry.clone()
                    })
                    .collect();
            }
            "duplicate" => payload.entries.push(payload.entries[0].clone()),
            _ => payload.entries[0].after.as_mut().unwrap().hash = "z".repeat(64),
        }
        assert!(payload.validate().is_err(), "{kind}");
    }
    let mut report = source_report();
    report.report["entries"] =
        serde_json::json!(vec![serde_json::json!({}); MAX_SOURCE_APPLY_ENTRIES + 1]);
    assert!(report.validate().is_err());
    report = source_report();
    report.report["extra"] = serde_json::json!("x".repeat(MAX_SOURCE_APPLY_BYTES));
    assert!(report.validate().is_err());
    let (_, w) = setup();
    let mut old = serde_json::to_value(w.snapshot().unwrap().state).unwrap();
    old.as_object_mut().unwrap().remove("source_applies");
    assert!(
        serde_json::from_value::<Sidecar>(old)
            .unwrap()
            .source_applies
            .is_empty()
    );
    let mut unknown = serde_json::to_value(c).unwrap();
    unknown["unknown"] = serde_json::json!(true);
    assert!(serde_json::from_value::<SourceApplyCandidate>(unknown).is_err());
}

#[test]
fn source_apply_full_history_rejects_corruption_and_evidence_removal() {
    use crate::conversation_state::content_hash;
    for kind in ["historical", "latest", "removed", "rewritten"] {
        let (root, mut w) = setup();
        source_terminal(&mut w);
        let (c, proof) = source_allow(&mut w);
        w.consume_source_apply_intent(
            &target(),
            &c.approval_id,
            &c.payload_hash,
            source_guard(&w),
            proof,
        )
        .unwrap();
        let path = dir(&w);
        let mut marker = w.snapshot().unwrap().marker;
        drop(w);
        let bytes = fs::read(path.join(disk::SIDECAR)).unwrap();
        let mut states: Vec<serde_json::Value> = bytes
            .split(|b| *b == b'\n')
            .filter(|b| !b.is_empty())
            .map(|b| serde_json::from_slice(b).unwrap())
            .collect();
        let last = states.len() - 1;
        match kind {
            "historical" => {
                states[last - 1]["source_applies"][0]["candidate"]["payload_hash"] =
                    serde_json::json!("bad")
            }
            "latest" => {
                states[last]["source_applies"][0]["intent_revision"] = serde_json::json!("999999")
            }
            "removed" => states[last]["source_applies"] = serde_json::json!([]),
            _ => {
                states[last]["source_applies"][0]["candidate"]["expires_at_unix_ms"] =
                    serde_json::json!("200")
            }
        }
        let rewritten: Vec<u8> = states.iter().flat_map(|s| disk::line(s).unwrap()).collect();
        marker.sidecar_offset = DecimalU64::new(rewritten.len() as u64);
        marker.sidecar_hash = content_hash(&rewritten);
        fs::write(path.join(disk::SIDECAR), rewritten).unwrap();
        fs::write(
            path.join(disk::MARKER),
            serde_json::to_vec(&marker).unwrap(),
        )
        .unwrap();
        assert!(root.open(&project(), &session()).is_err(), "{kind}");
    }
}

#[test]
fn source_apply_duplicate_bindings_and_unreported_intent_block_tombstone() {
    let (_, mut w) = setup();
    source_terminal(&mut w);
    let (c, proof) = source_allow(&mut w);
    let before = w.snapshot().unwrap();
    w.publish_source_apply(&target(), c.clone(), source_guard(&w), proof)
        .unwrap();
    w.resolve_source_apply(
        &target(),
        &c.approval_id,
        ApprovalDecision::Allow,
        source_guard(&w),
    )
    .unwrap();
    unchanged(&before, &w.snapshot().unwrap());
    let mut different = c.clone();
    different.expires_at_unix_ms = DecimalU64::new(200);
    assert!(
        w.publish_source_apply(&target(), different, source_guard(&w), proof)
            .is_err()
    );
    assert!(
        w.resolve_source_apply(
            &target(),
            &c.approval_id,
            ApprovalDecision::Deny,
            source_guard(&w)
        )
        .is_err()
    );
    assert!(
        w.record_source_apply_result(
            &target(),
            &c.operation_id,
            source_guard(&w).expected_session_revision,
            source_report()
        )
        .is_err()
    );
    w.consume_source_apply_intent(
        &target(),
        &c.approval_id,
        &c.payload_hash,
        source_guard(&w),
        proof,
    )
    .unwrap();
    assert!(matches!(w.tombstone(), Err(StoreError::Busy)));
}

#[test]
fn source_apply_aggregate_bytes_are_bounded_independently_of_record_count() {
    let (_, mut w) = setup();
    source_terminal(&mut w);
    let (candidate, _) = source_apply_fixture();
    let mut state = w.snapshot().unwrap().state;
    for i in 0..13 {
        let mut c = candidate.clone();
        c.operation_id = OperationId::new(format!("aggregate-op-{i}")).unwrap();
        c.approval_id = ApprovalId::new(format!("aggregate-approval-{i}")).unwrap();
        state.source_applies.push(SavedSourceApply {
            candidate: c,
            decision: Some(ApprovalDecision::Allow),
            invalidated: false,
            intent_revision: Some(state.session_revision),
            result: Some(SourceApplyResult {
                result_id: ResultId::new(format!("aggregate-result-{i}")).unwrap(),
                report: serde_json::json!({"entries": [], "evidence": "x".repeat(700_000)}),
            }),
        });
        state
            .source_applies
            .last()
            .unwrap()
            .result
            .as_ref()
            .unwrap()
            .validate()
            .unwrap();
        if i == 0 {
            super::source_apply::validate(&state).unwrap();
        }
    }
    assert!(state.source_applies.len() < MAX_SOURCE_APPLY_RECORDS);
    assert!(
        serde_json::to_vec(&state.source_applies).unwrap().len() > MAX_SOURCE_APPLY_LEDGER_BYTES
    );
    assert!(super::source_apply::validate(&state).is_err());
}

#[test]
fn source_apply_reservation_protects_maximum_reports_near_capacity() {
    let (_, mut w) = setup();
    source_terminal(&mut w);
    let (base, proof) = source_apply_fixture();
    let mut candidates = Vec::new();
    for i in 0..8 {
        let mut c = base.clone();
        c.operation_id = OperationId::new(format!("reserved-op-{i}")).unwrap();
        c.approval_id = ApprovalId::new(format!("reserved-approval-{i}")).unwrap();
        w.publish_source_apply(&target(), c.clone(), source_guard(&w), proof)
            .unwrap();
        w.resolve_source_apply(
            &target(),
            &c.approval_id,
            ApprovalDecision::Allow,
            source_guard(&w),
        )
        .unwrap();
        let before = w.snapshot().unwrap();
        let receipt = w.consume_source_apply_intent(
            &target(),
            &c.approval_id,
            &c.payload_hash,
            source_guard(&w),
            proof,
        );
        if i == 7 {
            // Eight full reservations plus metadata cannot fit in eight MiB.
            assert!(receipt.is_err());
            unchanged(&before, &w.snapshot().unwrap());
        } else {
            assert_eq!(receipt.unwrap(), IntentReceipt::NewlyPublished);
            candidates.push(c);
        }
    }

    // This valid payload fits the physical free space but would steal capacity
    // reserved for the seven reports. No unbounded production allocation is used.
    let mut large = base.clone();
    large.operation_id = OperationId::new("reservation-thief").unwrap();
    large.approval_id = ApprovalId::new("reservation-thief").unwrap();
    large.payload.entries = (0..300)
        .map(|i| SourceApplyEntry {
            relative_path: format!("{i:04}"),
            ..base.payload.entries[0].clone()
        })
        .collect();
    let mut remaining =
        MAX_SOURCE_APPLY_BYTES - 1 - serde_json::to_vec(&large.payload).unwrap().len();
    for entry in &mut large.payload.entries {
        let add = remaining.min(4096 - entry.relative_path.len());
        entry.relative_path.push_str(&"x".repeat(add));
        remaining -= add;
    }
    assert_eq!(remaining, 0);
    large.payload_hash = large.payload.payload_hash().unwrap();
    let before = w.snapshot().unwrap();
    assert!(
        w.publish_source_apply(&target(), large, source_guard(&w), proof)
            .is_err()
    );
    unchanged(&before, &w.snapshot().unwrap());

    for (i, c) in candidates.iter().enumerate() {
        let mut report = SourceApplyResult {
            result_id: ResultId::new(format!("reserved-result-{i}")).unwrap(),
            report: serde_json::json!({"entries": [], "evidence": ""}),
        };
        let overhead = serde_json::to_vec(&report).unwrap().len();
        report.report["evidence"] =
            serde_json::json!("x".repeat(MAX_SOURCE_APPLY_BYTES - overhead));
        assert_eq!(
            serde_json::to_vec(&report).unwrap().len(),
            MAX_SOURCE_APPLY_BYTES
        );
        report.validate().unwrap();
        w.record_source_apply_result(
            &target(),
            &c.operation_id,
            source_guard(&w).expected_session_revision,
            report,
        )
        .unwrap();
    }
    let saved = w.snapshot().unwrap();
    assert!(
        saved
            .state
            .source_applies
            .iter()
            .filter(|r| r.intent_revision.is_some())
            .all(|r| r.result.is_some())
    );
    assert!(
        serde_json::to_vec(&saved.state.source_applies)
            .unwrap()
            .len()
            < MAX_SOURCE_APPLY_LEDGER_BYTES
    );
    w.tombstone().unwrap();
}

fn source_answer_request(w: &Writer, c: &SourceApplyCandidate, id: &str) -> Request {
    let guard = source_guard(w);
    request(
        id,
        RequestBody::SourceApplyResolve(
            session(),
            SourceApplyResolve {
                run_id: c.run_id.clone(),
                attempt_id: c.attempt_id.clone(),
                approval_id: c.approval_id.clone(),
                payload_hash: c.payload_hash.clone(),
                expected_session_revision: guard.expected_session_revision,
                expected_policy_revision: guard.expected_policy_revision,
                decision: ApprovalDecision::Allow,
            },
        ),
    )
}

#[test]
fn source_answer_request_is_atomic_and_replay_is_receipt_only() {
    let (root, mut w) = setup();
    source_terminal(&mut w);
    let (c, proof) = source_apply_fixture();
    w.publish_source_apply(&target(), c.clone(), source_guard(&w), proof)
        .unwrap();
    let request = source_answer_request(&w, &c, "source-wire-answer");
    let before = w.snapshot().unwrap();
    let accepted = w.resolve_source_apply_request(&request, 1).unwrap();
    let saved = w.snapshot().unwrap();
    assert_eq!(
        saved.marker.session_revision,
        before.marker.session_revision.checked_add(1).unwrap()
    );
    assert_eq!(
        saved.state.source_applies[0].decision,
        Some(ApprovalDecision::Allow)
    );
    assert!(saved.state.source_applies[0].intent_revision.is_none());
    assert!(matches!(
        accepted.record.result,
        RequestResult::SourceApplyResolved { .. }
    ));
    assert_eq!(
        serde_json::to_value(&saved.raw).unwrap(),
        serde_json::to_value(&before.raw).unwrap()
    );
    assert_eq!(saved.state.runs, before.state.runs);
    let replay = w.resolve_source_apply_request(&request, u64::MAX).unwrap();
    assert_eq!(replay.record, accepted.record);
    unchanged(&saved, &w.snapshot().unwrap());
    let other = source_answer_request(&w, &c, "different-source-answer");
    assert!(w.resolve_source_apply_request(&other, 1).is_err());
    drop(w);
    let mut w = root.open(&project(), &session()).unwrap();
    assert_eq!(
        w.resolve_source_apply_request(&request, u64::MAX)
            .unwrap()
            .record,
        accepted.record
    );
    assert!(!matches!(
        w.consume_source_apply_intent(
            &target(),
            &c.approval_id,
            &c.payload_hash,
            source_guard(&w),
            proof
        ),
        Ok(IntentReceipt::NewlyPublished)
    ));
}

#[test]
fn source_answer_request_rejects_stale_changed_payload_and_wrong_scope() {
    for changed in ["session", "policy", "hash", "target", "scope"] {
        let (_, mut w) = setup();
        source_terminal(&mut w);
        let (c, proof) = source_apply_fixture();
        w.publish_source_apply(&target(), c.clone(), source_guard(&w), proof)
            .unwrap();
        let mut request = source_answer_request(&w, &c, "source-wire-bad");
        let RequestBody::SourceApplyResolve(session, p) = &mut request.body else {
            unreachable!()
        };
        match changed {
            "session" => p.expected_session_revision = DecimalU64::new(0),
            "policy" => p.expected_policy_revision = DecimalU64::new(0),
            "hash" => p.payload_hash = "f".repeat(64),
            "target" => p.attempt_id = AttemptId::new("different-attempt").unwrap(),
            _ => *session = SessionId::new("other-session").unwrap(),
        }
        let before = w.snapshot().unwrap();
        assert!(
            w.resolve_source_apply_request(&request, 1).is_err(),
            "{changed}"
        );
        unchanged(&before, &w.snapshot().unwrap());
    }
}

#[test]
fn source_answer_publication_fault_never_saves_answer_without_receipt() {
    for point in ["marker_rename", "directory_sync"] {
        let (_, mut w) = setup();
        source_terminal(&mut w);
        let (c, proof) = source_apply_fixture();
        w.publish_source_apply(&target(), c.clone(), source_guard(&w), proof)
            .unwrap();
        let request = source_answer_request(&w, &c, "source-wire-fault");
        inject(Some(point));
        let outcome = w.resolve_source_apply_request(&request, 1);
        inject(None);
        assert!(outcome.is_err());
        assert!(matches!(w.snapshot(), Err(StoreError::RecoveryRequired)));
        w.recover().unwrap();
        let saved = w.snapshot().unwrap();
        let receipt = w
            .request_status(&session(), &request.client_id, &request.request_id)
            .unwrap();
        assert_eq!(
            saved.state.source_applies[0].decision.is_some(),
            receipt.is_some()
        );
        assert!(saved.state.source_applies[0].intent_revision.is_none());
    }
}
