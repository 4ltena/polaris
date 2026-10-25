//! Synthetic provider calls through the real shared history and v2 owner.
use super::*;
use crate::conversation_state::{ConversationStateV2, HistoryMode};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

const ID: &str = "00000000-0000-4000-8000-000000000031";

struct Summary {
    calls: AtomicUsize,
    fail: AtomicBool,
    /// Written when a request arrives, which then never answers.
    hold: Option<PathBuf>,
}
impl StrictSummaryProvider for Summary {
    fn summarize<'a>(&'a self, request: SummaryRequest) -> StrictFuture<'a, String> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if let Some(marker) = &self.hold {
                std::fs::write(marker, b"summary")?;
                std::future::pending::<()>().await;
            }
            assert_eq!(request.model, SUMMARY_MODEL);
            assert_eq!(request.effort, SUMMARY_EFFORT);
            let input = request
                .structured_sources
                .ok_or_else(|| io::Error::other("structured envelope was not requested"))?;
            if self.fail.load(Ordering::SeqCst) {
                return Err(io::Error::other("fixture provider failure"));
            }
            let mut response = serde_json::json!({
                "summary": {"facts":["saved fact"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[request.source_turn_id]},
                "processed_source_ids": &input.extraction_source_ids,
                "entity_candidates": [], "fact_deltas": []
            });
            if let Some(source) = input.sources.iter().find(|source| {
                source.content == "Aster is production"
                    && input.extraction_source_ids.contains(&source.source_id)
            }) {
                response["entity_candidates"] = serde_json::json!([{"source_id":source.source_id,"evidence_span":{"start":0,"length":5},"speech_act":"assert","ordinal":0,"kind":"product","name":"Aster","subject_project":"p","evidence_text":"Aster"}]);
                response["fact_deltas"] = serde_json::json!([{"source_id":source.source_id,"evidence_span":{"start":0,"length":5},"speech_act":"assert","ordinal":0,"entity_kind":"product","entity_name":"Aster","fact_kind":"environment","value":"production","unit":"none","condition":"","relation":"assert","evidence_text":"Aster"}]);
            }
            Ok(response.to_string())
        })
    }
}

struct Embedder {
    model: EmbeddingModel,
    passages: AtomicUsize,
    queries: AtomicUsize,
    fail: AtomicBool,
    hold: Option<PathBuf>,
}
impl StrictEmbedder for Embedder {
    fn metadata(&self) -> &EmbeddingModel {
        &self.model
    }
    fn embed_passage<'a>(&'a self, _: &'a str) -> StrictFuture<'a, Vec<f32>> {
        Box::pin(async move {
            self.passages.fetch_add(1, Ordering::SeqCst);
            if let Some(marker) = &self.hold {
                std::fs::write(marker, b"passage")?;
                std::future::pending::<()>().await;
            }
            if self.fail.load(Ordering::SeqCst) {
                return Err(io::Error::other("fixture embedding failure"));
            }
            Ok(vec![1.0, 0.0])
        })
    }
    fn embed_query<'a>(&'a self, _: &'a str) -> StrictFuture<'a, Vec<Vec<f32>>> {
        Box::pin(async move {
            self.queries.fetch_add(1, Ordering::SeqCst);
            Ok(vec![vec![1.0, 0.0]])
        })
    }
}

fn backend() -> (StrictHistory, Arc<Summary>, Arc<Embedder>) {
    backend_holding(None, None)
}

fn backend_holding(
    summary: Option<PathBuf>,
    passage: Option<PathBuf>,
) -> (StrictHistory, Arc<Summary>, Arc<Embedder>) {
    let summary = Arc::new(Summary {
        calls: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
        hold: summary,
    });
    let embedder = Arc::new(Embedder {
        model: EmbeddingModel {
            model: "fixture".into(),
            revision: "r31".into(),
            dimension: 2,
        },
        passages: AtomicUsize::new(0),
        queries: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
        hold: passage,
    });
    (
        StrictHistory::new(summary.clone(), embedder.clone()),
        summary,
        embedder,
    )
}

fn setup(turns: u64) -> (tempfile::TempDir, ConversationStore, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let mut state = ConversationStateV2::new("p".into(), ID.into()).unwrap();
    state.history_mode = HistoryMode::Structured10;
    let store = ConversationStore::create(root.path(), state).unwrap();
    let database = root.path().join("memory.sqlite3");
    let mut lock = store.lock().unwrap();
    for turn in 1..=turns {
        lock.append(
            Message::user(if turn == 1 {
                "Aster is production".into()
            } else {
                format!("filler {turn}")
            }),
            true,
        )
        .unwrap();
    }
    let raw = lock.snapshot().unwrap();
    MemoryStore::open(&database)
        .unwrap()
        .with_conversation_marker(&published_view(&raw.state).unwrap(), || {
            lock.publish(&raw, raw.state.clone())
                .map_err(polaris_memory::Error::Io)
        })
        .unwrap();
    drop(lock);
    (root, store, database)
}

#[tokio::test]
async fn r31_saved_response_survives_embedding_failure_and_reopen_without_resend() {
    let (root, store, db) = setup(11);
    let (history, summary, embedding) = backend();
    let before = store.lock().unwrap().snapshot().unwrap();
    embedding.fail.store(true, Ordering::SeqCst);
    let error = history.recover_pending(&store, &db).await.unwrap_err();
    assert!(
        error.to_string().contains("fixture embedding failure"),
        "{error}"
    );
    assert_eq!(summary.calls.load(Ordering::SeqCst), 1);
    assert_eq!(embedding.passages.load(Ordering::SeqCst), 1);
    assert_eq!(
        store.lock().unwrap().snapshot().unwrap().state,
        before.state
    );
    assert!(
        store
            .bound_structured_attempt(&before, 1)
            .unwrap()
            .unwrap()
            .response
            .is_some()
    );
    let reopened = ConversationStore::open(root.path(), "p", ID).unwrap();
    embedding.fail.store(false, Ordering::SeqCst);
    let recovered = history.recover_pending(&reopened, &db).await.unwrap();
    assert_eq!(summary.calls.load(Ordering::SeqCst), 1);
    assert_eq!(embedding.passages.load(Ordering::SeqCst), 2);
    assert_eq!(
        recovered.snapshot.state.generation,
        before.state.generation + 1
    );
    assert_eq!(
        recovered
            .snapshot
            .state
            .visible_structured_publication_ids
            .len(),
        1
    );
    assert_eq!(recovered.snapshot.raw_hash, before.raw_hash);
    history.recover_pending(&reopened, &db).await.unwrap();
    assert_eq!(summary.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn r31_unknown_provider_result_stops_reopen_before_another_call() {
    let (root, store, db) = setup(11);
    let (history, summary, embedding) = backend();
    summary.fail.store(true, Ordering::SeqCst);
    assert!(history.recover_pending(&store, &db).await.is_err());
    let reopened = ConversationStore::open(root.path(), "p", ID).unwrap();
    let error = history.recover_pending(&reopened, &db).await.unwrap_err();
    assert!(error.to_string().contains("will not be resent"), "{error}");
    assert_eq!(summary.calls.load(Ordering::SeqCst), 1);
    assert_eq!(embedding.passages.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn first_ten_turns_use_raw_without_summary_or_query_embedding() {
    let (_root, store, db) = setup(10);
    let (history, summary, embedding) = backend();
    let before = store.lock().unwrap().snapshot().unwrap();
    let prepared = history
        .prepare(&store, &db, "Aster environment")
        .await
        .unwrap();
    let context = prepared.structured.unwrap();
    assert_eq!(
        context.status,
        crate::conversation_memory::structured::packing::RetrievalStatus::NotFound
    );
    assert!(context.source_uris.is_empty());
    assert!(prepared.retrieval.is_empty());
    assert_eq!(prepared.messages.len(), 10);
    assert_eq!(prepared.snapshot.state, before.state);
    assert_eq!(summary.calls.load(Ordering::SeqCst), 0);
    assert_eq!(embedding.passages.load(Ordering::SeqCst), 0);
    assert_eq!(embedding.queries.load(Ordering::SeqCst), 0);
    assert!(!store.directory().join("structured-attempts").exists());
}

#[tokio::test]
async fn structured_preparation_binds_original_source_and_refuses_a_changed_receipt() {
    let (_root, store, db) = setup(11);
    let (history, summary, embedding) = backend();
    let prepared = history
        .prepare(&store, &db, "Aster environment")
        .await
        .unwrap();
    assert!(prepared.retrieval.is_empty());
    assert_eq!(prepared.messages.len(), 10);
    let context = prepared.structured.unwrap();
    assert_eq!(
        context.status,
        crate::conversation_memory::structured::packing::RetrievalStatus::Found
    );
    assert!(context.text.contains("production"));
    assert_eq!(context.source_uris.len(), 1);
    let (request, tokens) = context.request_text().unwrap();
    assert_eq!(crate::budget::count_tokens(&request), tokens);
    assert!(tokens <= 768);
    let original =
        crate::conversation_memory::resolve_conversation_uri(&store, &db, &context.source_uris[0])
            .unwrap();
    assert!(original.text.contains("Aster is production"));
    assert_eq!(summary.calls.load(Ordering::SeqCst), 1);
    assert_eq!(embedding.passages.load(Ordering::SeqCst), 1);
    assert_eq!(embedding.queries.load(Ordering::SeqCst), 0);
    let path = store.directory().join("structured-attempts/0/1.json");
    let bytes = std::fs::read(&path).unwrap();
    let mut changed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    changed["prompt_version"] = "other-version".into();
    std::fs::write(&path, serde_json::to_vec(&changed).unwrap()).unwrap();
    assert!(
        history
            .prepare(&store, &db, "unknown identifier")
            .await
            .is_err()
    );
    std::fs::write(&path, bytes).unwrap();
    assert!(
        history
            .prepare(&store, &db, "Aster environment")
            .await
            .is_ok()
    );
    assert_eq!(summary.calls.load(Ordering::SeqCst), 1);
}

struct Fault(Option<&'static str>);
impl Fault {
    fn at(point: &'static str) -> Self {
        Self(crate::conversation_state::FAIL_POINT.with(|value| value.replace(Some(point))))
    }
}
impl Drop for Fault {
    fn drop(&mut self) {
        crate::conversation_state::FAIL_POINT.with(|value| value.set(self.0));
    }
}

#[tokio::test]
async fn marker_failure_reuses_saved_response_and_pending_embedding() {
    let (_root, store, db) = setup(11);
    let (history, summary, embedding) = backend();
    let before = store.lock().unwrap().snapshot().unwrap();
    {
        let _fault = Fault::at("marker");
        assert!(history.recover_pending(&store, &db).await.is_err());
    }
    assert_eq!(
        store.lock().unwrap().snapshot().unwrap().state,
        before.state
    );
    assert_eq!(summary.calls.load(Ordering::SeqCst), 1);
    assert_eq!(embedding.passages.load(Ordering::SeqCst), 1);
    let other_embedding = Arc::new(Embedder {
        model: EmbeddingModel {
            model: "fixture".into(),
            revision: "different-revision".into(),
            dimension: 2,
        },
        passages: AtomicUsize::new(0),
        queries: AtomicUsize::new(0),
        fail: AtomicBool::new(false),
        hold: None,
    });
    let other = StrictHistory::new(summary.clone(), other_embedding.clone());
    let error = other.recover_pending(&store, &db).await.unwrap_err();
    assert!(error.to_string().contains("current resources"));
    assert_eq!(other_embedding.passages.load(Ordering::SeqCst), 0);
    assert_eq!(summary.calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        store.lock().unwrap().snapshot().unwrap().state,
        before.state
    );
    let recovered = history.recover_pending(&store, &db).await.unwrap();
    assert_eq!(
        recovered
            .snapshot
            .state
            .visible_structured_publication_ids
            .len(),
        1
    );
    assert_eq!(summary.calls.load(Ordering::SeqCst), 1);
    assert_eq!(embedding.passages.load(Ordering::SeqCst), 1);
}

fn session_fixture() -> (
    tempfile::TempDir,
    crate::session::Session,
    Arc<Summary>,
    Arc<Embedder>,
) {
    let root = tempfile::tempdir().unwrap();
    // The product create/open gates stay closed. Build an explicit host fixture
    // by seeding only its initial mode before raw events, then use the normal
    // owner and exact generation checks. Publishing cannot change history mode.
    let saved = crate::session_store::PersistedSession::create(
        root.path(),
        &root.path().join("memory.sqlite3"),
        "p",
        HistoryMode::Strict10,
        None,
    )
    .unwrap();
    let mut initial = saved.snapshot().unwrap().state;
    initial.history_mode = HistoryMode::Structured10;
    std::fs::write(
        saved.store.directory().join("state.json"),
        serde_json::to_vec(&initial).unwrap(),
    )
    .unwrap();
    let mut lock = saved.store.lock().unwrap();
    for turn in 1..=11 {
        lock.append(
            Message::user(if turn == 1 {
                "Aster is production".into()
            } else {
                format!("filler {turn}")
            }),
            true,
        )
        .unwrap();
    }
    let raw = lock.snapshot().unwrap();
    lock.publish(&raw, raw.state.clone()).unwrap();
    let published = lock.snapshot().unwrap();
    drop(lock);
    saved.accept_publication(&published).unwrap();
    let (history, summary, embedding) = backend();
    let mut session = crate::session::Session::new();
    session.attach(saved).unwrap();
    session.strict_history = Some(Arc::new(history));
    (root, session, summary, embedding)
}

#[tokio::test]
async fn session_recovers_saved_response_before_new_input_and_keeps_cursor_in_step() {
    let (_root, mut session, summary, embedding) = session_fixture();
    assert!(session.uses_strict_history().unwrap());
    let saved = session.persistence.clone().unwrap();
    let before = saved.snapshot().unwrap();
    embedding.fail.store(true, Ordering::SeqCst);
    assert!(session.recover_history().await.is_err());
    assert_eq!(saved.snapshot().unwrap().state, before.state);
    let receipt = saved.store.directory().join("structured-attempts/0/1.json");
    let receipt_before = std::fs::read(&receipt).unwrap();
    let raw_before = std::fs::read(saved.store.directory().join("raw.jsonl")).unwrap();
    saved.checkpoint(None).unwrap(); // Exact no-op remains possible.
    assert!(
        saved
            .restore_workflow(crate::workflow::WorkflowConfig::default())
            .is_err()
    );
    session.workflow = Some(crate::workflow::SessionWorkflow::new(
        crate::workflow::WorkflowConfig::default(),
    ));
    assert!(session.checkpoint().is_err());
    assert!(
        saved
            .append_batch(
                &[Message::assistant("changed workflow")],
                session.workflow.as_ref()
            )
            .is_err()
    );
    let mut blocked = session.clone();
    blocked.push_user("new input must wait");
    assert!(blocked.check_persistence().is_err());
    let mut blocked_tool = session.clone();
    blocked_tool.push_tool_result("unresolved", "unknown");
    assert!(blocked_tool.check_persistence().is_err());
    assert_eq!(std::fs::read(&receipt).unwrap(), receipt_before);
    assert_eq!(
        std::fs::read(saved.store.directory().join("raw.jsonl")).unwrap(),
        raw_before
    );
    assert_eq!(saved.snapshot().unwrap().state, before.state);
    session.workflow = None;
    embedding.fail.store(false, Ordering::SeqCst);
    session.recover_history().await.unwrap();
    assert_eq!(
        saved.snapshot().unwrap().state.generation,
        before.state.generation + 1
    );
    assert_eq!(summary.calls.load(Ordering::SeqCst), 1);
    assert_eq!(embedding.passages.load(Ordering::SeqCst), 2);
    session.push_user("Aster environment");
    session.check_persistence().unwrap();
    let evidence = session.prepare_history().await.unwrap().unwrap();
    assert!(evidence.content.contains("production"));
    assert!(evidence.content.contains("Memory retrieval classification"));
    assert!(crate::budget::count_tokens(&evidence.content) <= 768);
    assert_eq!(
        session
            .messages
            .iter()
            .filter(|m| m.role == polaris_provider::Role::User)
            .count(),
        10
    );
    assert_eq!(summary.calls.load(Ordering::SeqCst), 2);
    assert_eq!(embedding.passages.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn absent_receipt_allows_tool_completion_before_starting_new_extraction() {
    let (_root, mut session, summary, embedding) = session_fixture();
    session.push_assistant_tool_calls(
        "",
        vec![polaris_provider::ToolCall {
            id: "pending-tool".into(),
            name: "write".into(),
            arguments: serde_json::json!({}),
        }],
        vec![],
    );
    session.check_persistence().unwrap();
    let saved = session.persistence.clone().unwrap();
    let before = saved.snapshot().unwrap();
    let recovered = session
        .strict_history
        .as_ref()
        .unwrap()
        .recover_saved_structured_observed(&saved.store, &saved.database, &|_| {
            panic!("no saved publication")
        })
        .await
        .unwrap();
    assert_eq!(recovered.snapshot.raw_hash, before.raw_hash);
    assert_eq!(summary.calls.load(Ordering::SeqCst), 0);
    assert!(!saved.store.directory().join("structured-attempts").exists());
    session.recover_history().await.unwrap();
    let recovered = saved.snapshot().unwrap();
    assert_eq!(
        recovered
            .events
            .iter()
            .filter(|e| e.message.tool_call_id.as_deref() == Some("pending-tool"))
            .count(),
        1
    );
    assert_eq!(recovered.state.visible_structured_publication_ids.len(), 1);
    assert_eq!(summary.calls.load(Ordering::SeqCst), 1);
    assert_eq!(embedding.passages.load(Ordering::SeqCst), 1);
    session.recover_history().await.unwrap();
    assert_eq!(summary.calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stale_saved_attempt_is_checked_before_interrupted_tool_append() {
    let (_root, mut session, _summary, embedding) = session_fixture();
    embedding.fail.store(true, Ordering::SeqCst);
    assert!(session.recover_history().await.is_err());
    let saved = session.persistence.clone().unwrap();
    // Reproduce old/corrupt raw state using the lower-level owner, outside the
    // newly guarded Session write path. Recovery must not reanchor this receipt.
    saved
        .store
        .lock()
        .unwrap()
        .append(
            Message::assistant_with_tool_calls(
                "",
                vec![polaris_provider::ToolCall {
                    id: "old-interrupted".into(),
                    name: "write".into(),
                    arguments: serde_json::json!({}),
                }],
            ),
            false,
        )
        .unwrap();
    let raw = std::fs::read(saved.store.directory().join("raw.jsonl")).unwrap();
    let error = session.recover_history().await.unwrap_err();
    assert!(
        error
            .to_string()
            .contains("ownership, generation, or source"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(saved.store.directory().join("raw.jsonl")).unwrap(),
        raw
    );
    assert!(
        !saved
            .snapshot()
            .unwrap()
            .events
            .iter()
            .any(|e| e.message.tool_call_id.as_deref() == Some("old-interrupted"))
    );
}

#[tokio::test]
async fn explicit_clear_retains_unknown_attempt_and_starts_a_fresh_epoch() {
    let (_root, mut session, summary, _embedding) = session_fixture();
    summary.fail.store(true, Ordering::SeqCst);
    assert!(session.recover_history().await.is_err());
    let saved = session.persistence.clone().unwrap();
    let before = saved.snapshot().unwrap();
    let receipt = saved.store.directory().join("structured-attempts/0/1.json");
    let receipt_bytes = std::fs::read(&receipt).unwrap();
    let raw = std::fs::read(saved.store.directory().join("raw.jsonl")).unwrap();
    session.clear_history().unwrap();
    session.recover_history().await.unwrap();
    let cleared = saved.snapshot().unwrap();
    assert_eq!(cleared.state.epoch, before.state.epoch + 1);
    assert_eq!(cleared.state.generation, before.state.generation + 1);
    assert!(cleared.state.visible_structured_publication_ids.is_empty());
    assert_eq!(std::fs::read(&receipt).unwrap(), receipt_bytes);
    assert_eq!(
        std::fs::read(saved.store.directory().join("raw.jsonl")).unwrap(),
        raw
    );
    session.push_user("new epoch");
    session.prepare_history().await.unwrap();
    assert_eq!(summary.calls.load(Ordering::SeqCst), 1);
    assert_eq!(session.messages.len(), 1);
}

#[tokio::test]
async fn r32_public_source_reads_verify_receipts_for_both_namespaces_and_forks() {
    use crate::conversation_memory::{resolve_conversation_uri, structured::sources};
    let (root, store, db) = setup(11);
    let (history, summary, embedding) = backend();
    let prepared = history
        .prepare(&store, &db, "Aster environment")
        .await
        .unwrap();
    let structured_uri = prepared.structured.unwrap().source_uris[0].clone();
    let summary_id = &prepared.snapshot.state.visible_summary_ids[0];
    let source_id = format!("source-{}", summary_id.strip_prefix("strict10-").unwrap());
    let historical_uri = format!("conversation://{source_id}?start=1&end=1");
    let (publication, source, _) = sources::parse_source_uri(&structured_uri).unwrap();
    for uri in [&structured_uri, &historical_uri] {
        let resolved = resolve_conversation_uri(&store, &db, uri).unwrap();
        assert!(resolved.text.contains("Aster is production"));
        assert!(crate::budget::count_tokens(&resolved.text) <= 1024);
    }
    sources::resolve_source(&store, &db, &publication, &source).unwrap();
    let child = store
        .lock()
        .unwrap()
        .fork(root.path(), "00000000-0000-4000-8000-000000000032", "")
        .unwrap();
    for uri in [&structured_uri, &historical_uri] {
        assert!(
            resolve_conversation_uri(&child, &db, uri)
                .unwrap()
                .text
                .contains("Aster is production")
        );
    }
    let receipt = store.directory().join("structured-attempts/0/1.json");
    let bytes = std::fs::read(&receipt).unwrap();
    let mut changed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    changed["prompt_version"] = "other-version".into();
    std::fs::write(&receipt, serde_json::to_vec(&changed).unwrap()).unwrap();
    for owner in [&store, &child] {
        for uri in [&structured_uri, &historical_uri] {
            assert!(resolve_conversation_uri(owner, &db, uri).is_err());
        }
        assert!(sources::resolve_source(owner, &db, &publication, &source).is_err());
    }
    std::fs::write(receipt, bytes).unwrap();
    for uri in [&structured_uri, &historical_uri] {
        resolve_conversation_uri(&child, &db, uri).unwrap();
    }
    MemoryStore::open(&db)
        .unwrap()
        .delete_session("p", ID)
        .unwrap();
    for uri in [&structured_uri, &historical_uri] {
        assert!(resolve_conversation_uri(&child, &db, uri).is_err());
    }
    assert_eq!(summary.calls.load(Ordering::SeqCst), 1);
    assert_eq!(embedding.passages.load(Ordering::SeqCst), 1);
    assert_eq!(embedding.queries.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn r32_historical_source_metadata_must_match_the_saved_response() {
    let (root, store, db) = setup(11);
    let (history, _, _) = backend();
    let source = store.lock().unwrap().snapshot().unwrap();
    history
        .prepare(&store, &db, "Aster environment")
        .await
        .unwrap();
    let response = store
        .bound_structured_attempt(&source, 1)
        .unwrap()
        .unwrap()
        .response
        .unwrap();
    let evidence = summary_evidence(&source, 1, &response.summary).unwrap();
    let mut pending = MemoryStore::open(&db)
        .unwrap()
        .pending_summary(&evidence.scope, &evidence.id)
        .unwrap()
        .unwrap();
    pending.source.id = "source-replaced".into();
    let other_path = root.path().join("replaced-index.sqlite3");
    let mut other = MemoryStore::open(&other_path).unwrap();
    other.enable_structured_memory().unwrap();
    other
        .insert_structured_delta(
            &response.delta,
            &crate::conversation_memory::structured::sources::publication_view(&source.state)
                .unwrap(),
        )
        .unwrap();
    other.insert_pending_summary(&pending).unwrap();
    drop(other);
    let error = crate::conversation_memory::resolve_conversation_uri(
        &store,
        &other_path,
        "conversation://source-replaced?start=1&end=1",
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("structured storage records"),
        "{error}"
    );
    // The structured rows themselves remain valid; only the forged v2 source fails.
    let uri = crate::conversation_memory::structured::sources::source_uri(
        &response.delta.publication.publication_id,
        &response.delta.entity_revisions[0].source_id,
    )
    .unwrap();
    crate::conversation_memory::resolve_conversation_uri(&store, &other_path, &uri).unwrap();
}

#[tokio::test]
async fn r32_historical_source_rejects_a_different_visible_summary_id() {
    let (root, store, db) = setup(12);
    let (history, _, _) = backend();
    let source = store.lock().unwrap().snapshot().unwrap();
    let prepared = history
        .prepare(&store, &db, "Aster environment")
        .await
        .unwrap();
    let response = store
        .bound_structured_attempt(&source, 1)
        .unwrap()
        .unwrap()
        .response
        .unwrap();
    let evidence = summary_evidence(&source, 1, &response.summary).unwrap();
    let mut pending = MemoryStore::open(&db)
        .unwrap()
        .pending_summary(&evidence.scope, &evidence.id)
        .unwrap()
        .unwrap();
    assert_eq!(prepared.snapshot.state.visible_summary_ids.len(), 2);
    pending.id = prepared.snapshot.state.visible_summary_ids[1].clone();
    let other_path = root.path().join("swapped-summary.sqlite3");
    let mut other = MemoryStore::open(&other_path).unwrap();
    other.enable_structured_memory().unwrap();
    for turn in 1..=2 {
        let receipt = store.read_structured_attempt(0, turn).unwrap().unwrap();
        let delta = receipt.response.unwrap().delta;
        let generation = u64::try_from(delta.publication.scope.generation).unwrap() - 1;
        let archived = store
            .load_snapshot(generation, &delta.publication.raw_hash)
            .unwrap();
        other
            .insert_structured_delta(
                &delta,
                &crate::conversation_memory::structured::sources::publication_view(&archived.state)
                    .unwrap(),
            )
            .unwrap();
    }
    other.insert_pending_summary(&pending).unwrap();
    drop(other);
    let uri = format!("conversation://{}?start=1&end=1", evidence.source.id);
    let error = crate::conversation_memory::resolve_conversation_uri(&store, &other_path, &uri)
        .unwrap_err();
    assert!(
        error.to_string().contains("structured storage records"),
        "{error}"
    );
}

#[path = "structured_tests/related.rs"]
mod related;

/// Kills and reaps a fixture child even when the parent test panics.
struct KillOnDrop(std::process::Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// SIGKILL runs no destructor. The unknown summary is never resent; a saved
/// response is published from the next process without another request.
#[test]
fn a_killed_extraction_neither_resends_an_unknown_summary_nor_repeats_a_saved_one() {
    for hold in ["summary", "passage"] {
        let (root, _store, db) = setup(11);
        let marker = root.path().join("held");
        let log = root.path().join("child.log");
        let mut child = KillOnDrop(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "conversation_memory::strict_history::structured_tests::killed_extraction_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("POLARIS_STRUCTURED_KILL_ROOT", root.path())
                .env("POLARIS_STRUCTURED_KILL_HOLD", hold)
                .stdout(std::process::Stdio::null())
                .stderr(std::fs::File::create(&log).unwrap())
                .spawn()
                .unwrap(),
        );
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
        while !marker.exists() {
            let early = child.0.try_wait().unwrap().is_some();
            assert!(
                !early && std::time::Instant::now() < deadline,
                "{hold}: {}",
                std::fs::read_to_string(&log).unwrap_or_default()
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        child.0.kill().unwrap();
        assert!(!child.0.wait().unwrap().success());
        let reopened = ConversationStore::open(root.path(), "p", ID).unwrap();
        let (history, summary, embedding) = backend();
        let result = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(history.recover_pending(&reopened, &db));
        assert_eq!(summary.calls.load(Ordering::SeqCst), 0, "{hold}");
        if hold == "summary" {
            let Err(error) = result else {
                panic!("unknown summary stops");
            };
            assert!(error.to_string().contains("will not be resent"), "{error}");
            assert_eq!(embedding.passages.load(Ordering::SeqCst), 0);
        } else {
            let recovered = result.unwrap();
            assert_eq!(embedding.passages.load(Ordering::SeqCst), 1);
            assert_eq!(
                recovered
                    .snapshot
                    .state
                    .visible_structured_publication_ids
                    .len(),
                1
            );
        }
    }
}

#[test]
#[ignore = "親テストが抽出の保留中にSIGKILLする子プロセス"]
fn killed_extraction_child() {
    let root = PathBuf::from(std::env::var_os("POLARIS_STRUCTURED_KILL_ROOT").unwrap());
    let marker = root.join("held");
    let summary = std::env::var("POLARIS_STRUCTURED_KILL_HOLD").unwrap() == "summary";
    let (history, _summary, _embedding) = backend_holding(
        summary.then(|| marker.clone()),
        (!summary).then(|| marker.clone()),
    );
    let store = ConversationStore::open(&root, "p", ID).unwrap();
    let _ = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(history.recover_pending(&store, &root.join("memory.sqlite3")));
    unreachable!("the parent kills this process while the work is held");
}
