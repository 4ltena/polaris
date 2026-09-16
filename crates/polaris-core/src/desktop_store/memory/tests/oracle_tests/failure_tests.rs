//! Fixed-oracle absence, source integrity, and interrupted withdrawal publication.
//! File execution and model classification are separate acceptance lanes.
use super::*;
use polaris_desktop_protocol::snapshot::MemoryRetrievalStatus;

fn case<'a>(fixture: &'a Value, id: &str) -> &'a Value {
    fixture["negative_cases"]
        .as_array()
        .unwrap()
        .iter()
        .find(|case| case["id"] == id)
        .unwrap()
}

fn backends(fixture: &Value) -> (Arc<AnnotatedSummary>, Arc<CountedEmbed>) {
    (
        Arc::new(AnnotatedSummary {
            fixture: fixture.clone(),
            calls: AtomicUsize::new(0),
        }),
        Arc::new(CountedEmbed {
            model: EmbeddingModel {
                model: "fixture".into(),
                revision: "fixed".into(),
                dimension: 2,
            },
            passages: AtomicUsize::new(0),
        }),
    )
}

fn seed(writer: &mut Writer, fixture: &Value, authored: bool) {
    for turn in 1..=10 {
        let text = if authored && turn <= 5 {
            fixture["inputs"][(turn - 1) as usize]["text"]
                .as_str()
                .unwrap()
                .to_owned()
        } else {
            fixture["filler"]["user_template"]
                .as_str()
                .unwrap()
                .replace("{turn}", &turn.to_string())
        };
        let target = accept_text(writer, turn, &text);
        finish(writer, &target, turn);
    }
}

fn assert_withdrawal(
    writer: &Writer,
    target: &RunTarget,
    database: &std::path::Path,
    fixture: &Value,
    text: &str,
) {
    assert_eq!(
        writer
            .snapshot()
            .unwrap()
            .state
            .runs
            .last()
            .unwrap()
            .memory
            .as_ref()
            .unwrap()
            .retrieval_status,
        Some(MemoryRetrievalStatus::PendingReconfirmation)
    );
    verify_authored_expectations(writer, target, database, fixture);
    verify_bundles(writer, target, database, text, fixture);
}

fn assert_absent(writer: &Writer, target: &RunTarget, database: &std::path::Path, text: &str) {
    let snapshot = writer.memory_snapshot(target).unwrap();
    let context = writer
        .prepare_structured_memory(target, &snapshot, database)
        .unwrap();
    assert_eq!(context.status, MemoryRetrievalStatus::NotFound);
    assert!(context.text.is_empty() && context.source_uris.is_empty());
    let saved = writer.snapshot().unwrap();
    let status = saved.state.runs.last().unwrap().memory.as_ref().unwrap();
    assert_eq!(
        status.retrieval_status,
        Some(MemoryRetrievalStatus::NotFound)
    );
    assert!(text.contains("not_found"));
    assert!(!text.lines().any(|line| line.starts_with('{')));
    for name in ["Lumen", "Aster", "sync.retry_limit"] {
        assert!(!text.contains(name));
    }
}

#[tokio::test]
async fn authored_oracle_unknown_key_returns_no_unrelated_evidence_after_positive_control() {
    let fixture = fixture();
    let case = case(&fixture, "unknown-key");
    assert_eq!(case["expect"], "not_found");
    let (_root, mut writer) = new_writer();
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let (summary, embed) = backends(&fixture);
    seed(&mut writer, &fixture, true);
    let known = accept_text(&mut writer, 11, fixture["query"].as_str().unwrap());
    writer.bind_memory(&known, resources()).unwrap();
    let (_, evidence) = prepare(
        &mut writer,
        &known,
        &database,
        summary.clone(),
        embed.clone(),
    )
    .await;
    assert_withdrawal(
        &writer,
        &known,
        &database,
        &fixture,
        &evidence.unwrap().content,
    );
    finish(&mut writer, &known, 11);
    let target = accept_text(&mut writer, 12, case["query"].as_str().unwrap());
    writer.bind_memory(&target, resources()).unwrap();
    let before = writer.snapshot().unwrap().marker.raw_hash;
    let (_, evidence) = prepare(
        &mut writer,
        &target,
        &database,
        summary.clone(),
        embed.clone(),
    )
    .await;
    assert_absent(&writer, &target, &database, &evidence.unwrap().content);
    assert_eq!(writer.snapshot().unwrap().marker.raw_hash, before);
    // Absence must not be achieved by deleting the original decisions.
    verify_authored_expectations(&writer, &target, &database, &fixture);
    assert_eq!(summary.calls.load(Ordering::SeqCst), 2);
    assert_eq!(embed.passages.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn authored_oracle_unrelated_session_and_project_cannot_read_shared_index() {
    let fixture = fixture();
    let unrelated = case(&fixture, "unrelated-session");
    assert_eq!(unrelated["published_ancestor_ids"], json!([]));
    assert_eq!(unrelated["expect"], "not_found");
    let foreign = case(&fixture, "cross-project");
    assert_eq!(foreign["expect"], "no_project_A_sources");
    let (_root, mut writer) = new_writer();
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let (summary, embed) = backends(&fixture);
    seed(&mut writer, &fixture, true);
    let target = accept_text(&mut writer, 11, fixture["query"].as_str().unwrap());
    writer.bind_memory(&target, resources()).unwrap();
    let (_, evidence) = prepare(
        &mut writer,
        &target,
        &database,
        summary.clone(),
        embed.clone(),
    )
    .await;
    assert_withdrawal(
        &writer,
        &target,
        &database,
        &fixture,
        &evidence.unwrap().content,
    );
    let original = writer.memory_snapshot(&target).unwrap();
    let context = writer
        .prepare_structured_memory(&target, &original, &database)
        .unwrap();
    assert!(!context.source_uris.is_empty());
    for (project, session) in [
        ("project-A", "unrelated-session"),
        ("project-B", "session-B"),
    ] {
        let (_other_root, mut other) = new_writer_named(project, session);
        seed(&mut other, &fixture, false);
        let other_target = accept_text(&mut other, 11, fixture["query"].as_str().unwrap());
        other.bind_memory(&other_target, resources()).unwrap();
        let before = other.snapshot().unwrap().marker.raw_hash;
        let (_, evidence) = prepare(
            &mut other,
            &other_target,
            &database,
            summary.clone(),
            embed.clone(),
        )
        .await;
        assert_absent(&other, &other_target, &database, &evidence.unwrap().content);
        let snapshot = other.memory_snapshot(&other_target).unwrap();
        assert!(snapshot.conversation.state.ancestors.is_empty());
        assert_eq!(snapshot.conversation.raw_hash, before);
        if project == "project-B" {
            assert_eq!(
                format!(
                    "{project}/{session}/epoch-{}",
                    snapshot.conversation.state.epoch
                ),
                foreign["storage_scope"]
            );
        }
        for uri in &context.source_uris {
            assert!(
                other
                    .read_memory_source(&other_target, &database, uri)
                    .is_err()
            );
        }
    }
    // A shared DB was used throughout: rejection is scope-bound, not an empty index.
    assert_eq!(
        writer
            .prepare_structured_memory(&target, &original, &database)
            .unwrap()
            .text,
        context.text
    );
    for uri in &context.source_uris {
        assert!(writer.read_memory_source(&target, &database, uri).is_ok());
    }
    assert_eq!(summary.calls.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn authored_oracle_source_hash_mismatch_rejects_values_and_originals() {
    let fixture = fixture();
    assert_eq!(
        case(&fixture, "source-hash-mismatch")["expect"],
        "source_integrity_error_no_value_fallback"
    );
    let (_root, mut writer) = new_writer();
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let (summary, embed) = backends(&fixture);
    seed(&mut writer, &fixture, true);
    let target = accept_text(&mut writer, 11, fixture["query"].as_str().unwrap());
    writer.bind_memory(&target, resources()).unwrap();
    let (_, evidence) = prepare(
        &mut writer,
        &target,
        &database,
        summary.clone(),
        embed.clone(),
    )
    .await;
    assert_withdrawal(
        &writer,
        &target,
        &database,
        &fixture,
        &evidence.unwrap().content,
    );
    let snapshot = writer.memory_snapshot(&target).unwrap();
    let context = writer
        .prepare_structured_memory(&target, &snapshot, &database)
        .unwrap();
    let path = writer.test_directory().join("raw.jsonl");
    let bytes = std::fs::read(&path).unwrap();
    let mut changed = bytes.clone();
    let offset = changed
        .windows(b"Lumen".len())
        .position(|p| p == b"Lumen")
        .unwrap();
    changed[offset] = b'X';
    assert_eq!(changed.len(), bytes.len());
    assert_ne!(content_hash(&changed), snapshot.conversation.raw_hash);
    std::fs::write(&path, &changed).unwrap();
    let rejected = writer.prepare_structured_memory(&target, &snapshot, &database);
    let rejected_sources: Vec<_> = context
        .source_uris
        .iter()
        .map(|uri| writer.read_memory_source(&target, &database, uri).is_err())
        .collect();
    // Restore the isolated source before assertions; no user/project source is touched.
    std::fs::write(&path, &bytes).unwrap();
    assert!(rejected.is_err());
    assert!(!rejected_sources.is_empty() && rejected_sources.iter().all(|rejected| *rejected));
    assert_eq!(
        writer
            .prepare_structured_memory(&target, &snapshot, &database)
            .unwrap()
            .text,
        context.text
    );
    assert_eq!(
        writer.snapshot().unwrap().marker.raw_hash,
        snapshot.conversation.raw_hash
    );
    assert_eq!(summary.calls.load(Ordering::SeqCst), 1);
    assert_eq!(embed.passages.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn authored_oracle_withdrawal_publication_interruption_never_restores_current_three() {
    let fixture = fixture();
    assert_eq!(
        case(&fixture, "withdrawal-publication-interrupted")["expect"],
        "consistent_snapshot_no_current_3_after_durable_T5"
    );
    for fault in ["marker_sync", "marker_rename"] {
        let (root, mut writer) = new_writer();
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("memory.sqlite3");
        let (summary, embed) = backends(&fixture);
        seed(&mut writer, &fixture, true);
        let target = accept_text(&mut writer, 11, fixture["query"].as_str().unwrap());
        writer.bind_memory(&target, resources()).unwrap();
        let snapshot = writer.memory_snapshot(&target).unwrap();
        assert!(snapshot.conversation.events.iter().any(|e| e.turn_id == 5
            && e.starts_turn
            && e.message.content == fixture["inputs"][4]["text"]));
        let work = writer.begin_summary(&target, &snapshot, 1).unwrap();
        let history = StrictHistory::new(summary.clone(), embed.clone());
        let response = history
            .summarize_structured_turn(
                &work.source,
                1,
                work.structured_intent.as_ref().unwrap(),
                &database,
                &work.expected.structured_view().unwrap(),
            )
            .await
            .unwrap();
        writer
            .save_structured_response(&target, &work, &response)
            .unwrap();
        let pending = history
            .embed_summary(&work.source, 1, response.summary.clone())
            .await
            .unwrap();
        let saved = SavedStructuredResponse {
            summary: response.summary,
            delta: response.delta,
            coverage: response.coverage,
        };
        let saved_memory = writer
            .snapshot()
            .unwrap()
            .state
            .conversation_memory
            .unwrap();
        crate::desktop_store::disk::FAIL.with(|flag| flag.set(Some(fault)));
        let result = writer.publish_structured_memory(&target, &work, &pending, &saved, &database);
        crate::desktop_store::disk::FAIL.with(|flag| flag.set(None));
        assert!(result.is_err());
        writer.recover().unwrap();
        let recovered = writer.memory_snapshot(&target).unwrap();
        assert_eq!(
            recovered.conversation.raw_hash,
            snapshot.conversation.raw_hash
        );
        assert_eq!(
            writer
                .snapshot()
                .unwrap()
                .state
                .conversation_memory
                .as_ref(),
            Some(&saved_memory)
        );
        assert!(
            recovered
                .conversation
                .state
                .visible_structured_publication_ids
                .is_empty()
        );
        let blocked = writer
            .prepare_structured_memory(&target, &recovered, &database)
            .unwrap();
        assert_eq!(blocked.status, MemoryRetrievalStatus::ExtractionIncomplete);
        assert!(blocked.text.is_empty() && blocked.source_uris.is_empty());
        let db = MemoryStore::open(&database).unwrap();
        // SQLite rows exist but cannot authorize retrieval before the owner marker.
        assert_eq!(
            db.structured_publication(
                &saved.delta.publication.scope,
                &saved.delta.publication.publication_id
            )
            .unwrap(),
            Some(saved.delta.publication.clone())
        );
        db.verify_structured_delta(&saved.delta).unwrap();
        assert_eq!(
            db.pending_summary(&pending.scope, &pending.id).unwrap(),
            Some(pending.clone())
        );
        drop(db);
        drop(writer);
        writer = root
            .open(
                &ProjectId::new("project-A").unwrap(),
                &SessionId::new("session-A").unwrap(),
            )
            .unwrap();
        assert_eq!(
            writer
                .snapshot()
                .unwrap()
                .state
                .conversation_memory
                .as_ref(),
            Some(&saved_memory)
        );
        let next = accept_text(&mut writer, 12, fixture["query"].as_str().unwrap());
        writer.bind_memory(&next, resources()).unwrap();
        let before = writer.snapshot().unwrap().marker.raw_hash;
        let resumed = writer
            .begin_summary(&next, &writer.memory_snapshot(&next).unwrap(), 1)
            .unwrap();
        assert!(!resumed.newly_started);
        assert_eq!(resumed.structured_response.as_ref(), Some(&saved));
        let (_, evidence) = prepare(
            &mut writer,
            &next,
            &database,
            summary.clone(),
            embed.clone(),
        )
        .await;
        assert_withdrawal(
            &writer,
            &next,
            &database,
            &fixture,
            &evidence.unwrap().content,
        );
        assert_eq!(writer.snapshot().unwrap().marker.raw_hash, before);
        // Only turn 2 is new; both the saved turn 1 response and embedding are reused.
        assert_eq!(summary.calls.load(Ordering::SeqCst), 2);
        assert_eq!(embed.passages.load(Ordering::SeqCst), 2);
        println!(
            "authored_oracle_withdrawal fault={fault} saved_response_reused=true semantic_classification_measured=false"
        );
    }
}
