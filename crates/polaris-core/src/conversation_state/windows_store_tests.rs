//! Windows conversation storage, publication, locking and refusal regression tests.
use super::*;
use std::cell::Cell;

const ID: &str = "00000000-0000-4000-8000-000000000001";

fn state() -> ConversationStateV2 {
    ConversationStateV2::new("project".into(), ID.into()).unwrap()
}

fn names(path: &Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

#[test]
fn windows_private_store_conversation_missing_root_is_not_created() {
    let root = tempfile::tempdir().unwrap();
    let absent = root.path().join("absent");
    let before = names(root.path());
    assert_eq!(
        ConversationStore::create(&absent, state())
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(
        ConversationStore::open(&absent, "project", ID)
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert!(!absent.exists());
    assert_eq!(
        private_directory(&absent).unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
    assert!(!absent.exists());
    assert_eq!(names(root.path()), before);
}

#[test]
fn windows_private_store_conversation_preserves_fixture_and_skips_reader() {
    let root = tempfile::tempdir().unwrap();
    let sessions = root.path().join("sessions-v2");
    let directory = sessions.join(ID);
    let nested = directory.join("snapshots");
    fs::create_dir_all(&nested).unwrap();
    let marker = directory.join("state.json");
    let raw = directory.join("raw.jsonl");
    let lock = directory.join("writer.lock");
    let binary = nested.join("retained.bin");
    let encoded = serde_json::to_vec(&state()).unwrap();
    let decoded: ConversationStateV2 = serde_json::from_slice(&encoded).unwrap();
    decoded.validate().unwrap();
    assert_eq!(serde_json::to_vec(&decoded).unwrap(), encoded);
    fs::write(&marker, &encoded).unwrap();
    fs::write(&raw, b"retained\0\xffraw").unwrap();
    fs::write(&lock, b"retained\0\xfflock").unwrap();
    fs::write(&binary, b"retained\0\xffnested").unwrap();
    let before_names = [
        names(root.path()),
        names(&sessions),
        names(&directory),
        names(&nested),
    ];
    let bytes = || [&marker, &raw, &lock, &binary].map(|path| fs::read(path).unwrap());
    let before_bytes = bytes();
    let called = Cell::new(false);
    assert_eq!(
        ConversationStore::open_with_reader(root.path(), "project", ID, &mut |_| {
            called.set(true);
            panic!("unsupported store must not read")
        })
        .unwrap_err()
        .kind(),
        io::ErrorKind::Unsupported
    );
    assert!(!called.get());
    assert_eq!(
        ConversationStore::create(root.path(), state())
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(
        [
            names(root.path()),
            names(&sessions),
            names(&directory),
            names(&nested),
        ],
        before_names
    );
    assert_eq!(bytes(), before_bytes);
}

#[test]
fn windows_private_store_conversation_rewind_candidate_does_not_create_a_root() {
    let root = tempfile::tempdir().unwrap();
    let absent = root.path().join("absent");
    let before = names(root.path());
    let mut state = state();
    state.generation = 1;
    state.validate().unwrap();
    let expected = ConversationSnapshot {
        state,
        raw_hash: content_hash(&[]),
        raw_offset: 0,
        events: vec![],
    };
    let metadata = RewindBranchMetadataV1 {
        schema_version: REWIND_BOUNDARY_SCHEMA_VERSION,
        boundary_id: "boundary".into(),
        origin_session_id: "00000000-0000-4000-8000-000000000002".into(),
        origin_generation: 2,
        workspace_pre_manifest_id: "before".into(),
        workspace_post_manifest_id: "after".into(),
        branch_initial_raw_offset: 0,
        branch_initial_raw_hash: content_hash(&[]),
        visible_memory_raw_object_ids: vec![],
    };
    validate_rewind_branch_metadata(&metadata).unwrap();
    assert_eq!(
        ConversationStore::create_rewind_candidate(&absent, expected, &metadata)
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert!(!absent.exists());
    assert_eq!(names(root.path()), before);
}

#[test]
fn windows_private_store_conversation_lock_refuses_before_touching_a_file() {
    let root = tempfile::tempdir().unwrap();
    let lock = root.path().join("writer.lock");
    fs::write(&lock, b"retained\0\xfflock").unwrap();
    let before_names = names(root.path());
    let before_bytes = fs::read(&lock).unwrap();
    let store = ConversationStore {
        directory: root.path().into(),
        project_id: "project".into(),
        session_id: ID.into(),
        windows: None,
    };
    assert_eq!(store.directory(), root.path());
    assert_eq!(
        store.lock().err().unwrap().kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(names(root.path()), before_names);
    assert_eq!(fs::read(&lock).unwrap(), before_bytes);
}

fn native_root(name: &str) -> PathBuf {
    use polaris_auth::protection::{WindowsPinnedDirectory, WindowsPrivateRoot};
    let base = PathBuf::from(
        std::env::var_os("POLARIS_WINDOWS_STORAGE_TEST_ROOT")
            .expect("native acceptance requires the explicit disposable root"),
    );
    WindowsPinnedDirectory::open_existing(&base).unwrap();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let path = base.join(format!(
        "b1-conversation-{name}-{}-{nonce}",
        std::process::id()
    ));
    WindowsPrivateRoot::create_new(&path).unwrap();
    path
}
fn native_store(root: &Path) -> ConversationStore {
    ConversationStore::create(root, state()).unwrap()
}
fn native_turn(lock: &mut LockedConversation<'_>) -> ConversationSnapshot {
    lock.append(Message::user("question"), true).unwrap();
    lock.append(Message::assistant("answer"), false).unwrap()
}
fn native_write(store: &ConversationStore, name: &str, bytes: &[u8]) {
    let directory =
        polaris_auth::protection::WindowsFamilyDirectory::open_existing(store.directory()).unwrap();
    let mut file = directory
        .open_file(std::ffi::OsStr::new(name), true, true)
        .unwrap();
    file.write_all(bytes).unwrap();
    file.sync_all().unwrap();
}

#[test]
fn windows_conversation_native_roundtrip_snapshot_and_publication() {
    for mode in [HistoryMode::Legacy, HistoryMode::Strict10] {
        let root = native_root("roundtrip");
        let mut initial = state();
        initial.history_mode = mode;
        let store = ConversationStore::create(&root, initial).unwrap();
        let expected = {
            let mut lock = store.lock().unwrap();
            let snapshot = native_turn(&mut lock);
            lock.publish(&snapshot, snapshot.state.clone()).unwrap();
            let current = lock.snapshot().unwrap();
            assert_eq!(current.state.generation, 1);
            assert_eq!(current.state.history_mode, mode);
            let path = lock.save_snapshot(&current).unwrap();
            let bytes = fs::read(&path).unwrap();
            assert_eq!(lock.save_snapshot(&current).unwrap(), path);
            assert_eq!(fs::read(path).unwrap(), bytes);
            current
        };
        let reopened = ConversationStore::open(&root, "project", ID).unwrap();
        let current = reopened.lock().unwrap().snapshot().unwrap();
        assert_eq!(
            serde_json::to_vec(&current).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
        let saved = reopened
            .load_snapshot(current.state.generation, &current.raw_hash)
            .unwrap();
        assert_eq!(
            serde_json::to_vec(&saved).unwrap(),
            serde_json::to_vec(&expected).unwrap()
        );
    }
}

#[test]
fn windows_conversation_native_writer_lock_contends_and_releases() {
    let root = native_root("lock");
    let store = native_store(&root);
    let clone = store.clone();
    let lock = store.lock().unwrap();
    assert!(clone.lock().is_err());
    drop(lock);
    drop(clone.lock().unwrap());
    drop(store.lock().unwrap());
}

#[test]
fn windows_conversation_native_fork_and_clear_preserve_parent_and_raw() {
    let root = native_root("fork");
    let store = native_store(&root);
    let mut lock = store.lock().unwrap();
    let snapshot = native_turn(&mut lock);
    lock.publish(&snapshot, snapshot.state.clone()).unwrap();
    let parent_state = fs::read(store.directory().join("state.json")).unwrap();
    let parent_raw = fs::read(store.directory().join("raw.jsonl")).unwrap();
    let child_id = "00000000-0000-4000-8000-000000000002";
    let child = lock.fork(&root, child_id, &"a".repeat(64)).unwrap();
    assert_eq!(
        fs::read(store.directory().join("state.json")).unwrap(),
        parent_state
    );
    assert_eq!(
        fs::read(store.directory().join("raw.jsonl")).unwrap(),
        parent_raw
    );
    drop(lock);
    let mut child_lock = child.lock().unwrap();
    let copied = child_lock.snapshot().unwrap();
    assert_eq!(copied.events.len(), 2);
    assert_eq!(copied.state.ancestors.len(), 1);
    let raw = fs::read(child.directory().join("raw.jsonl")).unwrap();
    child_lock.clear_epoch().unwrap();
    let cleared = child_lock.snapshot().unwrap();
    assert_eq!(cleared.state.epoch, copied.state.epoch + 1);
    assert_eq!(cleared.state.generation, copied.state.generation + 1);
    assert!(cleared.active_turns().is_empty());
    assert_eq!(fs::read(child.directory().join("raw.jsonl")).unwrap(), raw);
    drop(child_lock);
    assert_eq!(
        ConversationStore::open(&root, "project", child_id)
            .unwrap()
            .lock()
            .unwrap()
            .snapshot()
            .unwrap()
            .state,
        cleared.state
    );
}

#[test]
fn windows_conversation_native_foreign_hardlink_refuses_without_writes() {
    let root = native_root("foreign-link");
    let store = native_store(&root);
    let raw = store.directory().join("raw.jsonl");
    let alias = root.join("retained-alias");
    fs::hard_link(&raw, &alias).unwrap();
    let before_names = names(store.directory());
    let marker = fs::read(store.directory().join("state.json")).unwrap();
    let raw_bytes = fs::read(&raw).unwrap();
    let called = Cell::new(false);
    assert_eq!(
        ConversationStore::open_with_reader(&root, "project", ID, &mut |_| {
            called.set(true);
            panic!("native store must not delegate protected reads")
        })
        .unwrap_err()
        .kind(),
        io::ErrorKind::Unsupported
    );
    assert!(!called.get());
    assert_eq!(names(store.directory()), before_names);
    assert_eq!(
        fs::read(store.directory().join("state.json")).unwrap(),
        marker
    );
    assert_eq!(fs::read(&raw).unwrap(), raw_bytes);
    assert_eq!(fs::read(&alias).unwrap(), raw_bytes);
}

#[test]
fn windows_conversation_native_unsupported_states_and_sidecars_preserve_bytes() {
    let root = native_root("unsupported");
    let mut structured = state();
    structured.history_mode = HistoryMode::Structured10;
    assert_eq!(
        ConversationStore::create(&root, structured)
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert!(names(&root).is_empty());
    let store = native_store(&root);
    let expected = store.lock().unwrap().snapshot().unwrap();
    let before_names = names(store.directory());
    let marker = fs::read(store.directory().join("state.json")).unwrap();
    assert_eq!(
        store
            .bind_workspace_head(&expected, "manifest")
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(names(store.directory()), before_names);
    native_write(
        &store,
        "workspace-head-binding-v1.json",
        b"retained-workspace",
    );
    native_write(&store, "rewind-boundaries-v1.json", b"retained-rewind");
    let names_before = names(store.directory());
    assert_eq!(
        store.workspace_head_binding().unwrap_err().kind(),
        io::ErrorKind::Unsupported
    );
    let mut lock = store.lock().unwrap();
    let called = Cell::new(false);
    assert_eq!(
        lock.load_rewind_boundaries_with_reader(&mut |_| {
            called.set(true);
            panic!("unsupported rewind must not read")
        })
        .unwrap_err()
        .kind(),
        io::ErrorKind::Unsupported
    );
    assert!(!called.get());
    assert_eq!(
        lock.publish(&expected, expected.state.clone())
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(
        fs::read(store.directory().join("state.json")).unwrap(),
        marker
    );
    assert_eq!(
        fs::read(store.directory().join("workspace-head-binding-v1.json")).unwrap(),
        b"retained-workspace"
    );
    assert_eq!(
        fs::read(store.directory().join("rewind-boundaries-v1.json")).unwrap(),
        b"retained-rewind"
    );
    assert_eq!(names(store.directory()), names_before);
}

#[test]
fn windows_conversation_native_snapshot_collision_preserves_existing_bytes() {
    let root = native_root("collision");
    let store = native_store(&root);
    let mut lock = store.lock().unwrap();
    let snapshot = native_turn(&mut lock);
    let name = format!("{}-{}.json", snapshot.state.generation, snapshot.raw_hash);
    let directory = polaris_auth::protection::WindowsFamilyDirectory::open_existing(
        &store.directory().join("snapshots"),
    )
    .unwrap();
    let mut file = directory
        .open_file(std::ffi::OsStr::new(&name), true, true)
        .unwrap();
    file.write_all(b"retained-collision").unwrap();
    file.sync_all().unwrap();
    drop(file);
    let before_names = names(&store.directory().join("snapshots"));
    assert_eq!(
        lock.save_snapshot(&snapshot).unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    assert_eq!(
        fs::read(store.directory().join("snapshots").join(name)).unwrap(),
        b"retained-collision"
    );
    assert_eq!(names(&store.directory().join("snapshots")), before_names);
}

#[test]
fn windows_conversation_native_interrupted_marker_retains_raw_and_source() {
    let root = native_root("interrupted");
    let store = native_store(&root);
    let mut lock = store.lock().unwrap();
    let snapshot = native_turn(&mut lock);
    let marker = fs::read(store.directory().join("state.json")).unwrap();
    let raw = fs::read(store.directory().join("raw.jsonl")).unwrap();
    FAIL_POINT.with(|failure| failure.set(Some("atomic_rename")));
    let result = lock.publish(&snapshot, snapshot.state.clone());
    FAIL_POINT.with(|failure| failure.set(None));
    assert!(result.is_err());
    assert_eq!(
        fs::read(store.directory().join("state.json")).unwrap(),
        marker
    );
    assert_eq!(fs::read(store.directory().join("raw.jsonl")).unwrap(), raw);
    assert!(
        names(store.directory())
            .iter()
            .any(|name| name.to_string_lossy().starts_with(".pending-"))
    );
    drop(lock);
    let reopened = ConversationStore::open(&root, "project", ID).unwrap();
    let current = reopened.lock().unwrap().snapshot().unwrap();
    assert_eq!(current.state.generation, 0);
    assert_eq!(current.raw_hash, snapshot.raw_hash);
}
