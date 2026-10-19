use super::*;
use crate::{
    MemoryStore, Record,
    conversation::{PublishedView, Scope},
};
use std::{
    fs,
    sync::atomic::{AtomicBool, Ordering},
    time::{SystemTime, UNIX_EPOCH},
};

fn fixture(name: &str) -> (PathBuf, WindowsPrivateRoot) {
    let base = PathBuf::from(
        std::env::var_os("POLARIS_WINDOWS_STORAGE_TEST_ROOT")
            .expect("protected SQLite acceptance needs an explicit disposable root"),
    );
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let case = base.join(format!("sqlite-{name}-{}-{nonce}", std::process::id()));
    fs::create_dir(&case).unwrap();
    let path = case.join("private");
    let root = WindowsPrivateRoot::create_new(&path).unwrap();
    (path.join("memory.sqlite"), root) // Keep evidence and failed fixtures.
}
fn record(id: &str) -> Record {
    Record {
        project_id: "fixture-project".into(),
        session_id: "fixture-session".into(),
        id: id.into(),
        role: "user".into(),
        timestamp: "fixture-time".into(),
        source: "native-fixture".into(),
        text: "retained text".into(),
    }
}
fn view() -> PublishedView {
    PublishedView {
        scope: Scope {
            project_id: "fixture-project".into(),
            session_id: "fixture-session".into(),
            epoch: 1,
            generation: 1,
        },
        visible_ids: vec![],
        ancestors: vec![],
    }
}
fn names(path: &Path) -> Vec<std::ffi::OsString> {
    let mut names = fs::read_dir(path)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect::<Vec<_>>();
    names.sort();
    names
}

#[test]
fn windows_sqlite_protected_create_marker_reopen_and_default_vfs() {
    let (path, root) = fixture("create");
    let default = unsafe { sql::sqlite3_vfs_find(null()) };
    let default_name = unsafe { CStr::from_ptr((*default).zName) }.to_owned();
    let mut store = MemoryStore::open_windows_protected(&path, true).unwrap();
    store.upsert(&record("first"), None).unwrap();
    let called = AtomicBool::new(false);
    store
        .with_conversation_marker(&view(), || {
            called.store(true, Ordering::Relaxed);
            Ok(())
        })
        .unwrap();
    assert!(called.load(Ordering::Relaxed));
    let id = root
        .open_sqlite_file(OsStr::new("memory.sqlite"), false, false, false)
        .unwrap()
        .identity();
    drop(store);
    let reopened = MemoryStore::open_windows_protected(&path, false).unwrap();
    let text: String = reopened
        .connection
        .query_row(
            "SELECT text FROM memory_records WHERE id='first'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(text, "retained text");
    assert_eq!(
        root.open_sqlite_file(OsStr::new("memory.sqlite"), false, false, false)
            .unwrap()
            .identity(),
        id
    );
    assert_eq!(
        unsafe { CStr::from_ptr((*sql::sqlite3_vfs_find(null())).zName) },
        default_name.as_c_str()
    );
}

#[test]
fn windows_sqlite_protected_wal_shared_memory_and_fresh_readback() {
    let (path, _root) = fixture("wal");
    let mut first = MemoryStore::open_windows_protected(&path, true).unwrap();
    let mode: String = first
        .connection
        .query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))
        .unwrap();
    assert_eq!(
        mode, "wal",
        "WAL may not silently fall back to rollback mode"
    );
    first.upsert(&record("before"), None).unwrap();
    let second = MemoryStore::open_windows_protected(&path, false).unwrap();
    let count: i64 = second
        .connection
        .query_row("SELECT count(*) FROM memory_records", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 1);
    first.upsert(&record("after"), None).unwrap();
    let count: i64 = second
        .connection
        .query_row("SELECT count(*) FROM memory_records", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2);
    drop(second);
    drop(first);
    let reopened = MemoryStore::open_windows_protected(&path, false).unwrap();
    let count: i64 = reopened
        .connection
        .query_row("SELECT count(*) FROM memory_records", [], |r| r.get(0))
        .unwrap();
    assert_eq!(count, 2);
}

#[test]
fn windows_sqlite_protected_real_writer_contention_and_release() {
    let (path, _root) = fixture("locks");
    let mut first = MemoryStore::open_windows_protected(&path, true).unwrap();
    let mut second = MemoryStore::open_windows_protected(&path, false).unwrap();
    second
        .connection
        .busy_timeout(std::time::Duration::from_millis(10))
        .unwrap();
    let held = first
        .connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let error = second.upsert(&record("blocked"), None).unwrap_err();
    assert!(
        matches!(error, crate::Error::Sqlite(rusqlite::Error::SqliteFailure(e, _))
        if e.code == rusqlite::ErrorCode::DatabaseBusy)
    );
    drop(held);
    second.upsert(&record("released"), None).unwrap();
}

#[test]
fn windows_sqlite_protected_reader_accepts_recreated_writer_journals() {
    let (path, _root) = fixture("journal-generations");
    let mut writer = MemoryStore::open_windows_protected(&path, true).unwrap();
    let reader = MemoryStore::open_windows_protected(&path, false).unwrap();
    for id in ["one", "two"] {
        let transaction = writer
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        transaction.execute("INSERT INTO memory_records VALUES ('fixture-project','fixture-session',?1,'user','fixture-time','native-fixture','retained')", [id]).unwrap();
        assert!(path.with_file_name("memory.sqlite-journal").exists());
        let count: i64 = reader
            .connection
            .query_row("SELECT count(*) FROM memory_records", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, if id == "one" { 0 } else { 1 });
        transaction.commit().unwrap();
        assert!(!path.with_file_name("memory.sqlite-journal").exists());
        // The reader does not query during the absence between journal generations.
    }
}

#[test]
fn windows_sqlite_protected_foreign_sidecar_fails_before_creation() {
    let (path, _root) = fixture("foreign-before");
    let sidecar = path.with_file_name("memory.sqlite-journal");
    fs::write(&sidecar, b"foreign-retained").unwrap();
    let before = names(path.parent().unwrap());
    assert!(MemoryStore::open_windows_protected(&path, true).is_err());
    assert!(!path.exists());
    assert_eq!(fs::read(&sidecar).unwrap(), b"foreign-retained");
    assert_eq!(names(path.parent().unwrap()), before);
}

#[test]
fn windows_sqlite_protected_foreign_sidecar_fails_before_marker_callback() {
    let (path, _root) = fixture("foreign-callback");
    let mut store = MemoryStore::open_windows_protected(&path, true).unwrap();
    store.upsert(&record("retained"), None).unwrap();
    let before = fs::read(&path).unwrap();
    let sidecar = path.with_file_name("memory.sqlite-journal");
    fs::write(&sidecar, b"foreign-retained").unwrap();
    let before_names = names(path.parent().unwrap());
    let called = AtomicBool::new(false);
    assert!(
        store
            .with_conversation_marker(&view(), || {
                called.store(true, Ordering::Relaxed);
                Ok(())
            })
            .is_err()
    );
    assert!(!called.load(Ordering::Relaxed));
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(fs::read(&sidecar).unwrap(), b"foreign-retained");
    assert_eq!(names(path.parent().unwrap()), before_names);
}

#[test]
fn windows_sqlite_protected_directory_publication_preserves_collision_and_identity() {
    let (path, root) = fixture("directory-publish");
    let source = WindowsPrivateRoot::create_child(root.directory(), OsStr::new("source")).unwrap();
    let identity = source.identity();
    let mut payload = source.create_file(OsStr::new("payload")).unwrap();
    payload.write_all(b"retained-source").unwrap();
    payload.flush().unwrap();
    drop(payload);
    drop(source);
    let destination =
        WindowsPrivateRoot::create_child(root.directory(), OsStr::new("occupied")).unwrap();
    let mut payload = destination.create_file(OsStr::new("payload")).unwrap();
    payload.write_all(b"retained-destination").unwrap();
    payload.flush().unwrap();
    drop(payload);
    drop(destination);
    assert!(
        root.rename_child_no_clobber(OsStr::new("source"), OsStr::new("occupied"))
            .is_err()
    );
    let parent = path.parent().unwrap();
    assert_eq!(
        fs::read(parent.join("source/payload")).unwrap(),
        b"retained-source"
    );
    assert_eq!(
        fs::read(parent.join("occupied/payload")).unwrap(),
        b"retained-destination"
    );
    root.rename_child_no_clobber(OsStr::new("source"), OsStr::new("published"))
        .unwrap();
    assert!(!parent.join("source").exists());
    let published = WindowsPrivateRoot::open_existing(&parent.join("published")).unwrap();
    assert_eq!(published.identity(), identity);
    assert_eq!(
        published
            .open_file(OsStr::new("payload"))
            .unwrap()
            .read_bytes(1024)
            .unwrap(),
        b"retained-source"
    );
}

#[test]
fn windows_sqlite_protected_typed_offset_range_lock_mapping_and_delete() {
    let (_path, root) = fixture("native-bridge");
    let file = root
        .open_sqlite_file(OsStr::new("bridge-shm"), true, true, true)
        .unwrap();
    let other = root
        .open_sqlite_file(OsStr::new("bridge-shm"), true, false, false)
        .unwrap();
    file.write_at(7, b"retained").unwrap();
    file.flush().unwrap();
    let mut bytes = [0; 8];
    other.read_at(7, &mut bytes).unwrap();
    assert_eq!(&bytes, b"retained");
    let shared = file.try_lock_range(120, 1, false).unwrap();
    let second = other.try_lock_range(120, 1, false).unwrap();
    assert!(matches!(
        other.try_lock_range(120, 1, true),
        Err(AuthError::Busy)
    ));
    shared.release().unwrap();
    second.release().unwrap();
    let exclusive = file.try_lock_range(120, 1, true).unwrap();
    assert!(matches!(
        other.try_lock_range(120, 1, false),
        Err(AuthError::Busy)
    ));
    exclusive.release().unwrap();
    file.ensure_len(65536).unwrap();
    let mut a = file.map_shared(32768, 32768).unwrap();
    let mut b = other.map_shared(32768, 32768).unwrap();
    unsafe {
        *a.as_mut_ptr().cast::<u8>() = 73;
    }
    std::sync::atomic::fence(Ordering::SeqCst);
    assert_eq!(unsafe { *b.as_mut_ptr().cast::<u8>() }, 73);
    a.unmap().unwrap();
    b.unmap().unwrap();
    drop(a);
    drop(b);
    let identity = file.identity();
    drop(other);
    drop(file);
    assert!(
        root.delete_sqlite_sidecar(OsStr::new("bridge-shm"), identity)
            .unwrap()
    );
    assert!(
        root.open_sqlite_file(OsStr::new("bridge-shm"), false, false, false)
            .is_err()
    );
}
