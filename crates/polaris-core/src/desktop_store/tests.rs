//! S01〜S08の公開境界、要求台帳、復旧と実OS lockの回帰試験。

use super::disk;
use super::*;
use polaris_desktop_protocol::{ids::*, request::*, run_state::*, snapshot::*};
use std::{fs, io::Write};

mod admission_tests;
mod details_tests;
mod mcp_tests;
mod prompt_tests;

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

// Independently authored wire fixtures. Never regenerate these from a writer.
const M2A_SIDECAR_V1: &str = r#"{"session_revision":"0","draft":{"draft_revision":"0","text":"原文\n入力","attachment_ids":["attachment"]},"configuration":{"configuration_revision":"0","provider":"fake","model":"model","effort":"medium"},"policy_revision":"7","tasks":[],"runs":[],"requests":[],"unresolved_approvals":[]}"#;
const M2A_SIDECAR_V2: &str = r#"{"desktop_sidecar_schema_version":2,"inherited_turn_count":"0","conversation_memory":null,"source_applies":[],"children":[],"session_revision":"0","draft":{"draft_revision":"0","text":"原文\n入力","attachment_ids":["attachment"]},"configuration":{"configuration_revision":"0","provider":"fake","model":"model","effort":"medium"},"role_bindings":null,"tier_bindings":null,"goal":null,"instructions":[],"policy_revision":"7","tasks":[],"runs":[],"requests":[],"unresolved_approvals":[],"approval_records":[],"workflow":null,"workflow_artifacts":{"schema_version":1,"revision":0,"artifacts":[]}}"#;

#[test]
fn m2a_source_apply_sidecar_independent_v1_v2_bytes_and_field_order() {
    let (legacy, version) = super::model::decode_sidecar_wire(M2A_SIDECAR_V1.as_bytes()).unwrap();
    assert_eq!(version, 1);
    assert!(legacy.source_applies.is_empty());
    assert_eq!(legacy.inherited_turn_count.get(), 0);
    let (modern, version) = super::model::decode_sidecar_wire(M2A_SIDECAR_V2.as_bytes()).unwrap();
    assert_eq!(version, 2);
    assert_eq!(legacy, modern);
    assert_eq!(
        serde_json::to_vec(&legacy).unwrap(),
        M2A_SIDECAR_V2.as_bytes()
    );
    let (_root, writer) = setup();
    assert_eq!(
        fs::read(dir(&writer).join(disk::SIDECAR)).unwrap(),
        format!("{M2A_SIDECAR_V2}\n").as_bytes()
    );
}

#[test]
fn m2a_source_apply_sidecar_rejects_all_missing_unknown_duplicate_and_version_fields() {
    let full: serde_json::Value = serde_json::from_str(M2A_SIDECAR_V2).unwrap();
    assert_eq!(full.as_object().unwrap().len(), 20);
    for key in full.as_object().unwrap().keys() {
        if key == "desktop_sidecar_schema_version" {
            continue;
        } // Absence dispatches v1.
        let mut missing = full.clone();
        missing.as_object_mut().unwrap().remove(key);
        assert!(
            super::model::decode_sidecar_wire(&serde_json::to_vec(&missing).unwrap()).is_err(),
            "missing {key}"
        );
        let duplicate = format!(
            "{{{}:{},{}",
            serde_json::to_string(key).unwrap(),
            full[key],
            &M2A_SIDECAR_V2[1..]
        );
        assert!(
            super::model::decode_sidecar_wire(duplicate.as_bytes()).is_err(),
            "duplicate {key}"
        );
    }
    for value in ["0", "1", "3", "2.0", "null", "true", "\"2\""] {
        let raw = M2A_SIDECAR_V2.replacen(
            "\"desktop_sidecar_schema_version\":2",
            &format!("\"desktop_sidecar_schema_version\":{value}"),
            1,
        );
        assert!(
            super::model::decode_sidecar_wire(raw.as_bytes()).is_err(),
            "version {value}"
        );
    }
    for raw in [
        format!(
            "{{\"desktop_sidecar_schema_version\":2,{}",
            &M2A_SIDECAR_V2[1..]
        ),
        format!("{{\"foreign\":null,{}", &M2A_SIDECAR_V2[1..]),
        format!("{{\"session_revision\":\"0\",{}", &M2A_SIDECAR_V1[1..]),
        "[]".to_string(),
        "null".to_string(),
    ] {
        assert!(
            super::model::decode_sidecar_wire(raw.as_bytes()).is_err(),
            "{raw}"
        );
    }
}

#[test]
fn m2a_source_apply_sidecar_record_versions_require_exact_nullable_and_reserved_fields() {
    let (candidate, _) = source_apply_fixture();
    let record = SavedSourceApply {
        candidate,
        decision: None,
        invalidated: false,
        intent_revision: None,
        reserved_result_id: None,
        result: None,
    };
    let record_bytes = serde_json::to_string(&record).unwrap();
    let raw = M2A_SIDECAR_V2.replace(
        "\"source_applies\":[]",
        &format!("\"source_applies\":[{record_bytes}]"),
    );
    let (state, version) = super::model::decode_sidecar_wire(raw.as_bytes()).unwrap();
    assert_eq!(version, 2);
    assert_eq!(state.source_applies, [record.clone()]);
    let parsed: serde_json::Value = serde_json::from_str(&record_bytes).unwrap();
    for field in parsed.as_object().unwrap().keys() {
        let mut missing = parsed.clone();
        missing.as_object_mut().unwrap().remove(field);
        let changed = raw.replace(&record_bytes, &missing.to_string());
        assert!(
            super::model::decode_sidecar_wire(changed.as_bytes()).is_err(),
            "record missing {field}"
        );
        let duplicate = format!(
            "{{{}:{},{}",
            serde_json::to_string(field).unwrap(),
            parsed[field],
            &record_bytes[1..]
        );
        assert!(
            super::model::decode_sidecar_wire(raw.replace(&record_bytes, &duplicate).as_bytes())
                .is_err(),
            "record duplicate {field}"
        );
    }
    let unversioned = raw.replacen("\"desktop_sidecar_schema_version\":2,", "", 1);
    assert!(
        super::model::decode_sidecar_wire(unversioned.as_bytes()).is_err(),
        "reserved field on v1"
    );
    let legacy = unversioned.replace("\"reserved_result_id\":null,", "");
    let (legacy, version) = super::model::decode_sidecar_wire(legacy.as_bytes()).unwrap();
    assert_eq!(version, 1);
    assert_eq!(legacy.source_applies, [record]);
    let foreign = format!("{{\"foreign\":null,{}", &record_bytes[1..]);
    assert!(
        super::model::decode_sidecar_wire(raw.replace(&record_bytes, &foreign).as_bytes()).is_err()
    );
}

#[test]
fn m2a_source_apply_sidecar_prefix_migrates_without_rewrite_and_rejects_v2_to_v1() {
    use crate::conversation_state::content_hash;
    let (root, writer) = setup();
    let directory = dir(&writer);
    let mut marker = writer.snapshot().unwrap().marker;
    let legacy = format!("{M2A_SIDECAR_V1}\n").into_bytes();
    marker.sidecar_offset = DecimalU64::new(legacy.len() as u64);
    marker.sidecar_hash = content_hash(&legacy);
    fs::write(directory.join(disk::SIDECAR), &legacy).unwrap();
    fs::write(
        directory.join(disk::MARKER),
        serde_json::to_vec(&marker).unwrap(),
    )
    .unwrap();
    drop(writer);
    let mut writer = root.open(&project(), &session()).unwrap();
    writer.apply(&draft("migration", 0, "new"), None).unwrap();
    let migrated = fs::read(directory.join(disk::SIDECAR)).unwrap();
    assert!(migrated.starts_with(&legacy));
    let (_, version) = super::model::decode_sidecar_wire(&migrated[legacy.len()..]).unwrap();
    assert_eq!(version, 2);
    let mut marker = writer.snapshot().unwrap().marker;
    drop(writer);
    root.open(&project(), &session()).unwrap();
    let mut last: serde_json::Value = serde_json::from_slice(&migrated[legacy.len()..]).unwrap();
    last.as_object_mut()
        .unwrap()
        .remove("desktop_sidecar_schema_version");
    let mut rollback = format!("{M2A_SIDECAR_V2}\n").into_bytes();
    rollback.extend(disk::line(&last).unwrap());
    marker.sidecar_offset = DecimalU64::new(rollback.len() as u64);
    marker.sidecar_hash = content_hash(&rollback);
    let marker_bytes = serde_json::to_vec(&marker).unwrap();
    fs::write(directory.join(disk::SIDECAR), &rollback).unwrap();
    fs::write(directory.join(disk::MARKER), &marker_bytes).unwrap();
    assert!(matches!(
        root.open(&project(), &session()),
        Err(StoreError::Corrupt("desktop sidecar version rollback"))
    ));
    assert_eq!(fs::read(directory.join(disk::SIDECAR)).unwrap(), rollback);
    assert_eq!(
        fs::read(directory.join(disk::MARKER)).unwrap(),
        marker_bytes
    );
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
                mcp_prompt: None,
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
pub(crate) fn inject(point: Option<&'static str>) {
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
    let mut saved = SavedWorkflow::capture(&crate::workflow::SessionWorkflow::new(
        crate::workflow::WorkflowConfig {
            enabled: true,
            always: vec!["builtin:workflow-core".into()],
            ..Default::default()
        },
    ));
    // Storage/replay fixtures exercise the pre-M27 checkpoint protocol. Gate
    // enforcement has dedicated tests with an approved contract and basis.
    saved.gates.phase_gate_schema_version = None;
    saved
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
    let mut finished = initial_workflow.clone();
    finished.state.phase = crate::workflow::Phase::Review;
    finished.state.step = Some(crate::workflow::WorkflowStepV1::Review);
    finished.state.revision = 1;
    writer
        .checkpoint_workflow(
            &target(),
            desktop_tool_turn(),
            initial_workflow,
            finished.clone(),
            Some("call-1"),
            None,
        )
        .unwrap();
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
fn workflow_checkpoint_publishes_suffix_once_and_terminal_reuses_its_cursor() {
    let (_, mut writer) = setup();
    let previous = workflow_fixture();
    writer
        .configure_workflow(DecimalU64::new(0), Some(previous.clone()))
        .unwrap();
    writer
        .apply(&start("workflow-checkpoint", 0), Some(target()))
        .unwrap();
    let mut candidate = previous.clone();
    candidate.state.phase = crate::workflow::Phase::Review;
    candidate.state.revision = 1;
    let suffix = desktop_tool_turn();
    writer
        .checkpoint_workflow(
            &target(),
            suffix.clone(),
            previous.clone(),
            candidate.clone(),
            Some("call-1"),
            None,
        )
        .unwrap();
    let checkpointed = writer.snapshot().unwrap();
    assert_eq!(checkpointed.raw.len(), 4);
    assert_eq!(checkpointed.state.runs[0].workflow, Some(candidate.clone()));
    assert_eq!(checkpointed.state.workflow, Some(candidate.clone()));
    assert!(
        writer
            .checkpoint_workflow(
                &target(),
                desktop_tool_turn(),
                candidate.clone(),
                candidate.clone(),
                Some("different-call"),
                None,
            )
            .is_err()
    );
    unchanged(&checkpointed, &writer.snapshot().unwrap());
    // An uncertain acknowledgement is checked against the same candidate and
    // full suffix; it cannot append the tool result a second time.
    writer
        .checkpoint_workflow(
            &target(),
            suffix.clone(),
            previous,
            candidate.clone(),
            Some("call-1"),
            None,
        )
        .unwrap();
    unchanged(&checkpointed, &writer.snapshot().unwrap());
    writer.record_intent(&target(), op()).unwrap();
    writer
        .finish_with_messages_and_workflow(
            &target(),
            &op(),
            Observation::Succeeded,
            result(),
            suffix,
            Some(candidate),
        )
        .unwrap();
    let terminal = writer.snapshot().unwrap();
    assert_eq!(
        terminal.raw.len(),
        4,
        "terminal must reuse checkpoint cursor"
    );
    assert_eq!(terminal.state.runs[0].run.state, RunState::Succeeded);
}

#[test]
fn m27_contract_publication_and_writer_gate_use_the_exact_graph_basis() {
    let (root, mut writer) = setup();
    let workflow = SavedWorkflow::capture(&crate::workflow::SessionWorkflow::new(
        crate::workflow::WorkflowConfig {
            enabled: true,
            ..Default::default()
        },
    ));
    writer
        .configure_workflow(DecimalU64::new(0), Some(workflow))
        .unwrap();

    let draft = crate::workflow::ChangeContractV1::draft(
        "desktop-m27-contract",
        crate::workflow::RoutineEligibilityV1::local_component("desktop_store"),
        "phase changes could bypass their durable contract",
        "the writer did not compare the gate basis",
        "the owner re-evaluates the exact graph before publication",
        vec!["missing and stale gate bases are rejected".into()],
    )
    .unwrap();
    let revision = writer.snapshot().unwrap().marker.session_revision;
    let draft_ref = writer
        .publish_change_contract(revision, draft.clone())
        .unwrap();
    assert_eq!(writer.load_change_contract(&draft_ref).unwrap(), draft);
    let revision = writer.snapshot().unwrap().marker.session_revision;
    let approved_ref = writer
        .approve_change_contract_trusted(revision, &draft_ref)
        .unwrap();
    let approved = writer.load_change_contract(&approved_ref).unwrap();
    assert_eq!(
        approved.header.status,
        crate::workflow::ArtifactStatusV1::Approved
    );
    drop(writer);
    let mut writer = root.open(&project(), &session()).unwrap();
    let published = writer.snapshot().unwrap();
    assert_eq!(
        published
            .state
            .workflow
            .as_ref()
            .unwrap()
            .state
            .active_change_contract
            .as_ref(),
        Some(&approved_ref)
    );
    writer
        .apply(&start("m27-writer-gate", 0), Some(target()))
        .unwrap();

    let previous = writer.snapshot().unwrap().state.runs[0]
        .workflow
        .clone()
        .unwrap();
    let graph = writer.snapshot().unwrap().state.workflow_artifacts;
    let specify_proof = crate::workflow::evaluate_phase_transition(
        &previous.state,
        crate::workflow::Phase::Specify,
        &previous.gates,
        &graph,
    )
    .unwrap();
    let mut specify = previous.clone();
    specify.state.phase = crate::workflow::Phase::Specify;
    specify.state.step = Some(crate::workflow::WorkflowStepV1::Specify);
    specify.state.revision += 1;
    assert!(
        writer
            .checkpoint_workflow(
                &target(),
                vec![],
                previous.clone(),
                specify.clone(),
                None,
                None,
            )
            .is_err()
    );
    writer
        .checkpoint_workflow(
            &target(),
            vec![],
            previous,
            specify.clone(),
            None,
            Some(&specify_proof.basis_hash),
        )
        .unwrap();

    let implement_proof = crate::workflow::evaluate_phase_transition(
        &specify.state,
        crate::workflow::Phase::Implement,
        &specify.gates,
        &graph,
    )
    .unwrap();
    let mut implement = specify.clone();
    implement.state.phase = crate::workflow::Phase::Implement;
    implement.state.step = Some(crate::workflow::WorkflowStepV1::Implement);
    implement.state.revision += 1;
    assert!(
        writer
            .checkpoint_workflow(
                &target(),
                vec![],
                specify.clone(),
                implement.clone(),
                None,
                Some("wrong-basis"),
            )
            .is_err()
    );
    writer
        .checkpoint_workflow(
            &target(),
            vec![],
            specify,
            implement.clone(),
            None,
            Some(&implement_proof.basis_hash),
        )
        .unwrap();

    writer.record_intent(&target(), op()).unwrap();
    let mut uncheckpointed = implement.clone();
    uncheckpointed.state.phase = crate::workflow::Phase::Review;
    uncheckpointed.state.step = Some(crate::workflow::WorkflowStepV1::Review);
    uncheckpointed.state.revision += 1;
    assert!(
        writer
            .finish_with_messages_and_workflow(
                &target(),
                &op(),
                Observation::Succeeded,
                result(),
                vec![polaris_provider::Message::assistant("done")],
                Some(uncheckpointed),
            )
            .is_err()
    );
    writer
        .finish_with_messages_and_workflow(
            &target(),
            &op(),
            Observation::Succeeded,
            result(),
            vec![polaris_provider::Message::assistant("done")],
            Some(implement),
        )
        .unwrap();
}

#[test]
fn m27_approval_initializes_a_missing_desktop_workflow_and_survives_reopen() {
    let (root, mut writer) = setup();
    assert!(writer.snapshot().unwrap().state.workflow.is_none());
    let draft = crate::workflow::ChangeContractV1::draft(
        "desktop-m27-initial-workflow",
        crate::workflow::RoutineEligibilityV1::local_component("desktop_store"),
        "the session has no saved workflow yet",
        "contract approval previously required an existing workflow",
        "approval initializes the default workflow atomically",
        vec!["the approved contract survives reopen".into()],
    )
    .unwrap();
    let revision = writer.snapshot().unwrap().marker.session_revision;
    let draft_ref = writer.publish_change_contract(revision, draft).unwrap();
    let revision = writer.snapshot().unwrap().marker.session_revision;
    let approved_ref = writer
        .approve_change_contract_trusted(revision, &draft_ref)
        .unwrap();
    assert_eq!(
        writer
            .snapshot()
            .unwrap()
            .state
            .workflow
            .as_ref()
            .unwrap()
            .state
            .active_change_contract
            .as_ref(),
        Some(&approved_ref)
    );
    drop(writer);
    let reopened = root.open(&project(), &session()).unwrap();
    assert_eq!(
        reopened
            .snapshot()
            .unwrap()
            .state
            .workflow
            .unwrap()
            .state
            .active_change_contract,
        Some(approved_ref)
    );
}

#[test]
fn m27_reopen_rejects_an_active_contract_with_a_missing_body() {
    let (root, mut writer) = setup();
    let draft = crate::workflow::ChangeContractV1::draft(
        "desktop-m27-missing-body",
        crate::workflow::RoutineEligibilityV1::local_component("desktop_store"),
        "the graph can outlive a removed object",
        "startup only validated graph metadata",
        "startup revalidates the active immutable body",
        vec!["missing bodies fail closed".into()],
    )
    .unwrap();
    let revision = writer.snapshot().unwrap().marker.session_revision;
    let draft_ref = writer.publish_change_contract(revision, draft).unwrap();
    let revision = writer.snapshot().unwrap().marker.session_revision;
    let approved_ref = writer
        .approve_change_contract_trusted(revision, &draft_ref)
        .unwrap();
    let root_path = dir(&writer).parent().unwrap().to_path_buf();
    let object = root_path
        .join("workflow-artifacts-v1")
        .join(crate::conversation_state::content_hash(
            project().as_str().as_bytes(),
        ))
        .join("objects")
        .join(format!("{}.json", approved_ref.content_hash));
    drop(writer);
    std::fs::remove_file(object).unwrap();
    assert!(root.open(&project(), &session()).is_err());
}

#[tokio::test]
async fn workflow_lost_ack_confirms_marker_without_repeating_checkpoint() {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };
    let (_, mut writer) = setup();
    let previous = workflow_fixture();
    writer
        .configure_workflow(DecimalU64::new(0), Some(previous.clone()))
        .unwrap();
    writer
        .apply(&start("lost-workflow-ack", 0), Some(target()))
        .unwrap();
    let mut candidate = previous.clone();
    candidate.state.phase = crate::workflow::Phase::Review;
    candidate.state.revision = 1;
    let suffix = desktop_tool_turn();
    let (port, mut owner) = crate::desktop_workflow::DesktopWorkflow::pair(target());
    owner.discard_next_ack();
    let mut checkpoint = Box::pin(port.checkpoint(
        suffix.clone(),
        previous,
        candidate.clone(),
        Some("call-1".into()),
        None,
    ));
    let mut context = Context::from_waker(Waker::noop());
    assert!(matches!(
        checkpoint.as_mut().poll(&mut context),
        Poll::Pending
    ));
    owner.poll(Some(&mut writer), false, None);
    let committed = writer.snapshot().unwrap();
    assert_eq!(committed.state.workflow, Some(candidate.clone()));
    assert!(matches!(
        checkpoint.as_mut().poll(&mut context),
        Poll::Pending
    ));
    owner.poll(Some(&mut writer), true, None); // Cancellation permits only confirmation.
    assert!(matches!(
        checkpoint.as_mut().poll(&mut context),
        Poll::Ready(Ok(()))
    ));
    unchanged(&committed, &writer.snapshot().unwrap());
    candidate.state.revision = 2;
    assert!(
        writer
            .confirm_workflow_checkpoint(&target(), &suffix, &candidate)
            .is_err()
    );
    unchanged(&committed, &writer.snapshot().unwrap());
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
        tier: polaris_skills::Tier::Dwarf,
        tier_min: polaris_skills::Tier::Dwarf,
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
        endpoint: Some("http://127.0.0.1:11434".into()),
        effort: None,
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
            .configure_role_bindings(DecimalU64::new(0), Some(roles.clone()), None, &[])
            .is_err()
    );
    unchanged(&before, &writer.snapshot().unwrap());
    writer
        .configure_role_bindings(
            DecimalU64::new(0),
            Some(roles.clone()),
            None,
            &role_catalog(),
        )
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
        writer.configure_role_bindings(DecimalU64::new(0), None, None, &[]),
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
            .configure_role_bindings(DecimalU64::new(1), Some(invalid), None, &role_catalog())
            .is_err()
    );
    unchanged(&saved, &writer.snapshot().unwrap());
}

#[test]
fn saved_roles_active_update_freezes_run_and_terminal_reopen_preserves_both() {
    let (root, mut writer) = setup();
    let roles = saved_roles_fixture();
    writer
        .configure_role_bindings(
            DecimalU64::new(0),
            Some(roles.clone()),
            None,
            &role_catalog(),
        )
        .unwrap();
    let accepted = start_with_configuration("role-run", 1);
    writer.apply(&accepted, Some(target())).unwrap();
    writer.record_intent(&target(), op()).unwrap();
    let empty = SavedRoleBindings::new(vec![]).unwrap();
    writer
        .configure_role_bindings(DecimalU64::new(1), Some(empty.clone()), None, &[])
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
        .configure_role_bindings(DecimalU64::new(2), None, None, &[])
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
            .configure_role_bindings(DecimalU64::new(0), roles.clone(), None, &[])
            .unwrap();
        writer
            .apply(
                &start_with_configuration("empty-role-run", 1),
                Some(target()),
            )
            .unwrap();
        let saved = writer.snapshot().unwrap();
        let mut legacy = serde_json::to_value(&saved.state).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("desktop_sidecar_schema_version");
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
                    None,
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
                .configure_role_bindings(DecimalU64::new(1), None, None, &[])
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
        display_name: None,
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
fn child_ledger_assigns_distinct_star_names_and_does_not_reuse_within_root() {
    let (root, mut writer) = setup();
    writer.apply(&start("start", 0), Some(target())).unwrap();
    writer.record_intent(&target(), op()).unwrap();
    writer
        .record_operation_result(&target(), &op(), result())
        .unwrap();

    let first = saved_child_fixture(0);
    let second = saved_child_fixture(1);
    writer
        .record_child_start(&target(), first.clone(), "worker".into(), "one".into())
        .unwrap();
    writer
        .record_child_start(&target(), second.clone(), "worker".into(), "two".into())
        .unwrap();
    assert_eq!(
        writer.snapshot().unwrap().state.children[0]
            .child
            .display_name
            .as_deref(),
        Some("Sirius")
    );
    assert_eq!(
        writer.snapshot().unwrap().state.children[1]
            .child
            .display_name
            .as_deref(),
        Some("Canopus")
    );

    writer
        .record_child_finish(&target(), &child_identity(&first), Observation::Succeeded)
        .unwrap();
    writer
        .record_child_start(
            &target(),
            saved_child_fixture(2),
            "worker".into(),
            "three".into(),
        )
        .unwrap();
    assert_eq!(
        writer.snapshot().unwrap().state.children[2]
            .child
            .display_name
            .as_deref(),
        Some("Arcturus")
    );

    drop(writer);
    let mut writer = root.open(&project(), &session()).unwrap();
    assert_eq!(
        writer
            .record_child_start(&target(), second, "worker".into(), "two".into())
            .unwrap(),
        IntentReceipt::AlreadyRecorded
    );
    assert_eq!(
        writer.snapshot().unwrap().state.children[1]
            .child
            .display_name
            .as_deref(),
        Some("Canopus")
    );
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
    legacy
        .as_object_mut()
        .unwrap()
        .remove("desktop_sidecar_schema_version");
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

#[cfg(target_os = "linux")]
#[test]
fn m2a_source_apply_normal_open_preserves_auxiliary_directory_and_unpublished_tails() {
    use std::os::unix::fs::PermissionsExt;
    let (_root, writer) = setup();
    let path = dir(&writer);
    let root = DesktopRoot::open_owned(path.parent().unwrap()).unwrap();
    let directory = disk::Directory::open_owned(&path).unwrap();
    let marker = disk::read_marker(&directory).unwrap();
    drop(writer);
    let auxiliary = path.join("auxiliary");
    fs::create_dir(&auxiliary).unwrap();
    fs::set_permissions(&auxiliary, fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(auxiliary.join("sentinel"), b"caller-owned auxiliary data").unwrap();
    for name in [disk::RAW, disk::SIDECAR] {
        fs::OpenOptions::new()
            .append(true)
            .open(path.join(name))
            .unwrap()
            .write_all(b"unpublished, incomplete tail\n")
            .unwrap();
    }
    let raw = fs::read(path.join(disk::RAW)).unwrap();
    let sidecar = fs::read(path.join(disk::SIDECAR)).unwrap();
    let _reopened = root.open(&project(), &session()).unwrap();
    assert_eq!(disk::read_marker(&directory).unwrap(), marker);
    assert_eq!(fs::read(path.join(disk::RAW)).unwrap(), raw);
    assert_eq!(fs::read(path.join(disk::SIDECAR)).unwrap(), sidecar);
    assert_eq!(
        fs::read(auxiliary.join("sentinel")).unwrap(),
        b"caller-owned auxiliary data"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn m2a_source_apply_recovery_open_preserves_all_files_and_calls_no_mutations() {
    let (_root, mut writer) = setup();
    source_terminal(&mut writer);
    let (candidate, proof) = source_allow(&mut writer);
    let pre = m2a_precommit(
        &writer,
        &candidate,
        proof,
        ResultId::new("readonly-result").unwrap(),
    )
    .unwrap()
    .precommit()
    .clone();
    let baseline = m2a_session_files(&writer);
    let path = dir(&writer);
    let root = DesktopRoot::open_owned(dir(&writer).parent().unwrap()).unwrap();
    drop(writer);
    disk::M2A_MUTATION_CALLS.with(|calls| calls.set([0; 3]));
    let inspected = root
        .recovery_open_m2a(&project(), &session(), pre.project_binding)
        .unwrap();
    assert_eq!(disk::M2A_MUTATION_CALLS.with(|calls| calls.get()), [0; 3]);
    assert_eq!(
        inspected.view.snapshot.marker.session_revision.get(),
        pre.base_session_revision
    );
    assert_eq!(
        inspected.view.state_next.classification,
        StateNextClassificationV1::Absent
    );
    assert_eq!(inspected.view.files.len(), 4);
    assert!(inspected.legacy_terminal_notices.is_empty());
    assert!(matches!(
        root.open(&project(), &session()),
        Err(StoreError::Busy)
    ));
    drop(inspected);
    // Re-open only the disk directory to compare the complete original fixture.
    let directory = disk::Directory::open_owned(&path).unwrap();
    assert_eq!(m2a_directory_files(&path), baseline);
    let names = directory.names_m2a().unwrap();
    assert_eq!(names.len(), 4);
}

#[cfg(target_os = "linux")]
#[test]
fn m2a_source_apply_state_next_all_safe_and_unsafe_classes_are_readonly() {
    use std::os::unix::fs::PermissionsExt;
    let (_root, writer) = setup();
    let directory = disk::Directory::open_owned(&dir(&writer)).unwrap();
    let target = b"target-marker";
    assert_eq!(
        directory
            .inspect_next_m2a(Some(target))
            .unwrap()
            .classification,
        StateNextClassificationV1::Absent
    );
    for (bytes, class) in [
        (&b""[..], StateNextClassificationV1::ProperPrefix),
        (&target[..1], StateNextClassificationV1::ProperPrefix),
        (
            &target[..target.len() - 1],
            StateNextClassificationV1::ProperPrefix,
        ),
        (&target[..], StateNextClassificationV1::ExactTarget),
        (&b"foreign!!!!!!"[..], StateNextClassificationV1::Foreign),
        (&b"target-marker+"[..], StateNextClassificationV1::Overlong),
    ] {
        let mut file = directory.open("state.next", true, true).unwrap();
        file.write_all(bytes).unwrap();
        drop(file);
        disk::M2A_MUTATION_CALLS.with(|calls| calls.set([0; 3]));
        let inspected = directory.inspect_next_m2a(Some(target)).unwrap();
        assert_eq!(inspected.classification, class);
        assert_eq!(inspected.bytes.as_deref(), Some(bytes));
        assert_eq!(
            inspected.hash,
            Some(crate::management_effect::ManagementEffectHashV1::digest(
                bytes
            ))
        );
        assert_eq!(inspected.mode, Some(0o600));
        assert_eq!(disk::M2A_MUTATION_CALLS.with(|calls| calls.get()), [0; 3]);
        assert_eq!(fs::read(dir(&writer).join("state.next")).unwrap(), bytes);
        fs::remove_file(dir(&writer).join("state.next")).unwrap();
    }
    for length in [65_536, 65_537] {
        let mut file = directory.open("state.next", true, true).unwrap();
        let bytes = vec![b'x'; length];
        file.write_all(&bytes).unwrap();
        drop(file);
        let inspected = directory.inspect_next_m2a(Some(target)).unwrap();
        assert_eq!(
            inspected.classification,
            StateNextClassificationV1::Overlong
        );
        assert_eq!(inspected.bytes.is_some(), length == 65_536);
        assert_eq!(
            inspected.hash,
            Some(crate::management_effect::ManagementEffectHashV1::digest(
                &bytes
            ))
        );
        fs::remove_file(dir(&writer).join("state.next")).unwrap();
    }
    for mode in [0o400, 0o640] {
        let file = directory.open("state.next", true, true).unwrap();
        drop(file);
        fs::set_permissions(
            dir(&writer).join("state.next"),
            fs::Permissions::from_mode(mode),
        )
        .unwrap();
        let inspected = directory.inspect_next_m2a(Some(target)).unwrap();
        assert_eq!(
            inspected.classification,
            StateNextClassificationV1::UnsafeInode
        );
        assert!(inspected.bytes.is_none() && inspected.hash.is_none());
        fs::remove_file(dir(&writer).join("state.next")).unwrap();
    }
    std::os::unix::fs::symlink("raw.jsonl", dir(&writer).join("state.next")).unwrap();
    assert_eq!(
        directory
            .inspect_next_m2a(Some(target))
            .unwrap()
            .classification,
        StateNextClassificationV1::UnsafeInode
    );
    fs::remove_file(dir(&writer).join("state.next")).unwrap();
    fs::hard_link(
        dir(&writer).join(disk::RAW),
        dir(&writer).join("state.next"),
    )
    .unwrap();
    assert_eq!(
        directory
            .inspect_next_m2a(Some(target))
            .unwrap()
            .classification,
        StateNextClassificationV1::UnsafeInode
    );
    fs::remove_file(dir(&writer).join("state.next")).unwrap();
    fs::create_dir(dir(&writer).join("state.next")).unwrap();
    assert_eq!(
        directory
            .inspect_next_m2a(Some(target))
            .unwrap()
            .classification,
        StateNextClassificationV1::UnsafeInode
    );
    fs::remove_dir(dir(&writer).join("state.next")).unwrap();
}

#[cfg(target_os = "linux")]
#[test]
fn m2a_source_apply_joined_marker_recovery_uses_exact_precommit_and_report_at_publication_stops() {
    for result_publication in [false, true] {
        for point in [
            "marker_partial",
            "marker_sync",
            "marker_rename",
            "directory_sync",
        ] {
            let (_prototype, mut writer) = setup();
            source_terminal(&mut writer);
            let (candidate, proof) = source_allow(&mut writer);
            let token = m2a_precommit(
                &writer,
                &candidate,
                proof,
                ResultId::new("joined-result").unwrap(),
            )
            .unwrap();
            let pre = token.precommit().clone();
            let (descriptor, binding, request, before, generic) =
                m2a_result_artifacts(&writer, &pre);
            let root = DesktopRoot::open_owned(dir(&writer).parent().unwrap()).unwrap();
            let path = dir(&writer);
            let guard = source_guard(&writer);
            let mut token = Some(token);
            if result_publication {
                writer
                    .consume_source_apply_precommitted_m2a(token.take().unwrap(), guard, proof)
                    .unwrap();
            }
            disk::FAIL.with(|f| f.set(Some(point)));
            let failed = if result_publication {
                writer
                    .record_source_apply_result_evidence_m2a(
                        &pre,
                        &descriptor,
                        pre.reserved_result_id.clone(),
                        M2A_REPORT,
                        m2a_result_hash(M2A_REPORT),
                    )
                    .map(|_| ())
            } else {
                writer
                    .consume_source_apply_precommitted_m2a(token.take().unwrap(), guard, proof)
                    .map(|_| ())
            };
            assert!(failed.is_err(), "{point}");
            disk::FAIL.with(|f| f.set(None));
            let frozen = m2a_directory_files(&path);
            drop(writer);
            disk::M2A_MUTATION_CALLS.with(|calls| calls.set([0; 3]));
            disk::M2A_REMOVE_NEXT_CALLS.with(|calls| calls.set(0));
            let opened = root
                .recovery_open_m2a(&project(), &session(), pre.project_binding)
                .unwrap();
            assert_eq!(m2a_directory_files(&path), frozen);
            assert_eq!(disk::M2A_MUTATION_CALLS.with(|calls| calls.get()), [0; 3]);
            let report = result_publication.then_some((M2A_REPORT, m2a_result_hash(M2A_REPORT)));
            let joined = opened
                .join_marker_recovery_m2a(
                    &pre,
                    &descriptor,
                    &binding,
                    &request,
                    &before,
                    &generic,
                    report,
                )
                .unwrap();
            assert_eq!(m2a_directory_files(&path), frozen);
            assert_eq!(disk::M2A_MUTATION_CALLS.with(|calls| calls.get()), [0; 3]);
            let (writer, committed, kind) = opened.recover_published_m2a(joined).unwrap();
            assert!(committed);
            assert_eq!(
                kind,
                if result_publication {
                    D27PublicationKindV1::Result
                } else {
                    D27PublicationKindV1::Consume
                }
            );
            let state = writer.snapshot().unwrap();
            assert_eq!(
                state.marker.session_revision.get(),
                pre.consume_target_revision + u64::from(result_publication)
            );
            let record = state
                .state
                .source_applies
                .iter()
                .find(|r| r.candidate.approval_id == candidate.approval_id)
                .unwrap();
            assert_eq!(
                record.reserved_result_id.as_ref(),
                Some(&pre.reserved_result_id)
            );
            assert_eq!(record.result.is_some(), result_publication);
            assert!(!path.join("state.next").exists());
            assert_eq!(disk::M2A_REMOVE_NEXT_CALLS.with(|calls| calls.get()), 0);
            assert!(disk::M2A_MUTATION_CALLS.with(|calls| calls.get())[1] > 0);
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn m2a_source_apply_joined_marker_token_rejects_changed_inode_mode_link_length_and_bytes() {
    use std::os::unix::fs::PermissionsExt;
    for change in 0..5 {
        let (_prototype, mut writer) = setup();
        source_terminal(&mut writer);
        let (candidate, proof) = source_allow(&mut writer);
        let token = m2a_precommit(
            &writer,
            &candidate,
            proof,
            ResultId::new("joined-result").unwrap(),
        )
        .unwrap();
        let pre = token.precommit().clone();
        let (descriptor, binding, request, before, generic) = m2a_result_artifacts(&writer, &pre);
        let root = DesktopRoot::open_owned(dir(&writer).parent().unwrap()).unwrap();
        let path = dir(&writer);
        let guard = source_guard(&writer);
        disk::FAIL.with(|f| f.set(Some("marker_partial")));
        assert!(
            writer
                .consume_source_apply_precommitted_m2a(token, guard, proof)
                .is_err()
        );
        disk::FAIL.with(|f| f.set(None));
        drop(writer);
        let opened = root
            .recovery_open_m2a(&project(), &session(), pre.project_binding)
            .unwrap();
        let joined = opened
            .join_marker_recovery_m2a(
                &pre,
                &descriptor,
                &binding,
                &request,
                &before,
                &generic,
                None,
            )
            .unwrap();
        let next = path.join("state.next");
        let mut bytes = fs::read(&next).unwrap();
        match change {
            0 => {
                fs::rename(&next, path.join("retained-next")).unwrap();
                let file = std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(&next)
                    .unwrap();
                file.set_permissions(fs::Permissions::from_mode(0o600))
                    .unwrap();
                fs::write(&next, &bytes).unwrap();
            }
            1 => {
                fs::set_permissions(&next, fs::Permissions::from_mode(0o400)).unwrap();
            }
            2 => {
                fs::hard_link(&next, path.join("retained-link")).unwrap();
            }
            3 => {
                bytes.push(b'x');
                fs::write(&next, &bytes).unwrap();
            }
            _ => {
                bytes[0] ^= 1;
                fs::write(&next, &bytes).unwrap();
            }
        }
        let frozen = m2a_directory_files(&path);
        disk::M2A_MUTATION_CALLS.with(|calls| calls.set([0; 3]));
        assert!(opened.recover_published_m2a(joined).is_err());
        assert_eq!(disk::M2A_MUTATION_CALLS.with(|calls| calls.get()), [0; 3]);
        assert_eq!(m2a_directory_files(&path), frozen);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn m2a_source_apply_recovery_open_validates_artifacts_without_namespace_or_sync_calls() {
    let (_prototype, mut writer) = setup();
    let workflow = SavedWorkflow::capture(&crate::workflow::SessionWorkflow::new(
        crate::workflow::WorkflowConfig {
            enabled: true,
            ..Default::default()
        },
    ));
    writer
        .configure_workflow(DecimalU64::new(0), Some(workflow))
        .unwrap();
    let draft = crate::workflow::ChangeContractV1::draft(
        "readonly-artifact",
        crate::workflow::RoutineEligibilityV1::local_component("desktop_store"),
        "need",
        "cause",
        "outcome",
        vec!["criterion".into()],
    )
    .unwrap();
    let rev = writer.snapshot().unwrap().marker.session_revision;
    let reference = writer.publish_change_contract(rev, draft).unwrap();
    let rev = writer.snapshot().unwrap().marker.session_revision;
    writer
        .approve_change_contract_trusted(rev, &reference)
        .unwrap();
    let root = DesktopRoot::open_owned(dir(&writer).parent().unwrap()).unwrap();
    let path = dir(&writer);
    let frozen = m2a_directory_files(&path);
    drop(writer);
    disk::M2A_MUTATION_CALLS.with(|calls| calls.set([0; 3]));
    let opened = root
        .recovery_open_m2a(
            &project(),
            &session(),
            crate::management_effect::ManagementEffectHashV1::digest(b"project"),
        )
        .unwrap();
    assert!(
        opened
            .view
            .snapshot
            .state
            .workflow
            .as_ref()
            .unwrap()
            .state
            .active_change_contract
            .is_some()
    );
    assert_eq!(disk::M2A_MUTATION_CALLS.with(|calls| calls.get()), [0; 3]);
    assert_eq!(m2a_directory_files(&path), frozen);
}

#[cfg(target_os = "linux")]
#[test]
fn m2a_source_apply_legacy_pending_is_typed_blocker_and_reported_is_privacy_bounded_notice() {
    use crate::management_effect::ManagementEffectHashV1 as Hash;
    for version in [1, 2] {
        for reported in [false, true] {
            let (_prototype, mut writer) = setup();
            source_terminal(&mut writer);
            let (candidate, proof) = source_allow(&mut writer);
            writer
                .consume_source_apply_intent(
                    &target(),
                    &candidate.approval_id,
                    &candidate.payload_hash,
                    source_guard(&writer),
                    proof,
                )
                .unwrap();
            if reported {
                let rev = writer.snapshot().unwrap().marker.session_revision;
                writer
                    .record_source_apply_result(
                        &target(),
                        &candidate.operation_id,
                        rev,
                        source_report(),
                    )
                    .unwrap();
            }
            let root = DesktopRoot::open_owned(dir(&writer).parent().unwrap()).unwrap();
            let path = dir(&writer);
            let mut marker = writer.snapshot().unwrap().marker;
            drop(writer);
            if version == 1 {
                let mut legacy = Vec::new();
                for line in fs::read(path.join(disk::SIDECAR))
                    .unwrap()
                    .split_inclusive(|b| *b == b'\n')
                {
                    let mut state: serde_json::Value = serde_json::from_slice(line).unwrap();
                    state
                        .as_object_mut()
                        .unwrap()
                        .remove("desktop_sidecar_schema_version");
                    for record in state["source_applies"].as_array_mut().unwrap() {
                        record.as_object_mut().unwrap().remove("reserved_result_id");
                    }
                    legacy.extend(disk::line(&state).unwrap());
                }
                marker.sidecar_offset = DecimalU64::new(legacy.len() as u64);
                marker.sidecar_hash = crate::conversation_state::content_hash(&legacy);
                fs::write(path.join(disk::SIDECAR), legacy).unwrap();
                fs::write(
                    path.join(disk::MARKER),
                    serde_json::to_vec(&marker).unwrap(),
                )
                .unwrap();
            }
            let frozen = m2a_directory_files(&path);
            disk::M2A_MUTATION_CALLS.with(|calls| calls.set([0; 3]));
            let outcome = root.recovery_open_m2a(&project(), &session(), Hash::digest(b"project"));
            let diagnostic = match outcome {
                Err(D27RecoveryOpenError::LegacyUnboundIntent(d)) => {
                    assert!(!reported);
                    *d
                }
                Ok(opened) => {
                    assert!(reported);
                    assert_eq!(opened.legacy_terminal_notices.len(), 1);
                    opened.legacy_terminal_notices[0].clone()
                }
                Err(other) => panic!("{other}"),
            };
            assert_eq!(diagnostic.result_present, reported);
            assert_eq!(diagnostic.record_index, 0);
            assert_eq!(diagnostic.session_revision, marker.session_revision.get());
            let wire = serde_json::to_vec(&diagnostic).unwrap();
            let value: serde_json::Value = serde_json::from_slice(&wire).unwrap();
            assert_eq!(value.as_object().unwrap().len(), 12);
            assert!(wire.starts_with(br#"{"schema_version":1,"code":"legacy_source_apply_binding_missing","project_binding":"#));
            assert_eq!(
                diagnostic.next_action,
                if reported {
                    "preserve_terminal_legacy_record"
                } else {
                    "complete_with_legacy_desktop"
                }
            );
            let text = String::from_utf8(wire).unwrap();
            for forbidden in [candidate.operation_id.as_str(), "/source", "file.txt"] {
                assert!(!text.contains(forbidden));
            }
            assert_eq!(disk::M2A_MUTATION_CALLS.with(|calls| calls.get()), [0; 3]);
            assert_eq!(m2a_directory_files(&path), frozen);
        }
    }
}

#[cfg(target_os = "linux")]
type D27RestartFixture = (
    SourceApplyPrecommitV1,
    crate::apply_intent::ApplyIntentDescriptorV1,
    crate::apply_intent::D27SourceBindingV1,
    crate::apply_intent::RequestV1,
    crate::apply_intent::ObservedSourceV1,
    crate::management_effect::PrepareManagementEffectV1,
);

// Child entry point, excluded from the acceptance selector. The parent tests
// below own all positive assertions and capture this process's output.
#[cfg(target_os = "linux")]
#[test]
fn d27_marker_restart_worker() {
    let Some(path) = std::env::var_os("POLARIS_D27_RESTART_FIXTURE") else {
        return;
    };
    let root = DesktopRoot::open_owned(std::path::Path::new(&path)).unwrap();
    let point = std::env::var("POLARIS_D27_RESTART_POINT").unwrap();
    let result_publication = std::env::var("POLARIS_D27_RESTART_RESULT").unwrap() == "true";
    let fixture_path = std::path::Path::new(&path).join("restart-fixture.json");
    if !fixture_path.exists() {
        let mut writer = root.create(project(), session(), initial()).unwrap();
        source_terminal(&mut writer);
        let (candidate, proof) = source_allow(&mut writer);
        let token = m2a_precommit(
            &writer,
            &candidate,
            proof,
            ResultId::new("fresh-process-result").unwrap(),
        )
        .unwrap();
        let pre = token.precommit().clone();
        let (descriptor, binding, request, before, generic) = m2a_result_artifacts(&writer, &pre);
        let fixture = (
            pre.clone(),
            descriptor.clone(),
            binding,
            request,
            before,
            generic,
        );
        fs::write(&fixture_path, serde_json::to_vec(&fixture).unwrap()).unwrap();
        let guard = source_guard(&writer);
        if result_publication {
            writer
                .consume_source_apply_precommitted_m2a(token, guard, proof)
                .unwrap();
            disk::FAIL.with(|f| f.set(Some(point.as_str().to_owned().leak())));
            assert!(
                writer
                    .record_source_apply_result_evidence_m2a(
                        &pre,
                        &descriptor,
                        pre.reserved_result_id.clone(),
                        M2A_REPORT,
                        m2a_result_hash(M2A_REPORT)
                    )
                    .is_err()
            );
        } else {
            disk::FAIL.with(|f| f.set(Some(point.as_str().to_owned().leak())));
            assert!(
                writer
                    .consume_source_apply_precommitted_m2a(token, guard, proof)
                    .is_err()
            );
        }
        // Exit bypasses all destructor recovery/cleanup, leaving the exact
        // failed publication for a genuinely fresh process.
        std::process::exit(74);
    }
    let (pre, descriptor, binding, request, before, generic): D27RestartFixture =
        serde_json::from_slice(&fs::read(&fixture_path).unwrap()).unwrap();
    let session_path = std::path::Path::new(&path).join(crate::conversation_state::content_hash(
        session().as_str().as_bytes(),
    ));
    let frozen = m2a_directory_files(&session_path);
    disk::M2A_MUTATION_CALLS.with(|calls| calls.set([0; 3]));
    let opened = root
        .recovery_open_m2a(&project(), &session(), pre.project_binding)
        .unwrap();
    assert_eq!(disk::M2A_MUTATION_CALLS.with(|calls| calls.get()), [0; 3]);
    assert_eq!(m2a_directory_files(&session_path), frozen);
    let report = result_publication.then_some((M2A_REPORT, m2a_result_hash(M2A_REPORT)));
    let joined = opened
        .join_marker_recovery_m2a(
            &pre,
            &descriptor,
            &binding,
            &request,
            &before,
            &generic,
            report,
        )
        .unwrap();
    assert_eq!(disk::M2A_MUTATION_CALLS.with(|calls| calls.get()), [0; 3]);
    let (writer, committed, _) = opened.recover_published_m2a(joined).unwrap();
    assert!(committed);
    let state = writer.snapshot().unwrap();
    assert_eq!(
        state.marker.session_revision.get(),
        pre.consume_target_revision + u64::from(result_publication)
    );
    assert_eq!(
        state.state.source_applies[0].result.is_some(),
        result_publication
    );
    assert!(!session_path.join("state.next").exists());
    assert_eq!(disk::M2A_REMOVE_NEXT_CALLS.with(|calls| calls.get()), 0);
}

#[cfg(target_os = "linux")]
#[test]
fn m2a_source_apply_marker_publication_restarts_in_fresh_processes() {
    use std::os::unix::fs::PermissionsExt;
    for result in [false, true] {
        for point in [
            "marker_partial",
            "marker_sync",
            "marker_rename",
            "directory_sync",
        ] {
            let mut builder = tempfile::Builder::new();
            builder
                .prefix("d27-marker-restart-")
                .permissions(fs::Permissions::from_mode(0o700));
            let fixture = builder.tempdir().unwrap();
            for expected_exit in [74, 0] {
                let output = std::process::Command::new(std::env::current_exe().unwrap())
                    .args([
                        "--exact",
                        "desktop_store::tests::d27_marker_restart_worker",
                        "--nocapture",
                    ])
                    .env("POLARIS_D27_RESTART_FIXTURE", fixture.path())
                    .env("POLARIS_D27_RESTART_POINT", point)
                    .env("POLARIS_D27_RESTART_RESULT", result.to_string())
                    .output()
                    .unwrap();
                assert_eq!(
                    output.status.code(),
                    Some(expected_exit),
                    "{point}/{result}: {}",
                    String::from_utf8_lossy(&output.stderr)
                );
                if expected_exit == 0 {
                    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"));
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
#[test]
fn m2a_source_apply_marker_recovery_stops_resume_without_new_names_or_unjoined_cleanup() {
    for point in [
        "m2a_prefix_unlinked",
        "m2a_prefix_cleanup_sync",
        "m2a_next_created",
        "marker_partial",
        "marker_sync",
        "m2a_next_synced",
        "marker_rename",
        "directory_sync",
        "m2a_directory_synced",
    ] {
        let (_prototype, mut writer) = setup();
        source_terminal(&mut writer);
        let (candidate, proof) = source_allow(&mut writer);
        let token = m2a_precommit(
            &writer,
            &candidate,
            proof,
            ResultId::new("recovery-result").unwrap(),
        )
        .unwrap();
        let pre = token.precommit().clone();
        let (descriptor, binding, request, before, generic) = m2a_result_artifacts(&writer, &pre);
        let root = DesktopRoot::open_owned(dir(&writer).parent().unwrap()).unwrap();
        let path = dir(&writer);
        let guard = source_guard(&writer);
        disk::FAIL.with(|f| f.set(Some("marker_partial")));
        assert!(
            writer
                .consume_source_apply_precommitted_m2a(token, guard, proof)
                .is_err()
        );
        disk::FAIL.with(|f| f.set(None));
        drop(writer);
        let opened = root
            .recovery_open_m2a(&project(), &session(), pre.project_binding)
            .unwrap();
        let joined = opened
            .join_marker_recovery_m2a(
                &pre,
                &descriptor,
                &binding,
                &request,
                &before,
                &generic,
                None,
            )
            .unwrap();
        disk::FAIL.with(|f| f.set(Some(point)));
        assert!(opened.recover_published_m2a(joined).is_err(), "{point}");
        disk::FAIL.with(|f| f.set(None));
        let frozen = m2a_directory_files(&path);
        let opened = root
            .recovery_open_m2a(&project(), &session(), pre.project_binding)
            .unwrap();
        let joined = opened
            .join_marker_recovery_m2a(
                &pre,
                &descriptor,
                &binding,
                &request,
                &before,
                &generic,
                None,
            )
            .unwrap();
        assert_eq!(m2a_directory_files(&path), frozen);
        let (writer, committed, _) = opened.recover_published_m2a(joined).unwrap();
        assert!(committed);
        assert_eq!(
            writer.snapshot().unwrap().marker.session_revision.get(),
            pre.consume_target_revision
        );
        assert_eq!(m2a_directory_files(&path).len(), 4);
    }
}

#[cfg(target_os = "linux")]
#[test]
fn m2a_source_apply_state_next_wrong_owner_simulation_and_actual_open_race_refuse_without_mutations()
 {
    use std::os::unix::fs::PermissionsExt;
    let (_root, writer) = setup();
    let path = dir(&writer);
    let directory = disk::Directory::open_owned(&path).unwrap();
    let mut file = directory.open("state.next", true, true).unwrap();
    file.write_all(b"target").unwrap();
    drop(file);
    disk::M2A_NEXT_UID
        .with(|value| value.set(Some(unsafe { libc::geteuid() }.checked_add(1).unwrap())));
    disk::M2A_MUTATION_CALLS.with(|calls| calls.set([0; 3]));
    let wrong_owner = directory.inspect_next_m2a(Some(b"target")).unwrap();
    disk::M2A_NEXT_UID.with(|value| value.set(None));
    assert_eq!(
        wrong_owner.classification,
        StateNextClassificationV1::UnsafeInode
    );
    assert!(wrong_owner.hash.is_none() && wrong_owner.bytes.is_none());
    assert_eq!(disk::M2A_MUTATION_CALLS.with(|calls| calls.get()), [0; 3]);
    let (start, started) = std::sync::mpsc::channel();
    let (finished, done) = std::sync::mpsc::channel();
    let thread_path = path.clone();
    let racer = std::thread::spawn(move || {
        started.recv().unwrap();
        fs::rename(thread_path.join("state.next"), thread_path.join("old-next")).unwrap();
        fs::write(thread_path.join("state.next"), b"target").unwrap();
        fs::set_permissions(
            thread_path.join("state.next"),
            fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        finished.send(()).unwrap();
    });
    disk::M2A_NEXT_RACE.with(|hook| *hook.borrow_mut() = Some((start, done)));
    let raced = directory.inspect_next_m2a(Some(b"target")).unwrap();
    racer.join().unwrap();
    assert_eq!(raced.classification, StateNextClassificationV1::Raced);
    assert!(raced.hash.is_none() && raced.bytes.is_none());
    assert_eq!(disk::M2A_MUTATION_CALLS.with(|calls| calls.get()), [0; 3]);
    assert_eq!(fs::read(path.join("state.next")).unwrap(), b"target");
    assert_eq!(fs::read(path.join("old-next")).unwrap(), b"target");
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
const M2A_PRECOMMIT: &str = r#"{"schema_version":1,"project_binding":"1111111111111111111111111111111111111111111111111111111111111111","run_id":"run","attempt_id":"attempt","approval_id":"approval","source_operation_digest":"2222222222222222222222222222222222222222222222222222222222222222","payload_hash":"3333333333333333333333333333333333333333333333333333333333333333","policy_revision":7,"base_session_revision":8,"consume_target_revision":9,"reserved_result_id":"result-原文","reserved_result_id_digest":"1fec8a4ec4b8065d13d67b4258adfc8ecf8d6059d319211e609e51b3a45ce8db","base_marker_evidence_hash":"4444444444444444444444444444444444444444444444444444444444444444","consume_marker_evidence_hash":"5555555555555555555555555555555555555555555555555555555555555555","consume_sidecar_offset":100,"consume_sidecar_hash":"6666666666666666666666666666666666666666666666666666666666666666","consume_record_hash":"7777777777777777777777777777777777777777777777777777777777777777","result_capacity_bytes":1048576}"#;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn m2a_session_files(
    writer: &Writer,
) -> std::collections::BTreeMap<String, (u64, u64, u32, u32, u64, Vec<u8>)> {
    m2a_directory_files(&dir(writer))
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn m2a_directory_files(
    path: &std::path::Path,
) -> std::collections::BTreeMap<String, (u64, u64, u32, u32, u64, Vec<u8>)> {
    use std::os::unix::fs::MetadataExt;
    fs::read_dir(path)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            let metadata = entry.path().symlink_metadata().unwrap();
            let bytes = if metadata.is_file() {
                fs::read(entry.path()).unwrap()
            } else {
                Vec::new()
            };
            (
                entry.file_name().into_string().unwrap(),
                (
                    metadata.dev(),
                    metadata.ino(),
                    metadata.mode(),
                    metadata.uid(),
                    metadata.nlink(),
                    bytes,
                ),
            )
        })
        .collect()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_source_apply_precommit_independent_bytes_and_hash_vectors() {
    let value = SourceApplyPrecommitV1::from_canonical_bytes(M2A_PRECOMMIT.as_bytes()).unwrap();
    assert_eq!(value.canonical_bytes().unwrap(), M2A_PRECOMMIT.as_bytes());
    assert_eq!(
        value.precommit_hash().unwrap().as_hex(),
        "343eb447f4320fca11ccc6c8d921c0e5793b97b0125c9c1edc796107e459afca"
    );
    assert_eq!(
        super::source_apply::marker_evidence_hash(br#"{"marker":1}"#, b"row\n").as_hex(),
        "ef0a406474ef5f528e60c6895e044107a41ffc4a1ca8b8ff1e9aa2c711489a9e"
    );
    let marker = br#"{"marker":1}"#;
    assert_ne!(
        super::source_apply::marker_evidence_hash(marker, b"row\n"),
        super::source_apply::marker_evidence_hash(b" {\"marker\":1}", b"row\n")
    );
    assert_ne!(
        super::source_apply::marker_evidence_hash(marker, b"row\n"),
        super::source_apply::marker_evidence_hash(marker, b"row")
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_source_apply_precommit_closed_schema_and_boundaries() {
    let value: serde_json::Value = serde_json::from_str(M2A_PRECOMMIT).unwrap();
    assert_eq!(value.as_object().unwrap().len(), 18);
    for key in value.as_object().unwrap().keys() {
        let mut changed = value.clone();
        changed.as_object_mut().unwrap().remove(key);
        assert!(
            SourceApplyPrecommitV1::from_canonical_bytes(&serde_json::to_vec(&changed).unwrap())
                .is_err(),
            "missing {key}"
        );
        let duplicate = format!(
            "{{{},{}",
            serde_json::to_string(key).unwrap()
                + ":"
                + &serde_json::to_string(&value[key]).unwrap(),
            &M2A_PRECOMMIT[1..]
        );
        assert!(
            SourceApplyPrecommitV1::from_canonical_bytes(duplicate.as_bytes()).is_err(),
            "duplicate {key}"
        );
    }
    let mut valid = SourceApplyPrecommitV1::from_canonical_bytes(M2A_PRECOMMIT.as_bytes()).unwrap();
    valid.result_capacity_bytes = 1;
    valid.canonical_bytes().unwrap();
    valid.result_capacity_bytes = MAX_SOURCE_APPLY_BYTES as u64;
    valid.canonical_bytes().unwrap();
    for limit in [0, MAX_SOURCE_APPLY_BYTES as u64 + 1] {
        valid.result_capacity_bytes = limit;
        assert!(valid.canonical_bytes().is_err());
    }
    for (key, replacement) in [
        ("schema_version", serde_json::json!(2)),
        ("schema_version", serde_json::json!(1.0)),
        ("consume_target_revision", serde_json::json!(8)),
        ("consume_target_revision", serde_json::json!(10)),
        ("base_session_revision", serde_json::json!(u64::MAX)),
        ("consume_sidecar_offset", serde_json::json!(0)),
        ("reserved_result_id", serde_json::json!("result-other")),
        ("policy_revision", serde_json::json!("7")),
    ] {
        let mut changed = value.clone();
        changed[key] = replacement;
        assert!(
            SourceApplyPrecommitV1::from_canonical_bytes(&serde_json::to_vec(&changed).unwrap())
                .is_err(),
            "{key}"
        );
    }
    let zero = crate::management_effect::ManagementEffectHashV1::from_hex(&"0".repeat(64)).unwrap();
    for index in 0..8 {
        let mut changed =
            SourceApplyPrecommitV1::from_canonical_bytes(M2A_PRECOMMIT.as_bytes()).unwrap();
        let fields = [
            &mut changed.project_binding,
            &mut changed.source_operation_digest,
            &mut changed.payload_hash,
            &mut changed.reserved_result_id_digest,
            &mut changed.base_marker_evidence_hash,
            &mut changed.consume_marker_evidence_hash,
            &mut changed.consume_sidecar_hash,
            &mut changed.consume_record_hash,
        ];
        *fields.into_iter().nth(index).unwrap() = zero;
        assert!(changed.canonical_bytes().is_err());
    }
    assert!(
        SourceApplyPrecommitV1::from_canonical_bytes(format!(" {M2A_PRECOMMIT}").as_bytes())
            .is_err()
    );
    assert!(
        SourceApplyPrecommitV1::from_canonical_bytes(
            M2A_PRECOMMIT
                .replace("\"schema_version\":1", "\"extra\":0,\"schema_version\":1")
                .as_bytes()
        )
        .is_err()
    );
    assert!(
        SourceApplyPrecommitV1::from_canonical_bytes(&serde_json::to_vec(&value).unwrap()).is_err(),
        "reordered"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn m2a_precommit(
    writer: &Writer,
    candidate: &SourceApplyCandidate,
    proof: SourceApplyIdentityProof,
    reserved: ResultId,
) -> StoreResult<SourceApplyPrecommitTokenV1> {
    writer.precommit_source_apply_m2a(
        &target(),
        &candidate.approval_id,
        &candidate.payload_hash,
        source_guard(writer),
        proof,
        crate::management_effect::ManagementEffectHashV1::digest(b"verified-project-binding"),
        reserved,
        MAX_SOURCE_APPLY_BYTES as u64,
    )
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
const M2A_REPORT: &[u8] = br#"{"source_path":"/source","recovery_path":null,"recovery_identity":null,"entries":[{"relative_path":"file.txt","source_parent_chain":[],"phase":"Preparing","old_retained":false,"installed":false,"deleted":false,"restored":false,"old_name":"old-0","candidate_name":"candidate-0","install_name":"install-0","candidate_identity":null,"install_identity":null}],"failure":{"kind":"Conflict","entry":0,"os_error":null}}"#;

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn m2a_result_hash(bytes: &[u8]) -> crate::management_effect::ManagementEffectHashV1 {
    let mut value = b"polaris-workspace-apply-report-v1\0".to_vec();
    value.extend(bytes);
    crate::management_effect::ManagementEffectHashV1::digest(&value)
}

// Synthetic source/recovery coordinates exercise only the D27 ledger API.
// Actual pinned source effects and capacity admission belong to F5.
#[cfg(any(target_os = "linux", target_os = "macos"))]
fn m2a_result_descriptor(
    writer: &Writer,
    precommit: &SourceApplyPrecommitV1,
) -> crate::apply_intent::ApplyIntentDescriptorV1 {
    m2a_result_artifacts(writer, precommit).0
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn m2a_result_artifacts(
    writer: &Writer,
    precommit: &SourceApplyPrecommitV1,
) -> (
    crate::apply_intent::ApplyIntentDescriptorV1,
    crate::apply_intent::D27SourceBindingV1,
    crate::apply_intent::RequestV1,
    crate::apply_intent::ObservedSourceV1,
    crate::management_effect::PrepareManagementEffectV1,
) {
    use crate::apply_intent::*;
    use crate::management_effect::*;
    let current = writer.snapshot().unwrap();
    let candidate = &current
        .state
        .source_applies
        .iter()
        .find(|record| record.candidate.approval_id == precommit.approval_id)
        .unwrap()
        .candidate;
    let payload = &candidate.payload;
    let source = IdentityV1 {
        device: payload.source_identity.device.get(),
        inode: payload.source_identity.inode.get(),
    };
    let recovery = IdentityV1 {
        device: payload.recovery_parent_identity.device.get(),
        inode: payload.recovery_parent_identity.inode.get(),
    };
    let manifest = D27ManifestProjectionV1 {
        schema_version: 1,
        source_path: payload.source_path.clone(),
        source_identity: source,
        recovery_parent_path: payload.recovery_parent_path.clone(),
        recovery_parent_identity: recovery,
        entries: payload
            .entries
            .iter()
            .map(|entry| {
                let version = |value: &SourceApplyVersion| D27ManifestVersionV1 {
                    device: source.device,
                    inode: 30,
                    hash: ManagementEffectHashV1::from_hex(&value.hash).unwrap(),
                    normal_mode: value.mode,
                    observed_mode: value.mode,
                    bytes: 1,
                };
                D27ManifestProjectionEntryV1 {
                    path: entry.relative_path.clone(),
                    before: entry.before.as_ref().map(version),
                    after: entry.after.as_ref().map(version),
                }
            })
            .collect(),
    };
    let request = manifest.project_request().unwrap();
    let before = ObservedSourceV1 {
        schema_version: 1,
        source_identity: source,
        entries: request
            .entries
            .iter()
            .map(|entry| ObservedEntryV1 {
                path: entry.path.clone(),
                current: entry.before.clone(),
            })
            .collect(),
    };
    let binding = D27SourceBindingV1 {
        schema_version: 1,
        payload_bytes: serde_json::to_vec(payload).unwrap().len() as u64,
        payload_hash: precommit.payload_hash,
        manifest_bytes: manifest.canonical_bytes().unwrap().len() as u64,
        manifest_hash: manifest.manifest_hash().unwrap(),
        request_bytes: request.canonical_bytes().unwrap().len() as u64,
        request_hash: request.request_hash().unwrap(),
        source_before_bytes: before.canonical_bytes().unwrap().len() as u64,
        source_before_hash: before.observation_hash().unwrap(),
        manifest,
    };
    let identity = DesktopRoot::open_owned(dir(writer).parent().unwrap())
        .unwrap()
        .capacity_identity_m2a()
        .unwrap();
    let prepare = PrepareManagementEffectV1 {
        operation_id: d27_management_operation_id(precommit.source_operation_digest).unwrap(),
        inventory: ManagementInventoryIdV1::parse("D27").unwrap(),
        scope: ManagementEffectScopeV1 {
            project: precommit.project_binding,
            session: Some(d27_session_binding(current.marker.session_id.as_str()).unwrap()),
            generation: current.marker.epoch.get(),
            revision: precommit.policy_revision,
            permission: ManagementPermissionScopeV1::Project,
            account_binding: None,
        },
        request_hash: request.request_hash().unwrap(),
        rewind: ManagementRewindDispositionV1::CapturedRestorable,
    };
    let sample = CapacityRootSampleV1 {
        identity: identity.root_identity,
        allocation_unit: identity.allocation_unit,
        available_bytes: 1024 * 1024 * 1024,
    };
    let plan = CapacityPlanV1::build(CapacityAdmissionV1 {
        roots: CapacityRootsV1 {
            state: sample,
            recovery: CapacityRootSampleV1 {
                identity: recovery,
                allocation_unit: 4096,
                available_bytes: 1024 * 1024 * 1024,
            },
            desktop: Some(sample),
        },
        request: &request,
        source_before: &before,
        d27_source_binding: Some(&binding),
        prepare: &prepare,
        management_record_limit: 16 * 1024,
        management_records_remaining: 100,
        management_bytes_remaining: 16 * 1024 * 1024,
        inventory_commit_bytes: 2 * MAX_SOURCE_APPLY_LEDGER_BYTES as u64,
        journal_bytes: 4096,
        report_bytes: 4096,
        archive_seal_bytes: 4096,
        desktop_result_required_bytes: Some(precommit.result_capacity_bytes),
        desktop_records_remaining: Some(0),
        desktop_bytes_remaining: Some(MAX_SOURCE_APPLY_LEDGER_BYTES as u64),
        archive_count: 0,
        archive_file_bytes: 0,
    })
    .unwrap();
    let key =
        d27_operation_key(precommit.project_binding, precommit.source_operation_digest).unwrap();
    let descriptor = ApplyIntentDescriptorV1 {
        schema_version: 1,
        operation_key: key,
        management_operation_id: prepare.operation_id,
        inventory_id: ApplyInventoryV1::D27,
        project_binding: precommit.project_binding,
        source_identity: source,
        agreed_revision: RevisionV1 {
            kind: RevisionKindV1::Sha256,
            value: precommit.payload_hash.as_hex(),
        },
        staged_revision: RevisionV1 {
            kind: RevisionKindV1::Sha256,
            value: request.request_hash().unwrap().as_hex(),
        },
        previous_applied_revision: None,
        context: ApplyContextV1::Session {
            base_epoch: current.marker.epoch.get(),
            session_binding: d27_session_binding(current.marker.session_id.as_str()).unwrap(),
            base_session_revision: precommit.base_session_revision,
            consume_target_revision: precommit.consume_target_revision,
            policy_revision: precommit.policy_revision,
            source_operation_digest: precommit.source_operation_digest,
            reserved_result_id: precommit.reserved_result_id.as_str().into(),
            reserved_result_id_digest: precommit.reserved_result_id_digest,
            base_marker_evidence_hash: precommit.base_marker_evidence_hash,
            consume_marker_evidence_hash: precommit.consume_marker_evidence_hash,
            consume_sidecar_offset: precommit.consume_sidecar_offset,
            consume_sidecar_hash: precommit.consume_sidecar_hash,
            precommit_hash: precommit.precommit_hash().unwrap(),
        },
        share_source_manifest_digest: binding.manifest_hash,
        changed_path_count: request.entries.len() as u64,
        path_set_digest: request.path_set_digest().unwrap(),
        workspace_apply_request_digest: request.request_hash().unwrap(),
        source_before_observation_digest: before.observation_hash().unwrap(),
        d27_source_binding_bytes: Some(binding.canonical_bytes().unwrap().len() as u64),
        d27_source_binding_hash: Some(binding.binding_hash().unwrap()),
        recovery_parent_identity: recovery,
        recovery_provenance_digest: binding.manifest.recovery_provenance_digest().unwrap(),
        recovery_child: recovery_child(key).unwrap(),
        capacity_plan_bytes: plan.canonical_bytes().unwrap().len() as u64,
        capacity_plan_hash: plan.plan_hash().unwrap(),
        capacity_plan: plan,
        phase: ApplyPhaseV1::Prepared,
    };
    binding
        .verify_artifacts(&descriptor, &request, &before)
        .unwrap();
    let mut prepare = prepare;
    prepare.scope.revision = precommit.consume_target_revision;
    prepare.request_hash = descriptor.descriptor_hash().unwrap();
    prepare.rewind = ManagementRewindDispositionV1::IrreversibleLocal;
    (descriptor, binding, request, before, prepare)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn m2a_consumed(
    capacity: u64,
) -> (
    PrototypeRoot,
    Writer,
    SourceApplyPrecommitV1,
    crate::apply_intent::ApplyIntentDescriptorV1,
) {
    let (root, mut writer) = setup();
    source_terminal(&mut writer);
    let (candidate, proof) = source_allow(&mut writer);
    let token = writer
        .precommit_source_apply_m2a(
            &target(),
            &candidate.approval_id,
            &candidate.payload_hash,
            source_guard(&writer),
            proof,
            crate::management_effect::ManagementEffectHashV1::digest(b"verified-project-binding"),
            ResultId::new("trusted-result").unwrap(),
            capacity,
        )
        .unwrap();
    let guard = source_guard(&writer);
    let consumed = writer
        .consume_source_apply_precommitted_m2a(token, guard, proof)
        .unwrap();
    let descriptor = m2a_result_descriptor(&writer, &consumed.precommit);
    (root, writer, consumed.precommit, descriptor)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_source_apply_result_joins_reserved_id_original_report_and_exact_marker_and_replay() {
    let (_root, mut writer, precommit, descriptor) = m2a_consumed(MAX_SOURCE_APPLY_BYTES as u64);
    let hash = m2a_result_hash(M2A_REPORT);
    assert_eq!(
        hash.as_hex(),
        "c3cc7890c45e7325ee19bd793a35f2c95ab492da3e1173209cb62b8336bcfc81"
    );
    let evidence = writer
        .record_source_apply_result_evidence_m2a(
            &precommit,
            &descriptor,
            precommit.reserved_result_id.clone(),
            M2A_REPORT,
            hash,
        )
        .unwrap();
    let snapshot = writer.snapshot().unwrap();
    assert_eq!(
        evidence.session_revision,
        precommit.consume_target_revision + 1
    );
    assert_eq!(evidence.report_hash, hash);
    assert_eq!(
        evidence.reserved_result_id_digest,
        precommit.reserved_result_id_digest
    );
    assert_eq!(
        evidence.marker_bytes,
        fs::read(dir(&writer).join(disk::MARKER)).unwrap()
    );
    let sidecar = fs::read(dir(&writer).join(disk::SIDECAR)).unwrap();
    assert_eq!(
        evidence.marker_evidence_hash,
        super::source_apply::marker_evidence_hash(&evidence.marker_bytes, &sidecar)
    );
    assert_eq!(
        evidence.sidecar_prefix_hash.as_hex(),
        crate::conversation_state::content_hash(&sidecar)
    );
    assert_eq!(evidence.sidecar_hash.as_hex(), snapshot.marker.sidecar_hash);
    assert_eq!(
        evidence.sidecar_offset,
        snapshot.marker.sidecar_offset.get()
    );
    let record = &snapshot.state.source_applies[0];
    assert_eq!(
        record.reserved_result_id.as_ref(),
        Some(&precommit.reserved_result_id)
    );
    assert_eq!(
        record.result.as_ref().unwrap().result_id,
        precommit.reserved_result_id
    );
    let before = m2a_session_files(&writer);
    let replay = writer
        .record_source_apply_result_evidence_m2a(
            &precommit,
            &descriptor,
            precommit.reserved_result_id.clone(),
            M2A_REPORT,
            hash,
        )
        .unwrap();
    assert_eq!(replay, evidence);
    assert_eq!(m2a_session_files(&writer), before);
    writer.set_policy_revision(DecimalU64::new(8)).unwrap();
    let before = m2a_session_files(&writer);
    assert!(
        writer
            .record_source_apply_result_evidence_m2a(
                &precommit,
                &descriptor,
                precommit.reserved_result_id.clone(),
                M2A_REPORT,
                hash
            )
            .is_err()
    );
    assert_eq!(m2a_session_files(&writer), before);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_source_apply_result_refuses_wrong_id_hash_encoding_context_and_capacity_before_writes() {
    use crate::apply_intent::ApplyContextV1;
    let result = SourceApplyResult {
        result_id: ResultId::new("trusted-result").unwrap(),
        report: serde_json::from_slice(M2A_REPORT).unwrap(),
    };
    let length = serde_json::to_vec(&result).unwrap().len() as u64;
    for capacity in [length - 1, length] {
        let (_root, mut writer, precommit, descriptor) = m2a_consumed(capacity);
        let before = m2a_session_files(&writer);
        let outcome = writer.record_source_apply_result_evidence_m2a(
            &precommit,
            &descriptor,
            precommit.reserved_result_id.clone(),
            M2A_REPORT,
            m2a_result_hash(M2A_REPORT),
        );
        if capacity == length {
            outcome.unwrap();
        } else {
            assert!(matches!(outcome, Err(StoreError::Capacity(_))));
            assert_eq!(m2a_session_files(&writer), before);
        }
    }
    let (_root, mut writer, precommit, descriptor) = m2a_consumed(MAX_SOURCE_APPLY_BYTES as u64);
    let before = m2a_session_files(&writer);
    for field in 0..6 {
        let mut d = descriptor.clone();
        let mut report = M2A_REPORT.to_vec();
        let mut hash = m2a_result_hash(&report);
        let mut id = precommit.reserved_result_id.clone();
        match field {
            0 => id = ResultId::new("different-result").unwrap(),
            1 => {
                hash = crate::management_effect::ManagementEffectHashV1::digest(b"different-report")
            }
            2 => {
                report.insert(0, b' ');
                hash = m2a_result_hash(&report);
            }
            3 => {
                if let ApplyContextV1::Session {
                    policy_revision, ..
                } = &mut d.context
                {
                    *policy_revision += 1;
                }
            }
            4 => {
                if let ApplyContextV1::Session {
                    consume_sidecar_offset,
                    ..
                } = &mut d.context
                {
                    *consume_sidecar_offset += 1;
                }
            }
            5 => {
                report = String::from_utf8(report)
                    .unwrap()
                    .replacen(
                        "\"source_path\":\"/source\"",
                        "\"source_path\":\"/different\"",
                        1,
                    )
                    .into_bytes();
                hash = m2a_result_hash(&report);
            }
            _ => unreachable!(),
        }
        assert!(
            writer
                .record_source_apply_result_evidence_m2a(&precommit, &d, id, &report, hash)
                .is_err(),
            "field {field}"
        );
        assert_eq!(m2a_session_files(&writer), before);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_source_apply_capacity_counts_exact_wrapper_escaping_and_actual_full_sidecar_rows() {
    for raw_id in [
        "trusted-result".to_owned(),
        "原文\n\\\"".to_owned(),
        "\0".repeat(128),
    ] {
        let id = ResultId::new(raw_id).unwrap();
        let result = SourceApplyResult {
            result_id: id.clone(),
            report: serde_json::from_slice(M2A_REPORT).unwrap(),
        };
        let actual = serde_json::to_vec(&result).unwrap().len() as u64;
        assert_eq!(
            source_apply_result_capacity_m2a(&id, M2A_REPORT.len() as u64).unwrap(),
            actual
        );
        assert!(matches!(
            source_apply_result_capacity_m2a(&id, u64::MAX),
            Err(StoreError::Overflow)
        ));
    }
    let (_root, mut writer) = setup();
    source_terminal(&mut writer);
    let (candidate, proof) = source_allow(&mut writer);
    let token = m2a_precommit(
        &writer,
        &candidate,
        proof,
        ResultId::new("trusted-result").unwrap(),
    )
    .unwrap();
    let inventory = token.inventory_commit_bytes().unwrap();
    let unit = DesktopRoot::open_owned(dir(&writer).parent().unwrap())
        .unwrap()
        .capacity_identity_m2a()
        .unwrap()
        .allocation_unit;
    let base_length = fs::metadata(dir(&writer).join(disk::SIDECAR))
        .unwrap()
        .len();
    let guard = source_guard(&writer);
    let consumed = writer
        .consume_source_apply_precommitted_m2a(token, guard, proof)
        .unwrap();
    let consume_length = fs::metadata(dir(&writer).join(disk::SIDECAR))
        .unwrap()
        .len()
        - base_length;
    let descriptor = m2a_result_descriptor(&writer, &consumed.precommit);
    let before_result_length = fs::metadata(dir(&writer).join(disk::SIDECAR))
        .unwrap()
        .len();
    writer
        .record_source_apply_result_evidence_m2a(
            &consumed.precommit,
            &descriptor,
            consumed.precommit.reserved_result_id.clone(),
            M2A_REPORT,
            m2a_result_hash(M2A_REPORT),
        )
        .unwrap();
    let result_row_length = fs::metadata(dir(&writer).join(disk::SIDECAR))
        .unwrap()
        .len()
        - before_result_length;
    let result_object_length = serde_json::to_vec(
        writer.snapshot().unwrap().state.source_applies[0]
            .result
            .as_ref()
            .unwrap(),
    )
    .unwrap()
    .len() as u64;
    let result_bound_row = result_row_length - result_object_length + MAX_SOURCE_APPLY_BYTES as u64;
    let allocated = |bytes: u64| bytes.div_ceil(unit) * unit;
    assert_eq!(
        inventory,
        allocated(consume_length) + allocated(result_bound_row) + 2 * allocated(65_536)
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_source_apply_capacity_full_record_table_and_ledger_plus_minus_one() {
    for delta in [-1_i64, 0, 1] {
        let (_root, mut writer) = setup();
        source_terminal(&mut writer);
        let (candidate, proof) = source_allow(&mut writer);
        let mut next = writer.snapshot().unwrap();
        let future_revision = next.marker.session_revision.checked_add(2).unwrap();
        let reserved = ResultId::new("trusted-result").unwrap();
        for index in 1..MAX_SOURCE_APPLY_RECORDS {
            let mut record = next.state.source_applies[0].clone();
            record.candidate.approval_id =
                ApprovalId::new(format!("filler-approval-{index}")).unwrap();
            record.candidate.operation_id =
                OperationId::new(format!("filler-operation-{index}")).unwrap();
            record.decision = None;
            record.candidate.payload.entries.clear();
            record.candidate.payload.source_path = format!("/s{}", "a".repeat(2000));
            record.candidate.payload.recovery_parent_path = format!("/r{}", "b".repeat(2000));
            record.candidate.payload_hash = record.candidate.payload.payload_hash().unwrap();
            next.state.source_applies.push(record);
        }
        let mut consumed_rows = next.state.source_applies.clone();
        consumed_rows[0].intent_revision = Some(future_revision);
        consumed_rows[0].reserved_result_id = Some(reserved.clone());
        let desired = (MAX_SOURCE_APPLY_LEDGER_BYTES - MAX_SOURCE_APPLY_BYTES) as i64 + delta;
        let mut gap = desired as usize - serde_json::to_vec(&consumed_rows).unwrap().len();
        for index in 1..MAX_SOURCE_APPLY_RECORDS {
            for recovery in [false, true] {
                let payload = &mut next.state.source_applies[index].candidate.payload;
                let text = if recovery {
                    &mut payload.recovery_parent_path
                } else {
                    &mut payload.source_path
                };
                let count = gap.min(4096 - text.len());
                text.push_str(&"x".repeat(count));
                gap -= count;
            }
            let record = &mut next.state.source_applies[index];
            record.candidate.payload_hash = record.candidate.payload.payload_hash().unwrap();
            consumed_rows[index] = record.clone();
        }
        assert_eq!(gap, 0);
        assert_eq!(
            serde_json::to_vec(&consumed_rows).unwrap().len(),
            desired as usize
        );
        writer.commit(next, false).unwrap();
        let before = m2a_session_files(&writer);
        let outcome = m2a_precommit(&writer, &candidate, proof, reserved);
        if delta == 1 {
            assert!(
                matches!(outcome, Err(StoreError::Capacity(_))),
                "ledger overflow before consume"
            );
        } else {
            let token = outcome.unwrap();
            assert_eq!(
                token.logical_capacity().records_remaining,
                0,
                "existing record needs no new slot"
            );
            let guard = source_guard(&writer);
            writer
                .consume_source_apply_precommitted_m2a(token, guard, proof)
                .unwrap();
            assert_eq!(
                writer.snapshot().unwrap().state.source_applies.len(),
                MAX_SOURCE_APPLY_RECORDS
            );
            continue;
        }
        assert_eq!(m2a_session_files(&writer), before);
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_source_apply_result_id_rejects_each_existing_namespace_and_reservation_retrofit() {
    let (_root, mut writer) = setup();
    writer
        .apply(&start("source-start", 0), Some(target()))
        .unwrap();
    writer.record_intent(&target(), op()).unwrap();
    writer
        .record_operation_result(&target(), &op(), ResultId::new("operation-only").unwrap())
        .unwrap();
    writer
        .finish(
            &target(),
            Observation::Succeeded,
            ResultId::new("run-only").unwrap(),
        )
        .unwrap();
    let (candidate, proof) = source_allow(&mut writer);
    let before = m2a_session_files(&writer);
    for id in ["operation-only", "run-only"] {
        assert!(
            m2a_precommit(&writer, &candidate, proof, ResultId::new(id).unwrap()).is_err(),
            "{id}"
        );
        assert_eq!(m2a_session_files(&writer), before);
    }
    let guard = source_guard(&writer);
    writer
        .consume_source_apply_intent(
            &target(),
            &candidate.approval_id,
            &candidate.payload_hash,
            guard,
            proof,
        )
        .unwrap();
    let mut next = writer.snapshot().unwrap();
    next.state.source_applies[0].reserved_result_id = Some(ResultId::new("retrofit").unwrap());
    let before = m2a_session_files(&writer);
    assert!(
        writer.commit(next, false).is_err(),
        "legacy consume cannot be rebound"
    );
    assert_eq!(m2a_session_files(&writer), before);
    let mut legacy_result = source_report();
    legacy_result.result_id = ResultId::new("legacy-result-only").unwrap();
    let revision = writer.snapshot().unwrap().marker.session_revision;
    writer
        .record_source_apply_result(&target(), &candidate.operation_id, revision, legacy_result)
        .unwrap();
    let mut second = candidate.clone();
    second.approval_id = ApprovalId::new("second-approval").unwrap();
    second.operation_id = OperationId::new("second-operation").unwrap();
    let guard = source_guard(&writer);
    writer
        .publish_source_apply(&target(), second.clone(), guard, proof)
        .unwrap();
    let guard = source_guard(&writer);
    writer
        .resolve_source_apply(
            &target(),
            &second.approval_id,
            ApprovalDecision::Allow,
            guard,
        )
        .unwrap();
    let before = m2a_session_files(&writer);
    assert!(
        m2a_precommit(
            &writer,
            &second,
            proof,
            ResultId::new("legacy-result-only").unwrap()
        )
        .is_err()
    );
    assert_eq!(m2a_session_files(&writer), before);

    let (_root2, mut writer2) = setup();
    source_terminal(&mut writer2);
    let (first, proof2) = source_allow(&mut writer2);
    let token = m2a_precommit(
        &writer2,
        &first,
        proof2,
        ResultId::new("reservation-only").unwrap(),
    )
    .unwrap();
    let guard = source_guard(&writer2);
    writer2
        .consume_source_apply_precommitted_m2a(token, guard, proof2)
        .unwrap();
    let mut next = writer2.snapshot().unwrap();
    next.state.source_applies[0].reserved_result_id =
        Some(ResultId::new("changed-reservation").unwrap());
    let before = m2a_session_files(&writer2);
    assert!(
        writer2.commit(next, false).is_err(),
        "durable reservation is immutable"
    );
    assert_eq!(m2a_session_files(&writer2), before);
    let mut second = first.clone();
    second.approval_id = ApprovalId::new("other-approval").unwrap();
    second.operation_id = OperationId::new("other-operation").unwrap();
    let guard = source_guard(&writer2);
    writer2
        .publish_source_apply(&target(), second.clone(), guard, proof2)
        .unwrap();
    let guard = source_guard(&writer2);
    writer2
        .resolve_source_apply(
            &target(),
            &second.approval_id,
            ApprovalDecision::Allow,
            guard,
        )
        .unwrap();
    let before = m2a_session_files(&writer2);
    assert!(
        m2a_precommit(
            &writer2,
            &second,
            proof2,
            ResultId::new("reservation-only").unwrap()
        )
        .is_err()
    );
    assert_eq!(m2a_session_files(&writer2), before);
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_source_apply_full_result_one_mib_accepts_and_plus_one_refuses_before_publication() {
    for extra in [0, 1] {
        let (_root, mut writer, precommit, descriptor) =
            m2a_consumed(MAX_SOURCE_APPLY_BYTES as u64);
        let mut report: crate::workspace_apply::ApplyReport =
            serde_json::from_slice(M2A_REPORT).unwrap();
        let original = SourceApplyResult {
            result_id: precommit.reserved_result_id.clone(),
            report: serde_json::to_value(&report).unwrap(),
        };
        let length = serde_json::to_vec(&original).unwrap().len();
        // The ledger boundary is tested with a legal variable-length typed
        // report field. This is not a claim of a real source apply or its shape.
        report.entries[0]
            .old_name
            .push_str(&"x".repeat(MAX_SOURCE_APPLY_BYTES - length + extra));
        let bytes = report.canonical_bytes().unwrap();
        let result = SourceApplyResult {
            result_id: precommit.reserved_result_id.clone(),
            report: serde_json::to_value(&report).unwrap(),
        };
        assert_eq!(
            serde_json::to_vec(&result).unwrap().len(),
            MAX_SOURCE_APPLY_BYTES + extra
        );
        let before = m2a_session_files(&writer);
        let outcome = writer.record_source_apply_result_evidence_m2a(
            &precommit,
            &descriptor,
            precommit.reserved_result_id.clone(),
            &bytes,
            m2a_result_hash(&bytes),
        );
        if extra == 0 {
            outcome.unwrap();
            assert_eq!(
                serde_json::to_vec(
                    writer.snapshot().unwrap().state.source_applies[0]
                        .result
                        .as_ref()
                        .unwrap()
                )
                .unwrap()
                .len(),
                MAX_SOURCE_APPLY_BYTES
            );
        } else {
            assert!(matches!(outcome, Err(StoreError::Capacity(_))));
            assert_eq!(m2a_session_files(&writer), before);
        }
    }
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_source_apply_precommit_is_readonly_and_consume_matches_exact_prediction() {
    let (_root, mut writer) = setup();
    source_terminal(&mut writer);
    let (candidate, proof) = source_allow(&mut writer);
    let before = m2a_session_files(&writer);
    let reserved = ResultId::new("trusted-exact-result-原文").unwrap();
    let token = m2a_precommit(&writer, &candidate, proof, reserved.clone()).unwrap();
    assert_eq!(m2a_session_files(&writer), before);
    let expected = token.precommit().clone();
    let base = writer.snapshot().unwrap();
    assert_eq!(
        expected.base_session_revision,
        base.marker.session_revision.get()
    );
    assert_eq!(
        expected.consume_target_revision,
        expected.base_session_revision + 1
    );
    assert_eq!(expected.reserved_result_id, reserved);
    let guard = source_guard(&writer);
    let receipt = writer
        .consume_source_apply_precommitted_m2a(token, guard, proof)
        .unwrap();
    assert_eq!(receipt.precommit, expected);
    let after = writer.snapshot().unwrap();
    let record = after
        .state
        .source_applies
        .iter()
        .find(|r| r.candidate.operation_id == candidate.operation_id)
        .unwrap();
    assert_eq!(
        record.intent_revision.unwrap().get(),
        expected.consume_target_revision
    );
    assert_eq!(record.reserved_result_id.as_ref(), Some(&reserved));
    let mut bytes = b"polaris-d27-consume-record-v1\0".to_vec();
    bytes.extend(serde_json::to_vec(record).unwrap());
    assert_eq!(
        crate::management_effect::ManagementEffectHashV1::digest(&bytes),
        expected.consume_record_hash
    );
    assert_eq!(
        after.marker.sidecar_offset.get(),
        expected.consume_sidecar_offset
    );
    assert_eq!(
        after.marker.sidecar_hash,
        expected.consume_sidecar_hash.as_hex()
    );
    let sidecar = fs::read(dir(&writer).join(disk::SIDECAR)).unwrap();
    assert_eq!(
        super::source_apply::marker_evidence_hash(&receipt.marker_bytes, &sidecar),
        expected.consume_marker_evidence_hash
    );
    assert_eq!(
        fs::read(dir(&writer).join(disk::MARKER)).unwrap(),
        receipt.marker_bytes
    );
    assert!(!dir(&writer).join("state.next").exists());
    assert!(
        m2a_precommit(&writer, &candidate, proof, reserved).is_err(),
        "cannot bind a second consume"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn m2a_source_apply_precommit_rejects_collisions_stale_and_foreign_entries_without_mutation() {
    let (_root, mut writer) = setup();
    source_terminal(&mut writer);
    let (candidate, proof) = source_allow(&mut writer);
    let before = m2a_session_files(&writer);
    assert!(
        m2a_precommit(&writer, &candidate, proof, result()).is_err(),
        "existing result namespace"
    );
    let mut wrong = proof;
    wrong.source.inode = DecimalU64::new(99);
    assert!(
        m2a_precommit(
            &writer,
            &candidate,
            wrong,
            ResultId::new("fresh-result").unwrap()
        )
        .is_err()
    );
    assert_eq!(m2a_session_files(&writer), before);
    let token = m2a_precommit(
        &writer,
        &candidate,
        proof,
        ResultId::new("fresh-result").unwrap(),
    )
    .unwrap();
    let guard = source_guard(&writer);
    fs::write(dir(&writer).join("state.next"), b"foreign").unwrap();
    let foreign = m2a_session_files(&writer);
    assert!(
        writer
            .consume_source_apply_precommitted_m2a(token, guard, proof)
            .is_err()
    );
    assert_eq!(m2a_session_files(&writer), foreign);
    assert!(
        m2a_precommit(
            &writer,
            &candidate,
            proof,
            ResultId::new("fresh-result").unwrap()
        )
        .is_err()
    );
    assert_eq!(m2a_session_files(&writer), foreign);
    fs::remove_file(dir(&writer).join("state.next")).unwrap();
    let token = m2a_precommit(
        &writer,
        &candidate,
        proof,
        ResultId::new("fresh-result").unwrap(),
    )
    .unwrap();
    writer.set_policy_revision(DecimalU64::new(8)).unwrap();
    let changed = m2a_session_files(&writer);
    assert!(
        writer
            .consume_source_apply_precommitted_m2a(token, guard, proof)
            .is_err()
    );
    assert_eq!(m2a_session_files(&writer), changed);
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
    old.as_object_mut()
        .unwrap()
        .remove("desktop_sidecar_schema_version");
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
            reserved_result_id: None,
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

#[test]
fn the_acceptance_that_saves_a_run_saves_the_registration_it_is_bound_to() {
    let (_root, mut writer) = setup();
    let account = SavedAccount::new("reg-a".into(), 3).unwrap();
    writer
        .apply_bound(&start("start", 0), Some(target()), Some(account.clone()))
        .unwrap();
    let saved = writer.snapshot().unwrap();
    assert_eq!(saved.state.runs[0].account, Some(account));
    // Reloading keeps the binding and rejects a record that lost its generation.
    let reloaded: Sidecar =
        serde_json::from_str(&serde_json::to_string(&saved.state).unwrap()).unwrap();
    assert_eq!(
        reloaded.runs[0].account.as_ref().unwrap().registration_id,
        "reg-a"
    );
    assert!(SavedAccount::new("reg-a".into(), 0).is_err());
    assert!(SavedAccount::new("reg a".into(), 1).is_err());
}

#[test]
fn a_run_accepted_without_a_binding_stays_on_the_single_authentication() {
    let (_root, mut writer) = setup();
    writer.apply(&start("start", 0), Some(target())).unwrap();
    assert_eq!(writer.snapshot().unwrap().state.runs[0].account, None);
    // An invalid binding is refused before anything is written.
    let mut writer = setup().1;
    let invalid = SavedAccount {
        registration_id: String::new(),
        generation: DecimalU64::new(1),
    };
    assert!(
        writer
            .apply_bound(&start("start", 0), Some(target()), Some(invalid))
            .is_err()
    );
    assert!(writer.snapshot().unwrap().state.runs.is_empty());
}

#[test]
fn the_state_window_answers_from_the_published_state_and_follows_with_differences() {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };
    let (_root, mut writer) = setup();
    let (port, mut owner) = crate::desktop_workflow::DesktopWorkflow::pair(target());
    let mut context = Context::from_waker(Waker::noop());
    let mut first = Box::pin(port.window(None));
    assert!(matches!(first.as_mut().poll(&mut context), Poll::Pending));
    owner.poll(Some(&mut writer), false, None);
    let Poll::Ready(Ok(crate::desktop_store::hub::Delta::Resync(view))) =
        first.as_mut().poll(&mut context)
    else {
        panic!("an unknown base resynchronizes");
    };
    assert!(view.runs.is_empty());

    writer.apply(&start("window", 0), Some(target())).unwrap();
    let mut second = Box::pin(port.window(Some(view.revision)));
    assert!(matches!(second.as_mut().poll(&mut context), Poll::Pending));
    owner.poll(Some(&mut writer), false, None);
    let Poll::Ready(Ok(crate::desktop_store::hub::Delta::Changes(changes))) =
        second.as_mut().poll(&mut context)
    else {
        panic!("a matching base produces the difference only");
    };
    assert_eq!(changes.base, view.revision);
    assert_eq!(changes.runs.changed.len(), 1);
    assert!(changes.tasks.is_empty());

    // Reading is not writing: a cancelled run still sees where things stand.
    let mut third = Box::pin(port.window(Some(changes.revision)));
    assert!(matches!(third.as_mut().poll(&mut context), Poll::Pending));
    owner.poll(Some(&mut writer), true, None);
    let Poll::Ready(Ok(crate::desktop_store::hub::Delta::Changes(changes))) =
        third.as_mut().poll(&mut context)
    else {
        panic!("cancellation does not refuse a read");
    };
    assert!(changes.is_empty());
}

#[test]
fn a_goal_survives_a_stop_and_an_instruction_is_durable_before_it_is_applied() {
    let (_root, mut writer) = setup();
    assert_eq!(writer.set_goal("v1.0.0まで進める".into()).unwrap(), 1);
    assert!(writer.set_goal("   ".into()).is_err());
    let goal = writer.snapshot().unwrap().state.goal.unwrap();
    assert!(goal.active && !goal.stopped);

    // An explicit stop stays through a later goal change.
    assert_eq!(writer.update_goal(true, true).unwrap(), 2);
    assert_eq!(writer.set_goal("別の目標".into()).unwrap(), 3);
    let goal = writer.snapshot().unwrap().state.goal.unwrap();
    assert!(goal.stopped, "a new goal must not silently resume");
    assert_eq!(writer.update_goal(true, false).unwrap(), 4);
    assert!(!writer.snapshot().unwrap().state.goal.unwrap().stopped);

    writer
        .receive_instruction("i1".into(), "先にtestを直す".into())
        .unwrap();
    // The same instruction never lands twice, so a resend cannot double-apply.
    writer
        .receive_instruction("i1".into(), "先にtestを直す".into())
        .unwrap();
    let saved = writer.snapshot().unwrap();
    assert_eq!(saved.state.instructions.len(), 1);
    assert!(saved.state.instructions[0].applied_to.is_none());

    // The window shows a received instruction until a run takes it in.
    let pending = crate::desktop_store::hub::view(&writer.snapshot().unwrap().state, 1, None);
    assert_eq!(pending.instructions.len(), 1);
    assert_eq!(pending.goal.as_ref().map(|goal| goal.revision), Some(4));

    writer.apply(&start("goal-run", 0), Some(target())).unwrap();
    writer
        .apply_instructions(&target(), &["i1".into()])
        .unwrap();
    let after = crate::desktop_store::hub::view(&writer.snapshot().unwrap().state, 2, None);
    assert!(after.instructions.is_empty());
    let applied = writer.snapshot().unwrap().state.instructions[0]
        .applied_to
        .clone()
        .expect("applied");
    assert_eq!(applied.run_id, target().run_id);
    // Applying the same instruction again is a conflict, not a silent repeat.
    assert!(
        writer
            .apply_instructions(&target(), &["i1".into()])
            .is_err()
    );
    assert!(
        writer
            .apply_instructions(&target(), &["i2".into()])
            .is_err()
    );
}

#[test]
fn instructions_are_handed_to_the_run_once_and_stay_recorded_against_it() {
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };
    let (_root, mut writer) = setup();
    writer.apply(&start("first", 0), Some(target())).unwrap();
    writer
        .receive_instruction("i1".into(), "先にtestを直す".into())
        .unwrap();
    let (port, mut owner) = crate::desktop_workflow::DesktopWorkflow::pair(target());
    let mut context = Context::from_waker(Waker::noop());

    let mut take = Box::pin(port.take_instructions());
    assert!(matches!(take.as_mut().poll(&mut context), Poll::Pending));
    owner.poll(Some(&mut writer), false, None);
    let Poll::Ready(Ok(taken)) = take.as_mut().poll(&mut context) else {
        panic!("the instruction is handed over");
    };
    assert_eq!(taken, vec!["先にtestを直す".to_string()]);

    // The same attempt never receives it twice.
    let mut again = Box::pin(port.take_instructions());
    assert!(matches!(again.as_mut().poll(&mut context), Poll::Pending));
    owner.poll(Some(&mut writer), false, None);
    assert!(matches!(
        again.as_mut().poll(&mut context),
        Poll::Ready(Ok(taken)) if taken.is_empty()
    ));

    // It stays recorded against the run that took it, with its text.
    let saved = writer.snapshot().unwrap();
    assert_eq!(
        saved.state.instructions[0]
            .applied_to
            .as_ref()
            .map(|applied| applied.attempt_id.as_str()),
        Some(target().attempt_id.as_str())
    );
    assert!(
        crate::desktop_store::hub::view(&saved.state, 1, None)
            .instructions
            .is_empty()
    );
}

#[test]
fn an_applied_instruction_makes_room_so_a_long_session_keeps_accepting_them() {
    use crate::desktop_store::MAX_INSTRUCTIONS;
    let (_root, mut writer) = setup();
    writer.apply(&start("run", 0), Some(target())).unwrap();
    for n in 0..MAX_INSTRUCTIONS {
        writer
            .receive_instruction(format!("i{n}"), format!("指示{n}"))
            .unwrap();
    }
    // Every instruction is still waiting, so the bound refuses one more.
    assert!(
        writer
            .receive_instruction("over".into(), "あふれる".into())
            .is_err()
    );
    writer
        .apply_instructions(&target(), &["i0".into()])
        .unwrap();
    writer
        .receive_instruction("after".into(), "続きの指示".into())
        .unwrap();
    let held = writer.snapshot().unwrap().state.instructions;
    assert_eq!(held.len(), MAX_INSTRUCTIONS);
    assert!(held.iter().all(|held| held.instruction_id != "i0"));
    assert_eq!(held.last().unwrap().instruction_id, "after");
}
