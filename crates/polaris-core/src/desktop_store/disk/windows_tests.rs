//! Windows desktop store directory, locking and generation persistence tests.
use super::*;
use polaris_auth::protection::{WindowsPinnedDirectory, WindowsPrivateRoot};
use std::{
    ffi::OsStr,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

fn private(name: &str) -> (PathBuf, Directory) {
    let base = PathBuf::from(
        std::env::var_os("POLARIS_WINDOWS_STORAGE_TEST_ROOT")
            .expect("native acceptance requires the explicit disposable root"),
    );
    WindowsPinnedDirectory::open_existing(&base).unwrap();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = base.join(format!("r3-{name}-{}-{nonce}", std::process::id()));
    WindowsPrivateRoot::create_new(&path).unwrap();
    let directory = Directory::open_owned(&path).unwrap();
    (path, directory)
}
fn write(dir: &Directory, name: &str, bytes: &[u8]) {
    let mut file = dir.open(name, true, true).unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}
fn read(dir: &Directory, name: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    dir.open(name, false, false)
        .unwrap()
        .read_to_end(&mut bytes)
        .unwrap();
    bytes
}

#[test]
fn windows_family_native_file_clone_cursor_flush_reopen() {
    let (path, root) = private("file");
    let mut file = root.open("payload", true, true).unwrap();
    drop(root);
    assert!(fs::rename(&path, path.with_extension("changed")).is_err());
    file.write_all(b"abcdef").unwrap();
    file.seek(SeekFrom::Start(1)).unwrap();
    let mut clone = file.try_clone().unwrap();
    let mut byte = [0];
    clone.read_exact(&mut byte).unwrap();
    assert_eq!(&byte, b"b");
    assert_eq!(file.stream_position().unwrap(), 2);
    file.set_len(4).unwrap();
    file.sync_all().unwrap();
    assert_eq!(clone.metadata().unwrap().len(), 4);
    drop(file);
    drop(clone);
    let reopened = Directory::open_owned(&path).unwrap();
    assert_eq!(read(&reopened, "payload"), b"abcd");
    reopened
        .open("payload", false, false)
        .unwrap()
        .sync_all()
        .unwrap();
    reopened.sync().unwrap();
}

#[test]
fn windows_family_native_locks_contend_and_release() {
    let (_, root) = private("locks");
    let a = root.open("owner.lock", true, true).unwrap();
    let b = root.open("owner.lock", true, false).unwrap();
    a.try_lock().unwrap();
    assert!(matches!(
        b.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    assert!(matches!(
        b.try_lock_shared(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    a.unlock().unwrap();
    a.try_lock_shared().unwrap();
    b.try_lock_shared().unwrap();
    a.unlock().unwrap();
    b.unlock().unwrap();
    b.try_lock().unwrap();
    drop(b);
    a.try_lock().unwrap();
    a.unlock().unwrap();
}

#[test]
fn windows_family_native_hard_link_collision_and_expected_cleanup() {
    let (_, root) = private("hard-link");
    write(&root, "stage", b"first");
    assert!(root.link_no_replace("stage", "object").unwrap());
    // The native API must leave two names until the explicit staging cleanup.
    let native = WindowsPrivateRoot::open_existing(root.path()).unwrap();
    assert_eq!(
        native.open_file(OsStr::new("stage")).unwrap().identity(),
        native.open_file(OsStr::new("object")).unwrap().identity()
    );
    root.unlink_file("stage").unwrap();
    assert_eq!(read(&root, "object"), b"first");
    write(&root, "collision", b"second");
    assert!(!root.link_no_replace("collision", "object").unwrap());
    assert_eq!(read(&root, "collision"), b"second");
    assert_eq!(read(&root, "object"), b"first");
    write(&root, "stage-é-🙂", b"utf16-names");
    assert!(root.link_no_replace("stage-é-🙂", "object-é-🙂").unwrap());
    root.unlink_file("stage-é-🙂").unwrap();
    assert_eq!(read(&root, "object-é-🙂"), b"utf16-names");
    root.sync().unwrap();
}

#[test]
fn windows_family_native_current_replacement_and_readback() {
    let (_, root) = private("replace");
    write(&root, "CURRENT", b"old");
    write(&root, "next", b"new");
    let ordinary = root.open("CURRENT", false, false).unwrap();
    assert!(
        matches!(root.replace("next", "CURRENT"), Err(StoreError::Io(error)) if error.raw_os_error() == Some(32))
    );
    assert_eq!(read(&root, "CURRENT"), b"old");
    assert_eq!(read(&root, "next"), b"new");
    drop(ordinary);
    let mut old = root.open_mutable("CURRENT").unwrap();
    let old_id = old.identity().unwrap();
    root.replace("next", "CURRENT").unwrap();
    assert_ne!(
        old_id,
        root.open("CURRENT", false, false)
            .unwrap()
            .identity()
            .unwrap()
    );
    assert_eq!(read(&root, "CURRENT"), b"new");
    let mut old_bytes = Vec::new();
    old.read_to_end(&mut old_bytes).unwrap();
    assert_eq!(old_bytes, b"old");
    assert!(
        matches!(root.open("next", false, false), Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound)
    );
    root.sync().unwrap();
}

#[test]
fn windows_family_native_census_retains_sorted_children_and_bounds() {
    let (_, root) = private("census");
    let z = root.ensure_child("z").unwrap();
    let a = root.ensure_child("a").unwrap();
    write(&root, "file", b"data");
    let children = root.family_children(3).unwrap();
    assert_eq!(
        children
            .iter()
            .map(|child| child.path().file_name().unwrap().to_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["a", "z"]
    );
    assert_eq!(children[0].file.identity(), a.file.identity());
    assert_eq!(children[1].file.identity(), z.file.identity());
    assert!(root.family_children(2).is_err());
    assert!(fs::rename(a.path(), root.path().join("changed")).is_err());
    children[0].verify().unwrap();
}

#[test]
fn windows_family_native_foreign_entries_refuse_without_mutation() {
    let (path, root) = private("foreign");
    fs::create_dir(path.join("foreign")).unwrap();
    fs::write(path.join("foreign").join("evidence"), b"keep").unwrap();
    assert!(root.child("foreign", false).is_err());
    assert!(root.ensure_child("foreign").is_err());
    assert!(root.family_children(4096).is_err());
    fs::write(path.join("foreign-file"), b"untouched").unwrap();
    write(&root, "stage", b"source");
    assert!(root.replace("stage", "foreign-file").is_err());
    assert!(root.unlink_file("foreign-file").is_err());
    assert_eq!(read(&root, "stage"), b"source");
    assert_eq!(fs::read(path.join("foreign-file")).unwrap(), b"untouched");
    assert_eq!(fs::read(path.join("foreign/evidence")).unwrap(), b"keep");
}

#[test]
fn windows_family_native_foreign_hard_link_preserves_both_names() {
    let (path, root) = private("outside-link");
    write(&root, "payload", b"keep-both");
    let outside = path.with_extension("outside-link");
    fs::hard_link(path.join("payload"), &outside).unwrap();
    assert!(root.open("payload", false, false).is_err());
    assert!(root.unlink_file("payload").is_err());
    assert_eq!(fs::read(path.join("payload")).unwrap(), b"keep-both");
    assert_eq!(fs::read(outside).unwrap(), b"keep-both");
}

#[test]
fn windows_family_native_reparse_fixture_refuses_before_writes() {
    let path = PathBuf::from(std::env::var_os("POLARIS_WINDOWS_LINK_FIXTURE").unwrap());
    let before = fs::read(path.join("normal/payload")).unwrap();
    assert!(Directory::open_owned(&path.join("alias")).is_err());
    assert_eq!(fs::read(path.join("normal/payload")).unwrap(), before);
    let (_, root) = private("component-denials");
    write(&root, "payload", b"retained");
    for name in ["..", "a/b", "a\\b", "x:stream", "NUL", "tail."] {
        assert!(root.open(name, true, true).is_err());
    }
    assert_eq!(read(&root, "payload"), b"retained");
}

#[test]
fn windows_family_native_common_manager_index_reopen_and_orphan_repair() {
    use crate::desktop_store::SessionManagerStore;
    use crate::session_manager::SessionId;
    let (path, root) = private("common-family");
    let first = SessionId::new("first").unwrap();
    let child = SessionId::new("child").unwrap();
    let (store, state) = SessionManagerStore::open_family_owned(&path, &first).unwrap();
    assert!(state.sessions.contains_key(&first));
    assert!(matches!(
        SessionManagerStore::open_family_owned(&path, &first),
        Err(StoreError::Busy)
    ));
    drop(store);
    assert_eq!(
        SessionManagerStore::inspect_family_owned(&path, &first).unwrap(),
        Some(state.clone())
    );
    SessionManagerStore::create_family_owned(&path, &child, |_| Ok(())).unwrap();
    let (reopened, child_state) = SessionManagerStore::open_family_owned(&path, &child).unwrap();
    assert!(child_state.sessions.contains_key(&child));
    drop(reopened);
    let orphan = SessionId::new("orphan").unwrap();
    root.ensure_child(orphan.as_str()).unwrap();
    assert!(SessionManagerStore::has_child_artifacts(&path, &orphan).unwrap());
    SessionManagerStore::repair_published_child(&path, &orphan).unwrap();
    let (repaired, repaired_state) =
        SessionManagerStore::open_family_owned(&path, &orphan).unwrap();
    assert!(repaired_state.sessions.contains_key(&orphan));
    drop(repaired);
    let (reopened, recovered) = SessionManagerStore::open_family_owned(&path, &first).unwrap();
    assert_eq!(recovered, state);
    drop(reopened);
    assert!(read(&root, "family-index.v1.json").starts_with(b"{\"version\":1"));
}

#[test]
fn windows_family_native_unsupported_entries_refuse_before_creation() {
    use crate::desktop_store::{
        DesktopRoot, SubagentControlBindingV1, SubagentControlOwner, WorkflowArtifactStore,
    };
    use crate::subagent_control::{RootAttemptId, RootRunId, TaskId};
    let (path, root) = private("unsupported");
    let project = ProjectId::new("r3-refusal").unwrap();
    assert!(matches!(
        DesktopRoot::open_owned(&path),
        Err(StoreError::UnsupportedPlatform)
    ));
    assert!(matches!(
        WorkflowArtifactStore::open_owned(&path, &project),
        Err(StoreError::UnsupportedPlatform)
    ));
    let binding = SubagentControlBindingV1 {
        project_id: project,
        session_id: SessionId::new("r3-session").unwrap(),
        root_run_id: RootRunId::new("r3-run").unwrap(),
        root_attempt_id: RootAttemptId::new("r3-attempt").unwrap(),
        active_task_id: TaskId::new("r3-task").unwrap(),
    };
    assert!(matches!(
        SubagentControlOwner::open_or_create_owned(&path, binding),
        Err(StoreError::UnsupportedPlatform)
    ));
    assert!(root.family_children(0).unwrap().is_empty());
    assert_eq!(fs::read_dir(&path).unwrap().count(), 0);
}

#[test]
fn windows_family_generation_real_owner_capacity_orphan_current_reopen() {
    use crate::desktop_store::SessionManagerStore;
    use crate::session_manager::{
        AdmissionState, DestinationMode, SessionId, SubmissionPayloadV1, SubmitOutcomeV1,
        SubmitRequestV1,
    };
    use std::collections::BTreeMap;
    let (path, root) = private("manager-generation");
    let current = SessionId::new("generation-owner").unwrap();
    let (store, mut state) = SessionManagerStore::open_family_owned(&path, &current).unwrap();
    let before = state.revision;
    let SubmitOutcomeV1::Accepted { admission, .. } = state
        .submit(
            SubmitRequestV1 {
                parent_session: current.clone(),
                payload: SubmissionPayloadV1 {
                    text: "reserved-before-natural-rescue".into(),
                    attachment_ids: Vec::new(),
                    route: DestinationMode::Current,
                    expected_session_revision: 0,
                    expected_goal_revision: 0,
                },
                admission_id: None,
            },
            before,
        )
        .unwrap();
    store
        .persist_submission(before, &state, &BTreeMap::from([(current.clone(), 0)]))
        .unwrap();

    // Fill the real owner journal using valid public transitions. No journal
    // bytes, state objects, capacity constants or owner algorithms are replaced.
    let started = std::time::Instant::now();
    let mut capacity_denied = false;
    for observed in 1..=crate::desktop_store::session_manager::MAX_SESSION_MANAGER_COMMITS as u64 {
        assert!(
            started.elapsed().as_secs() < 240,
            "real manager capacity exercise exceeded its bounded runtime; retain this fixture for diagnosis"
        );
        let mut next = state.clone();
        next.observe_session_revisions(&current, state.revision, observed, 0)
            .unwrap();
        match store.persist_observed(state.revision, &next, &current, observed, 0) {
            Ok(()) => state = next,
            Err(StoreError::Capacity(_)) => {
                capacity_denied = true;
                break;
            }
            Err(error) => panic!("real manager owner transition failed: {error}"),
        }
    }
    assert!(capacity_denied, "actual durable capacity was not reached");
    let family = root.child(current.as_str(), false).unwrap();
    let manager = family.child("session-manager-v1", false).unwrap();
    let legacy_journal = read(&manager, "commits.jsonl");
    let legacy_state_name = format!("state-{}.json", state.revision);
    let legacy_state = read(&manager, &legacy_state_name);
    let count = legacy_journal
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .count();
    assert!(count > 1);
    assert_eq!(store.load().unwrap(), Some(state.clone()));
    drop(store);

    // An interrupted generation directory is a real private orphan fixture,
    // not a fabricated manager journal or evidence of process termination.
    let generations = manager.ensure_child("generations").unwrap();
    let orphan = generations.child("1", true).unwrap();
    write(&orphan, "sentinel", b"retained-orphan-generation");
    orphan.sync().unwrap();
    drop(orphan);
    drop(generations);

    let (rescued_store, rescued) = SessionManagerStore::open_family_owned(&path, &current).unwrap();
    let mut expected = state;
    expected.generation = 1;
    expected.new_submit_closed = true;
    assert_eq!(rescued, expected);
    assert!(matches!(
        rescued.status(&admission.id).unwrap().state,
        AdmissionState::Reserved
    ));
    assert_eq!(read(&manager, "commits.jsonl"), legacy_journal);
    assert_eq!(read(&manager, &legacy_state_name), legacy_state);

    let pointer_bytes = read(&manager, "CURRENT");
    let pointer: serde_json::Value = serde_json::from_slice(&pointer_bytes).unwrap();
    assert_eq!(pointer["version"], 2);
    assert_eq!(pointer["generation"], 1);
    let generations = manager.child("generations", false).unwrap();
    let active = generations.child("1", false).unwrap();
    let manifest_bytes = read(&active, "manifest.json");
    let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
    assert_eq!(pointer["manifest_hash"], content_hash(&manifest_bytes));
    assert_eq!(
        manifest["previous_journal_hash"],
        content_hash(&legacy_journal)
    );
    assert_eq!(manifest["previous_commit_count"], count);
    let children = generations.family_children(4).unwrap();
    let retained = children
        .iter()
        .filter(|child| {
            child
                .path()
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with(".orphan-1-")
        })
        .collect::<Vec<_>>();
    assert_eq!(retained.len(), 1);
    assert_eq!(read(retained[0], "sentinel"), b"retained-orphan-generation");
    let current_id = manager
        .open("CURRENT", false, false)
        .unwrap()
        .identity()
        .unwrap();
    drop(rescued_store);
    let (reopened, recovered) = SessionManagerStore::open_family_owned(&path, &current).unwrap();
    assert_eq!(recovered, expected);
    assert_eq!(read(&manager, "CURRENT"), pointer_bytes);
    assert_eq!(
        manager
            .open("CURRENT", false, false)
            .unwrap()
            .identity()
            .unwrap(),
        current_id
    );
    assert_eq!(read(&manager, "commits.jsonl"), legacy_journal);
    drop(reopened);
}
