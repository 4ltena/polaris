//! Real v2 raw/receipt/SQLite boundaries; no live provider or embedding service.

use super::*;
use crate::conversation_state::{FAIL_POINT, content_hash};
use polaris_memory::conversation::EmbeddingMetadata;
use polaris_provider::Message;
use std::{fs, path::PathBuf};

const ID: &str = "00000000-0000-4000-8000-000000000001";
const CHILD: &str = "00000000-0000-4000-8000-000000000002";

struct Fault(Option<&'static str>);
impl Fault {
    fn at(point: &'static str) -> Self {
        Self(FAIL_POINT.with(|value| value.replace(Some(point))))
    }
}
impl Drop for Fault {
    fn drop(&mut self) {
        FAIL_POINT.with(|value| value.set(self.0));
    }
}

fn setup(root: &Path) -> (ConversationStore, PathBuf, ConversationSnapshot) {
    let mut state = ConversationStateV2::new("project".into(), ID.into()).unwrap();
    state.history_mode = HistoryMode::Structured10;
    let store = ConversationStore::create(root, state).unwrap();
    let database = root.join("memory.sqlite3");
    let mut lock = store.lock().unwrap();
    for turn in 1..=11 {
        let text = if turn == 1 {
            "Aster is production".into()
        } else {
            format!("filler {turn}")
        };
        lock.append(Message::user(text), true).unwrap();
    }
    let snapshot = lock.snapshot().unwrap();
    MemoryStore::open(&database)
        .unwrap()
        .with_conversation_marker(&published_view(&snapshot.state).unwrap(), || {
            lock.publish(&snapshot, snapshot.state.clone())
                .map_err(polaris_memory::Error::Io)
        })
        .unwrap();
    let snapshot = lock.snapshot().unwrap();
    drop(lock);
    (store, database, snapshot)
}

fn response(work: &StructuredSummaryWork) -> ValidatedStructuredResponse {
    let mut value = serde_json::json!({
        "summary": {"facts":[format!("filler {}", work.turn)], "decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[work.turn]},
        "processed_source_ids": work.intent.sources.iter().map(|s| &s.source_id).collect::<Vec<_>>(),
        "entity_candidates": [], "fact_deltas": []
    });
    if work.turn == 1 {
        let id = &work.intent.sources[0].source_id;
        value["summary"]["facts"] = serde_json::json!(["Aster is production"]);
        value["entity_candidates"] = serde_json::json!([{"source_id":id,"evidence_span":{"start":0,"length":5},"speech_act":"assert","ordinal":0,"kind":"product","name":"Aster","subject_project":"project","evidence_text":"Aster"}]);
        value["fact_deltas"] = serde_json::json!([{"source_id":id,"evidence_span":{"start":0,"length":5},"speech_act":"assert","ordinal":0,"entity_kind":"product","entity_name":"Aster","fact_kind":"environment","value":"production","unit":"none","condition":"","relation":"assert","evidence_text":"Aster"}]);
    }
    structured::validate_response(&work.source, work.turn, &work.intent, &value.to_string())
        .unwrap()
}

fn pending(work: &StructuredSummaryWork, response: &ValidatedStructuredResponse) -> PendingSummary {
    let evidence = summary_evidence(&work.source, work.turn, &response.summary).unwrap();
    PendingSummary {
        scope: evidence.scope,
        id: evidence.id,
        source: evidence.source,
        summary: response.summary.clone(),
        summary_hash: content_hash(response.summary.as_bytes()),
        model: "gpt-6-astra".into(),
        effort: "medium".into(),
        prompt_version: "strict10-v1".into(),
        embedding: EmbeddingMetadata {
            model: "fixture".into(),
            revision: "v1".into(),
            dimension: 2,
            input_hash: content_hash(response.summary.as_bytes()),
            values: vec![1.0, 0.0],
        },
    }
}

fn receipt(store: &ConversationStore, epoch: u64, turn: u64) -> PathBuf {
    store
        .directory()
        .join("structured-attempts")
        .join(epoch.to_string())
        .join(format!("{turn}.json"))
}

fn prepared(
    store: &ConversationStore,
    db: &Path,
    snapshot: &ConversationSnapshot,
    turn: u64,
) -> (
    StructuredSummaryWork,
    ValidatedStructuredResponse,
    PendingSummary,
) {
    let work = begin_structured_summary(store, db, snapshot, turn).unwrap();
    assert!(work.newly_started);
    let response = response(&work);
    save_structured_summary_response(store, db, snapshot, turn, &response).unwrap();
    let pending = pending(&work, &response);
    (work, response, pending)
}

#[test]
fn intent_is_durable_once_and_saved_result_resumes_without_advancing_generation() {
    let temp = tempfile::tempdir().unwrap();
    let (store, db, source) = setup(temp.path());
    let raw_before = fs::read(store.directory().join("raw.jsonl")).unwrap();
    let first = begin_structured_summary(&store, &db, &source, 1).unwrap();
    let mut sends = usize::from(first.newly_started);
    let reopened = ConversationStore::open(temp.path(), "project", ID).unwrap();
    let unknown = begin_structured_summary(&reopened, &db, &source, 1).unwrap_err();
    assert!(unknown.to_string().contains("will not be resent"));
    let response = response(&first);
    save_structured_summary_response(&reopened, &db, &source, 1, &response).unwrap();
    let saved_bytes = fs::read(receipt(&store, 0, 1)).unwrap();
    save_structured_summary_response(&reopened, &db, &source, 1, &response).unwrap();
    assert_eq!(saved_bytes, fs::read(receipt(&store, 0, 1)).unwrap());
    let resumed = begin_structured_summary(&reopened, &db, &source, 1).unwrap();
    sends += usize::from(resumed.newly_started);
    assert_eq!(sends, 1);
    assert_eq!(resumed.response.as_ref().unwrap().delta, response.delta);
    assert_eq!(
        store.lock().unwrap().snapshot().unwrap().state,
        source.state
    );
    let item = pending(&first, &response);
    publish_summary(&reopened, &db, &source, &item).unwrap();
    let published = reopened.lock().unwrap().snapshot().unwrap();
    assert_eq!(published.state.generation, source.state.generation + 1);
    assert_eq!(published.state.visible_summary_ids, vec![item.id]);
    assert_eq!(
        published.state.visible_structured_publication_ids,
        vec![response.delta.publication.publication_id.clone()]
    );
    assert_eq!(published.state.structured_coverage, Some(response.coverage));
    assert_eq!(published.recent_messages().len(), 10);
    assert_eq!(
        fs::read(store.directory().join("raw.jsonl")).unwrap(),
        raw_before
    );
    let deltas = visible_deltas(&reopened, &published.state).unwrap();
    assert_eq!(deltas, vec![response.delta]);
    MemoryStore::open(&db)
        .unwrap()
        .with_verified_structured_deltas(&deltas.iter().collect::<Vec<_>>(), || Ok(()))
        .unwrap();
    assert!(begin_structured_summary(&reopened, &db, &published, 1).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(receipt(&store, 0, 1))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            fs::metadata(receipt(&store, 0, 1).parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
}

#[test]
fn intent_failures_never_return_dispatch_and_rename_without_sync_stays_unknown() {
    for (point, has_receipt) in [
        ("snapshot", false),
        ("structured_attempt_before_write", false),
        ("atomic_file_sync", false),
        ("atomic_rename", false),
        ("atomic_directory_sync", true),
        ("structured_attempt_after_write", true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (store, db, source) = setup(temp.path());
        if point != "snapshot" {
            store.lock().unwrap().save_snapshot(&source).unwrap();
        }
        {
            let _fault = Fault::at(point);
            assert!(
                begin_structured_summary(&store, &db, &source, 1).is_err(),
                "{point}"
            );
        }
        assert_eq!(receipt(&store, 0, 1).exists(), has_receipt, "{point}");
        assert_eq!(
            store.lock().unwrap().snapshot().unwrap().state,
            source.state
        );
        let retry = begin_structured_summary(&store, &db, &source, 1);
        if has_receipt {
            assert!(
                retry
                    .unwrap_err()
                    .to_string()
                    .contains("will not be resent")
            );
        } else {
            assert!(retry.unwrap().newly_started);
        }
    }
}

#[test]
fn response_failures_resume_only_a_saved_result_and_reject_replacement() {
    for (point, saved) in [
        ("structured_attempt_before_write", false),
        ("atomic_directory_sync", true),
    ] {
        let temp = tempfile::tempdir().unwrap();
        let (store, db, source) = setup(temp.path());
        let work = begin_structured_summary(&store, &db, &source, 1).unwrap();
        let response = response(&work);
        {
            let _fault = Fault::at(point);
            assert!(save_structured_summary_response(&store, &db, &source, 1, &response).is_err());
        }
        let resumed = begin_structured_summary(&store, &db, &source, 1);
        if saved {
            let resumed = resumed.unwrap();
            assert!(!resumed.newly_started);
            assert_eq!(resumed.response.unwrap().delta, response.delta);
        } else {
            assert!(resumed.is_err());
            save_structured_summary_response(&store, &db, &source, 1, &response).unwrap();
        }
        let mut changed = response.clone();
        changed.summary.push(' ');
        assert!(save_structured_summary_response(&store, &db, &source, 1, &changed).is_err());
    }
}

#[test]
fn index_commit_marker_failure_keeps_pending_invisible_then_reuses_both_saved_results() {
    let temp = tempfile::tempdir().unwrap();
    let (store, db, source) = setup(temp.path());
    let (_, response, item) = prepared(&store, &db, &source, 1);
    let bytes = fs::read(receipt(&store, 0, 1)).unwrap();
    {
        let _fault = Fault::at("marker");
        assert!(publish_summary(&store, &db, &source, &item).is_err());
    }
    assert_eq!(
        store.lock().unwrap().snapshot().unwrap().state,
        source.state
    );
    let memory = MemoryStore::open(&db).unwrap();
    assert_eq!(
        memory.pending_summary(&item.scope, &item.id).unwrap(),
        Some(item.clone())
    );
    assert!(
        !memory
            .structured_publication_visible(
                &structured::sources::publication_view(&source.state)
                    .unwrap()
                    .scope,
                &response.delta.publication.publication_id
            )
            .unwrap()
    );
    drop(memory);
    let reopened = ConversationStore::open(temp.path(), "project", ID).unwrap();
    let work = begin_structured_summary(&reopened, &db, &source, 1).unwrap();
    assert!(!work.newly_started);
    assert_eq!(work.response.unwrap().summary, item.summary);
    publish_summary(&reopened, &db, &source, &item).unwrap();
    assert_eq!(fs::read(receipt(&store, 0, 1)).unwrap(), bytes);
    let now = store.lock().unwrap().snapshot().unwrap();
    assert_eq!(
        visible_deltas(&store, &now.state).unwrap(),
        vec![response.delta]
    );
}

#[test]
fn receipts_reject_source_coverage_prompt_delta_and_snapshot_changes() {
    let temp = tempfile::tempdir().unwrap();
    let (store, db, source) = setup(temp.path());
    let (_, response, item) = prepared(&store, &db, &source, 1);
    let path = receipt(&store, 0, 1);
    let original = fs::read(&path).unwrap();
    for pointer in [
        "/raw_hash",
        "/generation",
        "/prompt_version",
        "/turn",
        "/intent/coverage/prefix_hash",
        "/intent/sources/0/content",
        "/response/delta/publication/delta_hash",
    ] {
        let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
        *value.pointer_mut(pointer).unwrap() = if ["/generation", "/turn"].contains(&pointer) {
            serde_json::json!(99)
        } else {
            serde_json::json!("tampered")
        };
        fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
        assert!(
            begin_structured_summary(&store, &db, &source, 1).is_err(),
            "{pointer}"
        );
        assert!(
            save_structured_summary_response(&store, &db, &source, 1, &response).is_err(),
            "{pointer}"
        );
        assert!(
            publish_summary(&store, &db, &source, &item).is_err(),
            "{pointer}"
        );
    }
    fs::write(&path, &original).unwrap();
    let archive = store.directory().join("snapshots").join(format!(
        "{}-{}.json",
        source.state.generation, source.raw_hash
    ));
    let original_archive = fs::read(&archive).unwrap();
    fs::write(&archive, b"{}").unwrap();
    assert!(begin_structured_summary(&store, &db, &source, 1).is_err());
    assert!(publish_summary(&store, &db, &source, &item).is_err());
    fs::write(&archive, original_archive).unwrap();
    assert!(
        !begin_structured_summary(&store, &db, &source, 1)
            .unwrap()
            .newly_started
    );
}

#[test]
fn stale_raw_clear_forget_and_foreign_owner_do_not_dispatch_or_publish() {
    let temp = tempfile::tempdir().unwrap();
    let (store, db, source) = setup(temp.path());
    let (_, response, item) = prepared(&store, &db, &source, 1);
    assert!(ConversationStore::open(temp.path(), "another-project", ID).is_err());
    let mut lock = store.lock().unwrap();
    let changed = lock.append(Message::user("next input"), true).unwrap();
    drop(lock);
    assert!(begin_structured_summary(&store, &db, &changed, 1).is_err());
    assert!(save_structured_summary_response(&store, &db, &source, 1, &response).is_err());
    assert!(publish_summary(&store, &db, &source, &item).is_err());
    store.lock().unwrap().clear_epoch().unwrap();
    let cleared = store.lock().unwrap().snapshot().unwrap();
    assert_eq!(cleared.state.epoch, 1);
    assert!(begin_structured_summary(&store, &db, &cleared, 1).is_err());
    assert!(receipt(&store, 0, 1).exists());
    assert!(!receipt(&store, 1, 1).exists());

    let other = tempfile::tempdir().unwrap();
    let (store, db, source) = setup(other.path());
    MemoryStore::open(&db)
        .unwrap()
        .delete_session("project", ID)
        .unwrap();
    assert!(begin_structured_summary(&store, &db, &source, 1).is_err());
    assert!(!receipt(&store, 0, 1).exists());
}

#[test]
fn oversize_and_symlink_receipts_stop_without_replacing_raw() {
    let temp = tempfile::tempdir().unwrap();
    let (store, db, source) = setup(temp.path());
    let _ = prepared(&store, &db, &source, 1);
    let path = receipt(&store, 0, 1);
    let original = fs::read(&path).unwrap();
    fs::OpenOptions::new()
        .write(true)
        .open(&path)
        .unwrap()
        .set_len(1024 * 1024 + 1)
        .unwrap();
    assert!(begin_structured_summary(&store, &db, &source, 1).is_err());
    fs::write(&path, &original).unwrap();
    #[cfg(unix)]
    {
        fs::remove_file(&path).unwrap();
        std::os::unix::fs::symlink(store.directory().join("raw.jsonl"), &path).unwrap();
        assert!(begin_structured_summary(&store, &db, &source, 1).is_err());
        fs::remove_file(&path).unwrap();
        fs::write(&path, &original).unwrap();
    }
    assert_eq!(
        store.lock().unwrap().snapshot().unwrap().raw_hash,
        source.raw_hash
    );
    assert!(
        !begin_structured_summary(&store, &db, &source, 1)
            .unwrap()
            .newly_started
    );
}

#[test]
fn actual_fork_reads_only_frozen_parent_receipts_and_later_ids_are_not_added() {
    let temp = tempfile::tempdir().unwrap();
    let (parent, db, source) = setup(temp.path());
    let (_, first_response, item) = prepared(&parent, &db, &source, 1);
    publish_summary(&parent, &db, &source, &item).unwrap();
    let child = parent
        .lock()
        .unwrap()
        .fork(temp.path(), CHILD, &content_hash(b"artifact"))
        .unwrap();
    let child_snapshot = child.lock().unwrap().snapshot().unwrap();
    assert_eq!(
        visible_deltas(&child, &child_snapshot.state).unwrap(),
        vec![first_response.delta.clone()]
    );
    assert!(child_snapshot.state.structured_coverage.is_none());
    let mut lock = parent.lock().unwrap();
    let appended = lock.append(Message::user("filler 12"), true).unwrap();
    lock.publish(&appended, appended.state.clone()).unwrap();
    let next = lock.snapshot().unwrap();
    drop(lock);
    let (_, later, item) = prepared(&parent, &db, &next, 2);
    publish_summary(&parent, &db, &next, &item).unwrap();
    let current_parent = parent.lock().unwrap().snapshot().unwrap();
    assert_eq!(
        visible_deltas(&parent, &current_parent.state)
            .unwrap()
            .len(),
        2
    );
    let frozen = visible_deltas(&child, &child_snapshot.state).unwrap();
    assert_eq!(frozen, vec![first_response.delta]);
    assert!(
        !frozen
            .iter()
            .any(|d| d.publication.publication_id == later.delta.publication.publication_id)
    );
    assert!(
        parent
            .structured_publication_receipt(
                0,
                child_snapshot.state.ancestors[0].generation,
                &later.delta.publication.publication_id
            )
            .is_err()
    );
    assert_eq!(
        child.lock().unwrap().snapshot().unwrap().state,
        child_snapshot.state
    );
}

fn fork_ready(
    root: &Path,
) -> (
    ConversationStore,
    ConversationStore,
    PathBuf,
    ConversationSnapshot,
    StructuredDelta,
) {
    let (parent, db, source) = setup(root);
    let (_, response, item) = prepared(&parent, &db, &source, 1);
    publish_summary(&parent, &db, &source, &item).unwrap();
    let child = parent
        .lock()
        .unwrap()
        .fork(root, CHILD, &content_hash(b"artifact"))
        .unwrap();
    let mut lock = child.lock().unwrap();
    let snapshot = lock
        .append(Message::user("Aster is staging"), true)
        .unwrap();
    lock.publish(&snapshot, snapshot.state.clone()).unwrap();
    let snapshot = lock.snapshot().unwrap();
    drop(lock);
    (parent, child, db, snapshot, response.delta)
}

#[test]
fn r30_child_overlay_publishes_with_exact_parent_receipts_and_parent_is_unchanged() {
    let temp = tempfile::tempdir().unwrap();
    let (parent, child, db, source, parent_delta) = fork_ready(temp.path());
    let parent_before = fs::read(parent.directory().join("state.json")).unwrap();
    let work = begin_structured_summary(&child, &db, &source, 1).unwrap();
    let id = &work
        .intent
        .sources
        .iter()
        .find(|s| s.content == "Aster is staging")
        .unwrap()
        .source_id;
    let body = serde_json::json!({
        "summary":{"facts":["filler 2"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[1]},
        "processed_source_ids":work.intent.sources.iter().map(|s| &s.source_id).collect::<Vec<_>>(),
        "entity_candidates":[],
        "fact_deltas":[{"source_id":id,"evidence_span":{"start":0,"length":5},"speech_act":"correct","ordinal":0,"entity_kind":"product","entity_name":"Aster","fact_kind":"environment","value":"staging","unit":"none","condition":"","relation":"replace","evidence_text":"Aster"}]
    });
    let memory = MemoryStore::open(&db).unwrap();
    let view = structured::sources::publication_view(&source.state).unwrap();
    let response = structured::validate_response_with_resolver(
        &source,
        1,
        &work.intent,
        &body.to_string(),
        &structured::PublishedResolver {
            memory: &memory,
            view: &view,
        },
    )
    .unwrap();
    assert!(
        response
            .delta
            .facts
            .iter()
            .all(|f| f.entity_id == parent_delta.entities[0].entity_id)
    );
    assert!(!response.delta.facts.is_empty());
    drop(memory);
    save_structured_summary_response(&child, &db, &source, 1, &response).unwrap();
    publish_summary(&child, &db, &source, &pending(&work, &response)).unwrap();
    let current = child.lock().unwrap().snapshot().unwrap();
    let deltas = visible_deltas(&child, &current.state).unwrap();
    assert_eq!(deltas.len(), 2);
    let memory = MemoryStore::open(&db).unwrap();
    let child_view = structured::sources::publication_view(&current.state).unwrap();
    let fact = memory
        .structured_fact_revision(&child_view, &parent_delta.facts[0].fact_id)
        .unwrap()
        .unwrap();
    assert_eq!(
        fact.status,
        polaris_memory::structured::FactStatus::Superseded
    );
    assert!(
        memory
            .structured_fact_candidates(
                &child_view,
                &parent_delta.entities[0].entity_id,
                "environment",
                ""
            )
            .unwrap()
            .iter()
            .any(|f| f.value_json == "\"staging\""
                && f.status == polaris_memory::structured::FactStatus::Active)
    );
    assert_eq!(
        fs::read(parent.directory().join("state.json")).unwrap(),
        parent_before
    );
    let parent_view =
        structured::sources::publication_view(&parent.lock().unwrap().snapshot().unwrap().state)
            .unwrap();
    assert_eq!(
        memory
            .structured_fact_revision(&parent_view, &parent_delta.facts[0].fact_id)
            .unwrap()
            .unwrap()
            .status,
        polaris_memory::structured::FactStatus::Active
    );
}

#[test]
fn r30_forgotten_parent_stops_child_before_a_new_intent() {
    let temp = tempfile::tempdir().unwrap();
    let (parent, child, db, source, _) = fork_ready(temp.path());
    MemoryStore::open(&db)
        .unwrap()
        .delete_session("project", ID)
        .unwrap();
    // A filesystem-first implementation would fail with NotFound instead.
    fs::rename(parent.directory(), temp.path().join("held-parent")).unwrap();
    let error = begin_structured_summary(&child, &db, &source, 1).unwrap_err();
    assert!(error.to_string().contains("forgotten"), "{error}");
    assert!(!receipt(&child, 0, 1).exists());
}

#[test]
fn forgotten_parent_after_response_stops_child_publication() {
    let temp = tempfile::tempdir().unwrap();
    let (parent, child, db, source, _) = fork_ready(temp.path());
    let work = begin_structured_summary(&child, &db, &source, 1).unwrap();
    let body = serde_json::json!({
        "summary":{"facts":["filler 2"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[1]},
        "processed_source_ids":work.intent.sources.iter().map(|s| &s.source_id).collect::<Vec<_>>(),
        "entity_candidates":[],"fact_deltas":[]
    });
    let response =
        structured::validate_response(&source, 1, &work.intent, &body.to_string()).unwrap();
    save_structured_summary_response(&child, &db, &source, 1, &response).unwrap();
    MemoryStore::open(&db)
        .unwrap()
        .delete_session("project", ID)
        .unwrap();
    fs::rename(parent.directory(), temp.path().join("held-parent")).unwrap();
    let error = publish_summary(&child, &db, &source, &pending(&work, &response)).unwrap_err();
    assert!(error.to_string().contains("forgotten"), "{error}");
    assert_eq!(
        child.lock().unwrap().snapshot().unwrap().state,
        source.state
    );
    assert!(begin_structured_summary(&child, &db, &source, 1).is_err());
}

#[test]
fn missing_index_and_resealed_receipt_stop_the_next_intent() {
    for missing_index in [false, true] {
        let temp = tempfile::tempdir().unwrap();
        let (store, db, source) = setup(temp.path());
        let (_, response, item) = prepared(&store, &db, &source, 1);
        publish_summary(&store, &db, &source, &item).unwrap();
        let published = store.lock().unwrap().snapshot().unwrap();
        MemoryStore::open(&db)
            .unwrap()
            .with_verified_structured_receipts(
                &structured::sources::publication_view(&published.state).unwrap(),
                &mut |scope, id| read_receipt_delta(&store, &published.state, scope, id),
                || Ok(()),
            )
            .unwrap();
        let mut lock = store.lock().unwrap();
        let appended = lock.append(Message::user("filler 12"), true).unwrap();
        lock.publish(&appended, appended.state.clone()).unwrap();
        let next = lock.snapshot().unwrap();
        drop(lock);
        if missing_index {
            fs::rename(&db, temp.path().join("original-memory.sqlite3")).unwrap();
            // Keep the v2 summary marker satisfiable, isolating missing v3 rows.
            MemoryStore::open(&db)
                .unwrap()
                .insert_pending_summary(&item)
                .unwrap();
        } else {
            let mut changed = response.delta;
            changed.entity_revisions[0].description = "resealed but not published".into();
            changed.publication.delta_hash =
                polaris_memory::structured::delta_hash(&changed).unwrap();
            let path = receipt(&store, 0, 1);
            let mut value: serde_json::Value =
                serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
            value["response"]["delta"] = serde_json::to_value(changed).unwrap();
            fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
        }
        assert!(begin_structured_summary(&store, &db, &next, 2).is_err());
        assert!(!receipt(&store, 0, 2).exists());
        assert_eq!(store.lock().unwrap().snapshot().unwrap().state, next.state);
    }
}
