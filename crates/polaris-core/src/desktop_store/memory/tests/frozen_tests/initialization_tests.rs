//! Initializer ownership, additive migration and unchanged failure state.
use super::*;

#[test]
fn structured_initializer_creates_only_fresh_index_after_resource_validation() {
    let (_root, writer, target) =
        setup_turns("memory-project", "cold", HistoryMode::Structured10, 1);
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let before = bytes(&writer.test_directory());
    let mut wrong = resources();
    wrong.fingerprint = "b".repeat(64);
    assert!(matches!(
        writer.prepare_structured_memory_index(&target, &database, &wrong),
        Err(StoreError::CasConflict(_))
    ));
    assert!(!database.exists());
    writer
        .prepare_structured_memory_index(&target, &database, &resources())
        .unwrap();
    let db = MemoryStore::open_read_only(&database).unwrap();
    assert_eq!(
        db.structured_memory_schema_version().unwrap(),
        Some(polaris_memory::structured::STRUCTURED_MEMORY_SCHEMA_VERSION)
    );
    assert_eq!(
        bytes(&writer.test_directory()),
        before,
        "index preparation must not bind or publish a marker"
    );
}

#[tokio::test]
async fn structured_initializer_reuses_exact_v3_ancestor_and_releases_locks() {
    let Fixture {
        root,
        parent,
        mut child,
        database,
        _directory,
        ..
    } = published_parent().await;
    let parent_marker = parent.snapshot().unwrap().marker;
    let parent_path = parent.test_directory();
    let parent_before = bytes(&parent_path);
    let target = accept(&mut child, 1);
    let before = bytes(&child.test_directory());
    assert!(matches!(
        child.prepare_structured_memory_index(&target, &database, &resources()),
        Err(StoreError::Busy)
    ));
    assert_eq!(bytes(&child.test_directory()), before);
    drop(parent);
    child
        .prepare_structured_memory_index(&target, &database, &resources())
        .unwrap();
    assert_eq!(bytes(&child.test_directory()), before);
    assert_eq!(bytes(&parent_path), parent_before);
    let reopened = root
        .open(&parent_marker.project_id, &parent_marker.session_id)
        .unwrap();
    drop(reopened);
    child.bind_memory(&target, resources()).unwrap();
    let snapshot = child.memory_snapshot(&target).unwrap();
    assert!(
        child
            .prepare_structured_memory_with_related(&target, &snapshot, &database)
            .is_ok()
    );
}

#[tokio::test]
async fn structured_initializer_rejects_missing_partial_and_changed_established_index() {
    let Fixture {
        parent,
        mut child,
        database,
        _directory,
        ..
    } = published_parent().await;
    drop(parent);
    let target = accept(&mut child, 1);
    let before = bytes(&child.test_directory());
    let good = fs::read(&database).unwrap();
    for damage in [
        "absent",
        "v2-table",
        "v3-table",
        "receipt",
        "summary",
        "component",
    ] {
        fs::write(&database, &good).unwrap();
        if damage == "absent" {
            fs::remove_file(&database).unwrap();
        } else {
            let db = rusqlite::Connection::open(&database).unwrap();
            db.pragma_update(None, "foreign_keys", false).unwrap();
            db.execute_batch(match damage {
                "v2-table" => "DROP TABLE conversation_vectors_v2",
                "v3-table" => "DROP TABLE conversation_entities_v3",
                "receipt" => "UPDATE conversation_publications_v3 SET raw_hash='changed'",
                "summary" => "DELETE FROM conversation_summaries_v2",
                "component" => "DELETE FROM memory_schema_meta",
                _ => unreachable!(),
            })
            .unwrap();
        }
        let damaged = fs::read(&database).ok();
        assert!(
            child
                .prepare_structured_memory_index(&target, &database, &resources())
                .is_err(),
            "{damage}"
        );
        assert_eq!(
            fs::read(&database).ok(),
            damaged,
            "{damage}: no database repair or creation"
        );
        assert_eq!(
            bytes(&child.test_directory()),
            before,
            "{damage}: no bind or marker write"
        );
    }
}

async fn v2_parent() -> (PrototypeRoot, Writer, Writer, PathBuf, tempfile::TempDir) {
    let (root, mut parent, target) =
        setup_mode("memory-project", "v2-parent", HistoryMode::Strict10);
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let (work, item) = pending(&mut parent, &target).await;
    parent
        .publish_memory(&target, &work, &item, &database)
        .unwrap();
    finish(&mut parent, &target, 11);
    let mut child = parent
        .fork_memory(SessionId::new("v2-child").unwrap(), "artifact")
        .unwrap();
    // Internal candidate configuration only; public admission remains gated.
    let mut p = child.snapshot().unwrap();
    p.state.configuration.history_mode = HistoryMode::Structured10;
    child.commit(p, false).unwrap();
    (root, parent, child, database, directory)
}

#[tokio::test]
async fn structured_initializer_migrates_verified_v2_ancestor_without_changing_raw_or_summary() {
    let (_root, parent, mut child, database, _directory) = v2_parent().await;
    let parent_path = parent.test_directory();
    let original_parent = bytes(&parent_path);
    drop(parent);
    let target = accept(&mut child, 1);
    let child_before = bytes(&child.test_directory());
    assert_eq!(
        MemoryStore::open_read_only(&database)
            .unwrap()
            .structured_memory_schema_version()
            .unwrap(),
        None
    );
    let db = rusqlite::Connection::open(&database).unwrap();
    let summaries = || {
        db.prepare("SELECT summary,summary_hash FROM conversation_summaries_v2 ORDER BY summary_id")
            .unwrap()
            .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let expected = summaries();
    child
        .prepare_structured_memory_index(&target, &database, &resources())
        .unwrap();
    assert_eq!(
        MemoryStore::open_read_only(&database)
            .unwrap()
            .structured_memory_schema_version()
            .unwrap(),
        Some(polaris_memory::structured::STRUCTURED_MEMORY_SCHEMA_VERSION)
    );
    assert_eq!(summaries(), expected);
    assert_eq!(bytes(&parent_path), original_parent);
    assert_eq!(bytes(&child.test_directory()), child_before);
}

#[tokio::test]
async fn structured_initializer_rolls_back_schema_when_v2_ancestor_receipt_is_missing() {
    let (_root, parent, mut child, database, _directory) = v2_parent().await;
    drop(parent);
    let target = accept(&mut child, 1);
    let before = bytes(&child.test_directory());
    rusqlite::Connection::open(&database)
        .unwrap()
        .execute("DELETE FROM conversation_summaries_v2", [])
        .unwrap();
    let db_before = fs::read(&database).unwrap();
    assert!(
        child
            .prepare_structured_memory_index(&target, &database, &resources())
            .is_err()
    );
    assert_eq!(
        MemoryStore::open_read_only(&database)
            .unwrap()
            .structured_memory_schema_version()
            .unwrap(),
        None
    );
    assert_eq!(fs::read(&database).unwrap(), db_before);
    assert_eq!(bytes(&child.test_directory()), before);
}

#[test]
fn structured_initializer_checks_empty_ancestor_tombstone_before_owner_access() {
    let (_root, mut parent, target) = setup_turns(
        "memory-project",
        "empty-parent",
        HistoryMode::Structured10,
        1,
    );
    finish(&mut parent, &target, 1);
    let mut child = parent
        .fork_memory(SessionId::new("empty-child").unwrap(), "artifact")
        .unwrap();
    let target = accept(&mut child, 1);
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    // Even an ancestor with no IDs requires its existing tombstone index.
    assert!(
        child
            .prepare_structured_memory_index(&target, &database, &resources())
            .is_err()
    );
    assert!(!database.exists());
    let mut db = MemoryStore::open(&database).unwrap();
    db.delete_session("memory-project", "empty-parent").unwrap();
    drop(db);
    let before = bytes(&child.test_directory());
    let database_before = fs::read(&database).unwrap();
    let error = child
        .prepare_structured_memory_index(&target, &database, &resources())
        .unwrap_err();
    assert!(error.to_string().contains("forgotten"), "{error}");
    assert!(
        !matches!(error, StoreError::Busy),
        "must not open the locked deleted ancestor"
    );
    assert_eq!(bytes(&child.test_directory()), before);
    assert_eq!(fs::read(&database).unwrap(), database_before);
}
