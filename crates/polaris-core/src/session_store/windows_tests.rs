use super::*;
use std::fs;

const ID: &str = "00000000-0000-4000-8000-000000000001";

fn names(path: &Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

#[test]
fn windows_session_creation_unavailable_does_not_create_root_or_database() {
    let parent = tempfile::tempdir().unwrap();
    let data_root = parent.path().join("absent");
    let database = parent.path().join("memory.sqlite3");
    for mode in [
        HistoryMode::Legacy,
        HistoryMode::Strict10,
        HistoryMode::Structured10,
    ] {
        let state = PersistedSession::new_state("project", ID, mode, None).unwrap();
        state.validate().unwrap();
        assert_eq!(state.session_id, ID);
        assert_eq!(state.history_mode, mode);
        assert_eq!(
            PersistedSession::create(&data_root, &database, "project", mode, None)
                .err()
                .expect("unsupported session creation must refuse")
                .kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            PersistedSession::create_with_id(&data_root, &database, "project", ID, mode, None)
                .err()
                .expect("unsupported named session creation must refuse")
                .kind(),
            io::ErrorKind::Unsupported
        );
    }
    assert!(!data_root.exists());
    assert!(!database.exists());
    assert!(names(parent.path()).is_empty());
}

#[test]
fn windows_session_creation_unavailable_preserves_database_and_sidecars() {
    let root = tempfile::tempdir().unwrap();
    let data_root = root.path().join("data");
    let nested = data_root.join("retained");
    fs::create_dir_all(&nested).unwrap();
    let database = root.path().join("memory.sqlite3");
    drop(MemoryStore::open(&database).unwrap());
    let wal = root.path().join("memory.sqlite3-wal");
    let shm = root.path().join("memory.sqlite3-shm");
    let journal = root.path().join("memory.sqlite3-journal");
    let binary = nested.join("retained.bin");
    fs::write(&wal, b"retained\0\xffwal").unwrap();
    fs::write(&shm, b"retained\0\xffshm").unwrap();
    fs::write(&journal, b"retained\0\xffjournal").unwrap();
    fs::write(&binary, b"retained\0\xffnested").unwrap();
    let before_names = [names(root.path()), names(&data_root), names(&nested)];
    let bytes = || [&database, &wal, &shm, &journal, &binary].map(|path| fs::read(path).unwrap());
    let before_bytes = bytes();
    assert!(before_bytes[0].starts_with(b"SQLite format 3\0"));
    for mode in [
        HistoryMode::Legacy,
        HistoryMode::Strict10,
        HistoryMode::Structured10,
    ] {
        assert_eq!(
            PersistedSession::create(&data_root, &database, "project", mode, None)
                .err()
                .expect("unsupported session creation must refuse")
                .kind(),
            io::ErrorKind::Unsupported
        );
        assert_eq!(
            PersistedSession::create_with_id(&data_root, &database, "project", ID, mode, None)
                .err()
                .expect("unsupported named session creation must refuse")
                .kind(),
            io::ErrorKind::Unsupported
        );
    }
    assert_eq!(
        [names(root.path()), names(&data_root), names(&nested)],
        before_names
    );
    assert_eq!(bytes(), before_bytes);
}

const CHILD: &str = "00000000-0000-4000-8000-000000000002";

fn native_root(label: &str) -> PathBuf {
    use polaris_auth::protection::{WindowsPinnedDirectory, WindowsPrivateRoot};
    use std::time::{SystemTime, UNIX_EPOCH};
    let base = PathBuf::from(
        std::env::var_os("POLARIS_WINDOWS_STORAGE_TEST_ROOT")
            .expect("native Session acceptance requires an explicit disposable root"),
    );
    WindowsPinnedDirectory::open_existing(&base).unwrap();
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = base.join(format!("b1-session-{label}-{}-{nonce}", std::process::id()));
    WindowsPrivateRoot::create_new(&root).unwrap();
    root
}

fn saved_bytes(saved: &PersistedSession) -> [Vec<u8>; 2] {
    ["raw.jsonl", "state.json"].map(|name| fs::read(saved.store.directory().join(name)).unwrap())
}

#[test]
fn windows_session_native_lifecycle_cursor_fork_clear_and_reopen() {
    for mode in [HistoryMode::Legacy, HistoryMode::Strict10] {
        let root = native_root("lifecycle");
        let database = root.join("memory.sqlite3");
        let saved = if mode == HistoryMode::Legacy {
            PersistedSession::create_with_id(&root, &database, "project", ID, mode, None)
        } else {
            PersistedSession::create(&root, &database, "project", mode, None)
        }
        .unwrap();
        let id = saved.snapshot().unwrap().state.session_id;
        let stale = PersistedSession::open(&root, &database, "project", &id).unwrap();
        saved.append(Message::user("first"), true, None).unwrap();
        let before = saved_bytes(&saved);
        assert!(stale.append(Message::user("stale"), true, None).is_err());
        assert_eq!(saved_bytes(&saved), before);
        assert!(stale.snapshot().is_err());
        saved
            .clone()
            .append(Message::assistant("answer"), false, None)
            .unwrap();
        let snapshot = saved.snapshot().unwrap();
        assert_eq!(snapshot.events.len(), 2);
        assert_eq!(snapshot.state.history_mode, mode);
        let before = saved_bytes(&saved);
        let child = saved.fork_with_id(CHILD, &"a".repeat(64)).unwrap();
        assert_eq!(saved_bytes(&saved), before);
        assert_eq!(child.snapshot().unwrap().events.len(), 2);
        saved.clear().unwrap();
        saved
            .append(Message::user("after clear"), true, None)
            .unwrap();
        let parent = saved.snapshot().unwrap();
        assert_eq!(parent.events.len(), 3);
        assert_eq!(parent.recent_messages().len(), 1);
        assert_eq!(parent.state.epoch, snapshot.state.epoch + 1);
        for (id, expected) in [(&id[..], parent), (CHILD, child.snapshot().unwrap())] {
            let reopened = PersistedSession::open(&root, &database, "project", id).unwrap();
            let actual = reopened.snapshot().unwrap();
            assert_eq!(actual.state.history_mode, mode);
            assert_eq!(
                serde_json::to_vec(&actual).unwrap(),
                serde_json::to_vec(&expected).unwrap()
            );
        }
    }
}

#[test]
fn windows_session_native_real_tombstone_blocks_writes_and_resume() {
    let root = native_root("tombstone");
    let database = root.join("memory.sqlite3");
    let saved = PersistedSession::create_with_id(
        &root,
        &database,
        "project",
        ID,
        HistoryMode::Legacy,
        None,
    )
    .unwrap();
    saved.append(Message::user("retained"), true, None).unwrap();
    let mut memory = MemoryStore::open_windows_protected(&database, false).unwrap();
    memory.delete_session("project", ID).unwrap();
    assert!(memory.is_session_forgotten("project", ID).unwrap());
    drop(memory);
    let before = saved_bytes(&saved);
    let before_names = names(saved.store.directory());
    assert!(
        saved
            .append(Message::assistant("blocked"), false, None)
            .is_err()
    );
    assert!(saved.clear().is_err());
    assert!(saved.fork_with_id(CHILD, &"a".repeat(64)).is_err());
    assert!(saved.snapshot().is_err());
    assert!(PersistedSession::open(&root, &database, "project", ID).is_err());
    assert_eq!(saved_bytes(&saved), before);
    assert_eq!(names(saved.store.directory()), before_names);
    assert!(!root.join("sessions-v2").join(CHILD).exists());
}

#[test]
fn windows_session_native_foreign_database_preserves_all_bytes() {
    let root = native_root("foreign-db");
    let foreign = tempfile::tempdir().unwrap();
    let database = foreign.path().join("memory.sqlite3");
    drop(MemoryStore::open(&database).unwrap());
    let paths = [
        database.clone(),
        foreign.path().join("memory.sqlite3-journal"),
        foreign.path().join("memory.sqlite3-wal"),
        foreign.path().join("memory.sqlite3-shm"),
    ];
    for path in &paths[1..] {
        fs::write(path, b"foreign\0retained").unwrap();
    }
    let before = paths.each_ref().map(|path| fs::read(path).unwrap());
    let before_names = names(foreign.path());
    assert!(
        PersistedSession::create_with_id(
            &root,
            &database,
            "project",
            ID,
            HistoryMode::Legacy,
            None,
        )
        .is_err()
    );
    assert_eq!(paths.each_ref().map(|path| fs::read(path).unwrap()), before);
    assert_eq!(names(foreign.path()), before_names);
    assert!(names(&root).is_empty());
}

#[test]
fn windows_session_native_foreign_sidecars_block_creation_and_append() {
    for suffix in ["-journal", "-wal", "-shm"] {
        let root = native_root("foreign-before");
        let database = root.join("memory.sqlite3");
        let sidecar = root.join(format!("memory.sqlite3{suffix}"));
        fs::write(&sidecar, b"foreign\0sidecar").unwrap();
        let before_names = names(&root);
        assert!(
            PersistedSession::create_with_id(
                &root,
                &database,
                "project",
                ID,
                HistoryMode::Legacy,
                None,
            )
            .is_err()
        );
        assert!(!database.exists());
        assert_eq!(names(&root), before_names);
        assert_eq!(fs::read(&sidecar).unwrap(), b"foreign\0sidecar");

        let root = native_root("foreign-after");
        let database = root.join("memory.sqlite3");
        let saved = PersistedSession::create_with_id(
            &root,
            &database,
            "project",
            ID,
            HistoryMode::Legacy,
            None,
        )
        .unwrap();
        let sidecar = root.join(format!("memory.sqlite3{suffix}"));
        fs::write(&sidecar, b"foreign\0sidecar").unwrap();
        let before = saved_bytes(&saved);
        let db_before = fs::read(&database).unwrap();
        let before_names = names(&root);
        assert!(saved.snapshot().is_err());
        assert!(saved.append(Message::user("blocked"), true, None).is_err());
        assert_eq!(saved_bytes(&saved), before);
        assert_eq!(fs::read(&database).unwrap(), db_before);
        assert_eq!(fs::read(&sidecar).unwrap(), b"foreign\0sidecar");
        assert_eq!(names(&root), before_names);
    }
}

#[test]
fn windows_session_native_unsupported_and_duplicate_refuse_before_schema() {
    let root = native_root("unsupported");
    let database = root.join("memory.sqlite3");
    let workflow = SessionWorkflow::new(WorkflowConfig::default());
    for (mode, workflow) in [
        (HistoryMode::Structured10, None),
        (HistoryMode::Legacy, Some(&workflow)),
    ] {
        assert_eq!(
            PersistedSession::create_with_id(&root, &database, "project", ID, mode, workflow,)
                .err()
                .unwrap()
                .kind(),
            io::ErrorKind::Unsupported
        );
        assert!(!database.exists());
        assert!(names(&root).is_empty());
    }
    let saved = PersistedSession::create_with_id(
        &root,
        &database,
        "project",
        ID,
        HistoryMode::Legacy,
        None,
    )
    .unwrap();
    let other = root.join("other.sqlite3");
    let before_names = names(&root);
    let before = saved_bytes(&saved);
    let db_before = fs::read(&database).unwrap();
    assert!(
        PersistedSession::create_with_id(&root, &other, "project", ID, HistoryMode::Legacy, None,)
            .is_err()
    );
    assert!(!other.exists());
    assert_eq!(
        saved
            .append(Message::user("blocked"), true, Some(&workflow))
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(
        saved
            .prepare_workflow(WorkflowConfig::default())
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(
        PersistedSession::import(&root, &other, "project", HistoryMode::Legacy, None, &[],)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::Unsupported
    );
    let mut candidate = saved.store.lock().unwrap().snapshot().unwrap().state;
    let hash = crate::conversation_state::content_hash(&serde_json::to_vec(&candidate).unwrap());
    candidate.history_mode = HistoryMode::Structured10;
    assert_eq!(
        saved
            .publish_session_change(0, &hash, candidate)
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(saved_bytes(&saved), before);
    assert_eq!(fs::read(&database).unwrap(), db_before);
    assert_eq!(names(&root), before_names);
}
