//! Atomic memory import, revision and forget regression tests.

use super::*;

fn record(project: &str, session: &str, id: &str) -> Record {
    Record {
        project_id: project.into(),
        session_id: session.into(),
        id: id.into(),
        role: "user".into(),
        timestamp: "2026-10-04".into(),
        source: "explicit-import".into(),
        text: "historical evidence".into(),
    }
}
fn import(
    store: &MemoryStore,
    project: &str,
    operation: u64,
    records: Vec<Record>,
) -> MemoryChangeRequestV1 {
    MemoryChangeRequestV1 {
        operation_id: format!("{operation:032x}"),
        project_id: project.into(),
        expected_revision: store.project_revision(project).unwrap(),
        change: MemoryChangeV1::Import { records },
    }
}

#[test]
fn m3c_normal_atomic_import_embedding_and_forget_advance_revision() {
    let root = tempfile::tempdir().unwrap();
    let mut store = MemoryStore::open(root.path().join("memory.sqlite3")).unwrap();
    let request = import(
        &store,
        "project",
        1,
        vec![record("project", "s", "a"), record("project", "s", "b")],
    );
    let saved = store.apply_change("project", &request).unwrap();
    assert_eq!(saved.records_changed, 2);
    assert!(saved.after_revision > saved.before_revision);
    assert!(store.get("project", "s", "b").unwrap().is_some());
    let embedding = Embedding {
        model: "local".into(),
        values: vec![1.0, 2.0],
    };
    let embed = MemoryChangeRequestV1 {
        operation_id: format!("{:032x}", 2),
        project_id: "project".into(),
        expected_revision: saved.after_revision,
        change: MemoryChangeV1::Embed {
            updates: vec![MemoryEmbeddingUpdateV1 {
                session_id: "s".into(),
                id: "a".into(),
                embedding: embedding.clone(),
            }],
        },
    };
    let saved = store.apply_change("project", &embed).unwrap();
    assert!(store.has_embedding("project", "s", "a", "local").unwrap());
    assert!(saved.after_revision > embed.expected_revision);
    let before = store.project_revision("project").unwrap();
    store
        .set_embedding("project", "s", "b", &embedding)
        .unwrap();
    assert!(store.project_revision("project").unwrap() > before);
    let forget = MemoryChangeRequestV1 {
        operation_id: format!("{:032x}", 3),
        project_id: "project".into(),
        expected_revision: store.project_revision("project").unwrap(),
        change: MemoryChangeV1::Forget {
            session_id: "s".into(),
        },
    };
    let saved = store.apply_change("project", &forget).unwrap();
    assert_eq!(saved.records_changed, 2);
    assert!(store.is_session_forgotten("project", "s").unwrap());
    assert!(store.get("project", "s", "a").unwrap().is_none());
    assert!(!store.has_embedding("project", "s", "a", "local").unwrap());
}

#[test]
fn m3c_denial_wrong_scope_forgotten_and_invalid_vector_preserve_state() {
    let root = tempfile::tempdir().unwrap();
    let mut store = MemoryStore::open(root.path().join("memory.sqlite3")).unwrap();
    let request = import(&store, "project", 1, vec![record("project", "s", "a")]);
    assert!(matches!(
        store.apply_change("foreign", &request),
        Err(Error::WrongProject)
    ));
    assert_eq!(store.project_revision("project").unwrap(), 0);
    store.upsert(&record("project", "s", "a"), None).unwrap();
    let before = store.project_revision("project").unwrap();
    let invalid = MemoryChangeRequestV1 {
        operation_id: format!("{:032x}", 2),
        project_id: "project".into(),
        expected_revision: before,
        change: MemoryChangeV1::Index {
            updates: vec![MemoryEmbeddingUpdateV1 {
                session_id: "s".into(),
                id: "a".into(),
                embedding: Embedding {
                    model: "local".into(),
                    values: vec![0.0],
                },
            }],
        },
    };
    assert!(store.apply_change("project", &invalid).is_err());
    assert_eq!(store.project_revision("project").unwrap(), before);
    assert!(store.get("project", "s", "a").unwrap().is_some());
    store.delete_session("project", "gone").unwrap();
    let rejected = import(
        &store,
        "project",
        3,
        vec![record("project", "s", "b"), record("project", "gone", "c")],
    );
    let before = store.project_revision("project").unwrap();
    assert!(matches!(
        store.apply_change("project", &rejected),
        Err(Error::SessionForgotten)
    ));
    assert_eq!(store.project_revision("project").unwrap(), before);
    assert!(store.get("project", "s", "b").unwrap().is_none());
}

#[test]
fn m3c_stale_preview_rejects_legacy_writer_change() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("memory.sqlite3");
    let mut store = MemoryStore::open(&path).unwrap();
    let request = import(
        &store,
        "project",
        1,
        vec![record("project", "s", "requested")],
    );
    let mut independent = MemoryStore::open_existing(&path).unwrap();
    independent
        .upsert(&record("project", "s", "intervening"), None)
        .unwrap();
    assert!(matches!(
        store.apply_change("project", &request),
        Err(Error::StaleRevision)
    ));
    assert!(store.get("project", "s", "requested").unwrap().is_none());
    assert!(store.get("project", "s", "intervening").unwrap().is_some());
    assert!(
        store
            .change_receipt("project", &request.operation_id)
            .unwrap()
            .is_none()
    );
}

#[test]
fn m3c_restart_receipt_preserves_revision_and_exact_request_binding() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("memory.sqlite3");
    let mut store = MemoryStore::open(&path).unwrap();
    let mut request = import(&store, "project", 1, vec![record("project", "s", "a")]);
    let receipt = store.apply_change("project", &request).unwrap();
    drop(store);
    let mut restarted = MemoryStore::open(&path).unwrap();
    assert_eq!(
        restarted
            .change_receipt("project", &request.operation_id)
            .unwrap(),
        Some(receipt.clone())
    );
    assert_eq!(
        restarted.apply_change("project", &request).unwrap(),
        receipt
    );
    assert_eq!(
        restarted.project_revision("project").unwrap(),
        receipt.after_revision
    );
    if let MemoryChangeV1::Import { records } = &mut request.change {
        records[0].text = "changed request".into();
    }
    assert!(matches!(
        restarted.apply_change("project", &request),
        Err(Error::OperationConflict)
    ));
    assert_eq!(
        restarted.get("project", "s", "a").unwrap().unwrap().text,
        "historical evidence"
    );
}

#[test]
fn m3c_lost_response_replay_does_not_overwrite_later_work() {
    let root = tempfile::tempdir().unwrap();
    let mut store = MemoryStore::open(root.path().join("memory.sqlite3")).unwrap();
    let request = import(&store, "project", 1, vec![record("project", "s", "a")]);
    store.apply_change("project", &request).unwrap(); // Response deliberately discarded.
    let mut later = record("project", "s", "a");
    later.text = "newer evidence".into();
    store.upsert(&later, None).unwrap();
    let before = store.project_revision("project").unwrap();
    let replay = store.apply_change("project", &request).unwrap();
    assert!(replay.after_revision < before);
    assert_eq!(store.project_revision("project").unwrap(), before);
    assert_eq!(
        store.get("project", "s", "a").unwrap().unwrap().text,
        "newer evidence"
    );
}
