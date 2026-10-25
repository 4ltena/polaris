//! v3 strict10 publication, scope, recovery and bounded original-source tests.
use super::*;
use crate::conversation_memory::{
    EmbeddingModel, StrictEmbedder, StrictHistory, StrictSummaryProvider, SummaryRequest,
};
use polaris_desktop_protocol::{
    request::*,
    run_state::Observation,
    snapshot::{Configuration, Draft},
};
use std::{future::Future, io, pin::Pin, sync::Arc};

mod fork_tests;
mod frozen_tests;
mod oracle_tests;
mod publication_process_tests;
mod wire_tests;

type FutureResult<'a, T> = Pin<Box<dyn Future<Output = io::Result<T>> + Send + 'a>>;
struct Summary;
impl StrictSummaryProvider for Summary {
    fn summarize<'a>(&'a self, request: SummaryRequest) -> FutureResult<'a, String> {
        Box::pin(async move { Ok(summary(request.source_turn_id)) })
    }
}
struct Embed(EmbeddingModel);
impl StrictEmbedder for Embed {
    fn metadata(&self) -> &EmbeddingModel {
        &self.0
    }
    fn embed_passage<'a>(&'a self, _: &'a str) -> FutureResult<'a, Vec<f32>> {
        Box::pin(async { Ok(vec![1.0, 0.0]) })
    }
    fn embed_query<'a>(&'a self, _: &'a str) -> FutureResult<'a, Vec<Vec<f32>>> {
        Box::pin(async { Ok(vec![vec![1.0, 0.0]]) })
    }
}
fn summary(turn: u64) -> String {
    serde_json::json!({"facts":["旧名Rigel。現在の制約は読取専用"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[turn]}).to_string()
}
fn resources() -> MemoryResources {
    MemoryResources {
        embedding_model: "fixture".into(),
        embedding_revision: "fixed".into(),
        embedding_dimension: 2,
        fingerprint: content_hash(b"fixture"),
    }
}
fn history() -> StrictHistory {
    StrictHistory::new(
        Arc::new(Summary),
        Arc::new(Embed(EmbeddingModel {
            model: "fixture".into(),
            revision: "fixed".into(),
            dimension: 2,
        })),
    )
}
fn request(id: &str, body: RequestBody) -> Request {
    Request {
        protocol_version: Default::default(),
        client_id: ClientId::new("memory-test").unwrap(),
        request_id: RequestId::new(id).unwrap(),
        body,
    }
}
fn accept(writer: &mut Writer, number: u64) -> RunTarget {
    accept_text(writer, number, &format!("turn {number}: Rigel"))
}
fn accept_text(writer: &mut Writer, number: u64, text: &str) -> RunTarget {
    let saved = writer.snapshot().unwrap();
    let session = saved.marker.session_id.clone();
    writer
        .apply(
            &request(
                &format!("draft-{number}"),
                RequestBody::DraftUpdate(
                    session.clone(),
                    DraftUpdate {
                        expected_draft_revision: saved.state.draft.draft_revision,
                        text: text.into(),
                        attachment_ids: vec![],
                    },
                ),
            ),
            None,
        )
        .unwrap();
    let saved = writer.snapshot().unwrap();
    let target = RunTarget {
        run_id: RunId::new(format!("run-{number}")).unwrap(),
        attempt_id: AttemptId::new(format!("attempt-{number}")).unwrap(),
    };
    writer
        .apply(
            &request(
                &format!("start-{number}"),
                RequestBody::RunStart(
                    session,
                    RunStart {
                        mcp_prompt: None,
                        expected_draft_revision: saved.state.draft.draft_revision,
                        expected_configuration_revision: saved
                            .state
                            .configuration
                            .configuration_revision,
                        expected_policy_revision: saved.state.policy_revision,
                    },
                ),
            ),
            Some(target.clone()),
        )
        .unwrap();
    writer
        .record_intent(&target, OperationId::new(format!("main-{number}")).unwrap())
        .unwrap();
    target
}
fn finish(writer: &mut Writer, target: &RunTarget, number: u64) {
    writer
        .finish_with_text(
            target,
            &OperationId::new(format!("main-{number}")).unwrap(),
            Observation::Succeeded,
            ResultId::new(format!("result-{number}")).unwrap(),
            format!("answer {number}"),
        )
        .unwrap();
}
fn setup() -> (PrototypeRoot, Writer, RunTarget) {
    setup_named("memory-project", "memory-session")
}
fn setup_named(project: &str, session: &str) -> (PrototypeRoot, Writer, RunTarget) {
    setup_mode(project, session, HistoryMode::Strict10)
}
fn setup_mode(
    project: &str,
    session: &str,
    mode: HistoryMode,
) -> (PrototypeRoot, Writer, RunTarget) {
    setup_turns(project, session, mode, 11)
}
fn setup_turns(
    project: &str,
    session: &str,
    mode: HistoryMode,
    turns: u64,
) -> (PrototypeRoot, Writer, RunTarget) {
    setup_query_turns(
        project,
        session,
        mode,
        turns,
        &format!("turn {turns}: Rigel"),
    )
}
fn setup_query_turns(
    project: &str,
    session: &str,
    mode: HistoryMode,
    turns: u64,
    query: &str,
) -> (PrototypeRoot, Writer, RunTarget) {
    let root = PrototypeRoot::new().unwrap();
    // v3 identifiers are opaque and need not satisfy v2's UUID grammar.
    let mut writer = root
        .create(
            ProjectId::new(project).unwrap(),
            SessionId::new(session).unwrap(),
            InitialState {
                draft: Draft {
                    draft_revision: DecimalU64::new(0),
                    text: String::new(),
                    attachment_ids: vec![],
                },
                configuration: Configuration {
                    configuration_revision: DecimalU64::new(0),
                    provider: "codex".into(),
                    model: "gpt-6-astra".into(),
                    effort: "medium".into(),
                    history_mode: mode,
                },
                policy_revision: DecimalU64::new(0),
            },
        )
        .unwrap();
    for number in 1..turns {
        let target = accept(&mut writer, number);
        finish(&mut writer, &target, number);
    }
    let target = accept_text(&mut writer, turns, query);
    writer.bind_memory(&target, resources()).unwrap();
    (root, writer, target)
}

async fn structured_pending(
    writer: &mut Writer,
    target: &RunTarget,
) -> (SummaryWork, PendingSummary, SavedStructuredResponse) {
    let snapshot = writer.memory_snapshot(target).unwrap();
    let work = writer.begin_summary(target, &snapshot, 1).unwrap();
    let intent = work.structured_intent.as_ref().unwrap();
    let source = intent.sources.first().unwrap();
    let start = source.content.find("Rigel").unwrap();
    let response = serde_json::json!({
        "summary": {"facts":["Rigel is a fixture"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[1]},
        "processed_source_ids": intent.sources.iter().map(|source| source.source_id.clone()).collect::<Vec<_>>(),
        "entity_candidates": [{"source_id":source.source_id,"evidence_span":{"start":start,"length":5},"speech_act":"assert","ordinal":0,"kind":"product","name":"Rigel","subject_project":"memory-project","evidence_text":"Rigel"}],
        "fact_deltas": [{"source_id":source.source_id,"evidence_span":{"start":start,"length":5},"speech_act":"assert","ordinal":0,"entity_kind":"product","entity_name":"Rigel","fact_kind":"fixture","value":"active","unit":"none","condition":"","relation":"assert","evidence_text":"Rigel"}]
    }).to_string();
    let response = crate::conversation_memory::structured::validate_response(
        &work.source,
        1,
        intent,
        &response,
    )
    .unwrap();
    writer
        .save_structured_response(target, &work, &response)
        .unwrap();
    let saved = SavedStructuredResponse {
        summary: response.summary.clone(),
        delta: response.delta.clone(),
        coverage: response.coverage.clone(),
    };
    let pending = history()
        .embed_summary(&work.source, 1, response.summary)
        .await
        .unwrap();
    (work, pending, saved)
}

#[tokio::test]
async fn structured_desktop_first_ten_turns_use_raw_without_auxiliary_requests() {
    use crate::desktop_memory::DesktopMemory;
    use polaris_desktop_protocol::snapshot::MemoryRetrievalStatus;
    struct NoRequests(EmbeddingModel);
    impl StrictSummaryProvider for NoRequests {
        fn summarize<'a>(&'a self, _: SummaryRequest) -> FutureResult<'a, String> {
            panic!("no summary request is needed within the direct window")
        }
    }
    impl StrictEmbedder for NoRequests {
        fn metadata(&self) -> &EmbeddingModel {
            &self.0
        }
        fn embed_passage<'a>(&'a self, _: &'a str) -> FutureResult<'a, Vec<f32>> {
            panic!("no passage embedding is needed within the direct window")
        }
        fn embed_query<'a>(&'a self, _: &'a str) -> FutureResult<'a, Vec<Vec<f32>>> {
            panic!("no query embedding is needed without historical evidence")
        }
    }
    for turns in [1, 10] {
        let (_root, mut writer, target) = setup_turns(
            "memory-project",
            "memory-session",
            HistoryMode::Structured10,
            turns,
        );
        let temp = tempfile::tempdir().unwrap();
        let database = temp.path().join("memory.sqlite3");
        MemoryStore::open(&database)
            .unwrap()
            .enable_structured_memory()
            .unwrap();
        let backend = Arc::new(NoRequests(EmbeddingModel {
            model: "fixture".into(),
            revision: "fixed".into(),
            dimension: 2,
        }));
        let (runtime, mut owner) = DesktopMemory::pair(
            target,
            Arc::new(StrictHistory::new(backend.clone(), backend)),
            database,
            resources(),
        )
        .unwrap();
        let raw_before = writer.snapshot().unwrap().marker.raw_hash;
        let task = tokio::spawn(async move { runtime.prepare().await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !task.is_finished() {
                owner.poll(Some(&mut writer), false);
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let crate::desktop_memory::PreparedDesktopHistory {
            messages, evidence, ..
        } = task.await.unwrap().unwrap();
        assert_eq!(
            messages
                .iter()
                .filter(|m| m.role == polaris_provider::Role::User)
                .count(),
            turns as usize
        );
        assert!(messages.iter().any(|m| m.content == "turn 1: Rigel"));
        let evidence = evidence.unwrap();
        assert!(evidence.content.contains("\"not_found\""));
        let saved = writer.snapshot().unwrap();
        assert_eq!(saved.marker.raw_hash, raw_before);
        let status = saved.state.runs.last().unwrap().memory.as_ref().unwrap();
        assert_eq!(status.phase, MemoryPhase::Ready);
        assert_eq!(
            status.retrieval_status,
            Some(MemoryRetrievalStatus::NotFound)
        );
        assert!(status.retrieval_sources.is_empty());
        assert_eq!(
            status.reference_tokens.get(),
            crate::budget::count_tokens(&evidence.content) as u64
        );
    }
}

#[tokio::test]
async fn structured_desktop_owner_injects_verified_evidence_and_records_classification() {
    use crate::desktop_memory::DesktopMemory;
    use polaris_desktop_protocol::snapshot::MemoryRetrievalStatus;
    let (_root, mut writer, target) = setup_mode(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
    );
    let db = tempfile::tempdir().unwrap();
    let database = db.path().join("memory.sqlite3");
    let (work, pending, response) = structured_pending(&mut writer, &target).await;
    writer
        .publish_structured_memory(&target, &work, &pending, &response, &database)
        .unwrap();
    let raw_before = writer.snapshot().unwrap().marker.raw_hash;
    let (runtime, mut owner) = DesktopMemory::pair(
        target.clone(),
        Arc::new(history()),
        database.clone(),
        resources(),
    )
    .unwrap();
    let task = tokio::spawn(async move { runtime.prepare().await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !task.is_finished() {
            owner.poll(Some(&mut writer), false);
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let crate::desktop_memory::PreparedDesktopHistory {
        messages, evidence, ..
    } = task.await.unwrap().unwrap();
    let evidence = evidence.unwrap();
    assert!(
        evidence.content.contains("Rigel")
            && evidence.content.contains("conversation://structured/")
    );
    assert!(evidence.content.contains("\"found\""));
    assert_eq!(
        messages
            .iter()
            .filter(|m| m.role == polaris_provider::Role::User)
            .count(),
        10
    );
    assert!(!messages.iter().any(|m| m.content == "turn 1: Rigel"));
    let saved = writer.snapshot().unwrap();
    assert_eq!(saved.marker.raw_hash, raw_before);
    let status = saved.state.runs.last().unwrap().memory.as_ref().unwrap();
    assert_eq!(status.retrieval_status, Some(MemoryRetrievalStatus::Found));
    assert_eq!(status.phase, MemoryPhase::Ready);
    assert_eq!(status.retrieval_sources.len(), 1);
    assert_eq!(
        status.reference_tokens.get(),
        crate::budget::count_tokens(&evidence.content) as u64
    );
    assert!(status.reference_tokens.get() <= 768);

    let before_cancelled = writer.snapshot().unwrap().marker;
    let (runtime, mut owner) = DesktopMemory::pair(
        target.clone(),
        Arc::new(history()),
        database.clone(),
        resources(),
    )
    .unwrap();
    let cancelled = tokio::spawn(async move { runtime.prepare().await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !cancelled.is_finished() {
            owner.poll(Some(&mut writer), true);
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(cancelled.await.unwrap().is_err());
    assert_eq!(writer.snapshot().unwrap().marker, before_cancelled);

    // Retrieval cannot invent extraction or fall back to old values when a
    // new accepted input has not yet been processed by the summary path.
    finish(&mut writer, &target, 11);
    let next = accept(&mut writer, 12);
    writer.bind_memory(&next, resources()).unwrap();
    let current = writer.memory_snapshot(&next).unwrap();
    let incomplete = writer
        .prepare_structured_memory(&next, &current, &database)
        .unwrap();
    assert_eq!(
        incomplete.status,
        MemoryRetrievalStatus::ExtractionIncomplete
    );
    assert!(incomplete.text.is_empty());
    assert!(
        writer
            .prepare_structured_memory(&target, &current, &database)
            .is_err()
    );
    assert!(
        writer
            .prepare_structured_memory(&next, &work.expected, &database)
            .is_err()
    );
}

#[tokio::test]
async fn structured_publication_is_atomic_visible_and_restart_reuses_saved_response() {
    let (root, mut writer, target) = setup_mode(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
    );
    let db = tempfile::tempdir().unwrap();
    let database = db.path().join("memory.sqlite3");
    let raw_before = writer.snapshot().unwrap();
    let (work, pending, response) = structured_pending(&mut writer, &target).await;
    writer
        .publish_structured_memory(&target, &work, &pending, &response, &database)
        .unwrap();
    let current = writer.memory_snapshot(&target).unwrap();
    let uri = crate::conversation_memory::structured::sources::source_uri(
        &response.delta.publication.publication_id,
        &response.delta.entity_revisions[0].source_id,
    )
    .unwrap();
    let original = writer.read_memory_source(&target, &database, &uri).unwrap();
    let before_prepare = writer.snapshot().unwrap();
    let prepared = writer
        .prepare_structured_memory(&target, &current, &database)
        .unwrap();
    assert_eq!(
        prepared.status,
        crate::conversation_memory::structured::packing::RetrievalStatus::Found
    );
    assert!(prepared.text.contains("Rigel") && prepared.text.contains("active"));
    assert_eq!(prepared.source_uris, vec![uri.clone()]);
    assert_eq!(writer.snapshot().unwrap().marker, before_prepare.marker);
    let mut forged = current.clone();
    forged
        .conversation
        .events
        .last_mut()
        .unwrap()
        .message
        .content = "unlisted.config".into();
    assert!(
        writer
            .prepare_structured_memory(&target, &forged, &database)
            .is_err()
    );
    assert!(original.contains("turn 1: Rigel"));
    assert!(original.contains(&response.delta.publication.raw_hash));
    assert!(original.contains("historical evidence"));
    for invalid in [
        uri.replace("offset=0", "offset=01"),
        crate::conversation_memory::structured::sources::source_uri(
            "unpublished",
            &response.delta.entity_revisions[0].source_id,
        )
        .unwrap(),
        crate::conversation_memory::structured::sources::source_uri(
            &response.delta.publication.publication_id,
            "source-v3:memory-project:memory-session:0:999",
        )
        .unwrap(),
    ] {
        assert!(
            writer
                .read_memory_source(&target, &database, &invalid)
                .is_err()
        );
    }
    let mut wrong_publication = writer.snapshot().unwrap();
    wrong_publication
        .state
        .conversation_memory
        .as_mut()
        .unwrap()
        .sources[0]
        .structured_publication_id = Some("substituted".into());
    assert!(super::validate(&wrong_publication).is_err());
    assert_eq!(
        current.conversation.state.history_mode,
        HistoryMode::Structured10
    );
    assert_eq!(
        current
            .conversation
            .state
            .visible_structured_publication_ids,
        vec![response.delta.publication.publication_id.clone()]
    );
    assert_eq!(
        current.conversation.state.structured_coverage.as_ref(),
        Some(&response.coverage)
    );
    assert_eq!(
        writer.snapshot().unwrap().marker.raw_hash,
        raw_before.marker.raw_hash
    );
    let publication = response.delta.publication.scope.clone();
    assert_eq!(
        MemoryStore::open(&database)
            .unwrap()
            .structured_publication(&publication, &response.delta.publication.publication_id)
            .unwrap(),
        Some(response.delta.publication.clone())
    );
    finish(&mut writer, &target, 11);
    drop(writer);
    let mut reopened = root
        .open(
            &ProjectId::new("memory-project").unwrap(),
            &SessionId::new("memory-session").unwrap(),
        )
        .unwrap();
    let next = accept(&mut reopened, 12);
    reopened.bind_memory(&next, resources()).unwrap();
    assert_eq!(
        reopened.read_memory_source(&next, &database, &uri).unwrap(),
        original
    );
    assert!(
        reopened
            .read_memory_source(&target, &database, &uri)
            .is_err()
    );
    assert_eq!(
        reopened
            .memory_snapshot(&next)
            .unwrap()
            .conversation
            .state
            .visible_structured_publication_ids
            .len(),
        1
    );
    let (_, other, other_target) =
        setup_mode("memory-project", "other-session", HistoryMode::Structured10);
    assert!(
        other
            .read_memory_source(&other_target, &database, &uri)
            .is_err()
    );
    let raw_path = reopened.test_directory().join("raw.jsonl");
    let raw_bytes = std::fs::read(&raw_path).unwrap();
    let mut tampered = raw_bytes.clone();
    tampered[0] = b'[';
    std::fs::write(&raw_path, tampered).unwrap();
    let rejected = reopened.read_memory_source(&next, &database, &uri).is_err();
    std::fs::write(&raw_path, raw_bytes).unwrap();
    assert!(rejected);
    assert_eq!(
        reopened.read_memory_source(&next, &database, &uri).unwrap(),
        original
    );
    MemoryStore::open(&database)
        .unwrap()
        .delete_session("memory-project", "memory-session")
        .unwrap();
    assert!(reopened.read_memory_source(&next, &database, &uri).is_err());
}

#[tokio::test]
async fn structured_source_rejects_new_strict_run_without_writes() {
    let (_, mut writer, target) = setup_mode(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
    );
    let db = tempfile::tempdir().unwrap();
    let database = db.path().join("memory.sqlite3");
    let (work, pending, response) = structured_pending(&mut writer, &target).await;
    writer
        .publish_structured_memory(&target, &work, &pending, &response, &database)
        .unwrap();
    let uri = crate::conversation_memory::structured::sources::source_uri(
        &response.delta.publication.publication_id,
        &response.delta.entity_revisions[0].source_id,
    )
    .unwrap();
    assert!(writer.read_memory_source(&target, &database, &uri).is_ok());
    finish(&mut writer, &target, 11);
    let before = writer.snapshot().unwrap();
    writer
        .apply(
            &request(
                "strict-mode",
                RequestBody::SessionConfigure(
                    before.marker.session_id,
                    SessionConfigure {
                        expected_configuration_revision: before
                            .state
                            .configuration
                            .configuration_revision,
                        provider: "codex".into(),
                        model: "gpt-6-astra".into(),
                        effort: "medium".into(),
                        history_mode: HistoryMode::Strict10,
                    },
                ),
            ),
            None,
        )
        .unwrap();
    let next = accept(&mut writer, 12);
    writer.bind_memory(&next, resources()).unwrap();
    let before = writer.snapshot().unwrap();
    assert!(writer.read_memory_source(&next, &database, &uri).is_err());
    let after = writer.snapshot().unwrap();
    assert_eq!(before.marker.raw_hash, after.marker.raw_hash);
    assert_eq!(
        before.marker.session_revision,
        after.marker.session_revision
    );
}

#[tokio::test]
async fn structured_source_rejects_resealed_foreign_index() {
    use polaris_memory::structured::{StructuredAlias, delta_hash};
    let (_, mut writer, target) = setup_mode(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
    );
    let db = tempfile::tempdir().unwrap();
    let database = db.path().join("memory.sqlite3");
    let (work, pending, response) = structured_pending(&mut writer, &target).await;
    writer
        .publish_structured_memory(&target, &work, &pending, &response, &database)
        .unwrap();
    let original_source = &response.delta.entity_revisions[0].source_id;
    let uri = crate::conversation_memory::structured::sources::source_uri(
        &response.delta.publication.publication_id,
        original_source,
    )
    .unwrap();
    assert!(writer.read_memory_source(&target, &database, &uri).is_ok());
    let foreign_source =
        crate::conversation_memory::structured::source_id(&work.source, &work.source.events[1]);
    let mut changed = response.delta.clone();
    changed.entity_revisions[0].canonical_name = "Other".into();
    changed.aliases.push(StructuredAlias {
        entity_id: changed.entities[0].entity_id.clone(),
        display_alias: "Other".into(),
        match_alias: "other".into(),
        relation: "alias".into(),
        source_id: foreign_source.clone(),
    });
    changed.publication.prompt_version = "foreign-index".into();
    changed.publication.delta_hash = delta_hash(&changed).unwrap();
    let foreign_db = db.path().join("foreign.sqlite3");
    let mut foreign = MemoryStore::open(&foreign_db).unwrap();
    foreign.enable_structured_memory().unwrap();
    foreign
        .insert_structured_delta(&changed, &work.expected.structured_view().unwrap())
        .unwrap();
    let foreign_uri = crate::conversation_memory::structured::sources::source_uri(
        &changed.publication.publication_id,
        &foreign_source,
    )
    .unwrap();
    let view = writer
        .memory_snapshot(&target)
        .unwrap()
        .structured_view()
        .unwrap();
    // The alternate index is internally valid and authorizes this raw event.
    foreign
        .with_structured_source(
            &view,
            &changed.publication.publication_id,
            &foreign_source,
            |_| Ok(()),
        )
        .unwrap();
    let current = writer.memory_snapshot(&target).unwrap();
    assert!(
        writer
            .prepare_structured_memory(&target, &current, &foreign_db)
            .is_err()
    );
    assert!(
        writer
            .prepare_structured_memory(&target, &current, &database)
            .is_ok()
    );
    assert!(
        writer
            .read_memory_source(&target, &foreign_db, &foreign_uri)
            .is_err()
    );
    assert!(
        writer
            .read_memory_source(&target, &foreign_db, &uri)
            .is_err()
    );
    assert!(writer.read_memory_source(&target, &database, &uri).is_ok());
}

#[tokio::test]
async fn structured_saved_response_and_source_tampering_are_rejected() {
    let (_, mut writer, target) = setup_mode(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
    );
    let (work, _, response) = structured_pending(&mut writer, &target).await;
    let mut modified = writer.snapshot().unwrap();
    modified
        .state
        .conversation_memory
        .as_mut()
        .unwrap()
        .attempts[0]
        .structured
        .as_mut()
        .unwrap()
        .response
        .as_mut()
        .unwrap()
        .delta
        .facts[0]
        .value_json = "\"tampered\"".into();
    assert!(super::validate(&modified).is_err());
    let mut source = writer.snapshot().unwrap();
    source.state.conversation_memory.as_mut().unwrap().attempts[0]
        .structured
        .as_mut()
        .unwrap()
        .intent
        .sources[0]
        .role = polaris_provider::Role::Assistant;
    assert!(super::validate(&source).is_err());
    let mut coverage = writer.snapshot().unwrap();
    let structured = coverage
        .state
        .conversation_memory
        .as_mut()
        .unwrap()
        .attempts[0]
        .structured
        .as_mut()
        .unwrap();
    structured.intent.coverage.raw_offset -= 1;
    structured.response.as_mut().unwrap().coverage = structured.intent.coverage.clone();
    assert!(super::validate(&coverage).is_err());
    let mut changed = response.delta.clone();
    changed.entity_revisions[0].source_id = "source-v3:other".into();
    changed.publication.delta_hash = polaris_memory::structured::delta_hash(&changed).unwrap();
    assert!(
        crate::conversation_memory::structured::validate_saved_response(
            &work.source,
            work.turn,
            work.structured_intent.as_ref().unwrap(),
            &response.summary,
            &changed,
            &response.coverage,
        )
        .is_err()
    );
    super::validate(&writer.snapshot().unwrap()).unwrap();
}

#[tokio::test]
async fn structured_marker_failure_keeps_v2_v3_rows_invisible_and_result_reusable() {
    let (_, mut writer, target) = setup_mode(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
    );
    let db = tempfile::tempdir().unwrap();
    let database = db.path().join("memory.sqlite3");
    let (work, pending, response) = structured_pending(&mut writer, &target).await;
    super::super::disk::FAIL.with(|flag| flag.set(Some("marker_rename")));
    assert!(
        writer
            .publish_structured_memory(&target, &work, &pending, &response, &database)
            .is_err()
    );
    super::super::disk::FAIL.with(|flag| flag.set(None));
    writer.recover().unwrap();
    assert!(
        writer
            .memory_snapshot(&target)
            .unwrap()
            .conversation
            .state
            .visible_structured_publication_ids
            .is_empty()
    );
    let publication = response.delta.publication.scope.clone();
    assert!(
        MemoryStore::open(&database)
            .unwrap()
            .structured_publication(&publication, &response.delta.publication.publication_id)
            .unwrap()
            .is_some()
    );
    let retry = writer
        .begin_summary(&target, &writer.memory_snapshot(&target).unwrap(), 1)
        .unwrap();
    assert!(!retry.newly_started);
    assert_eq!(retry.structured_response.unwrap().summary, response.summary);
    writer
        .publish_structured_memory(&target, &work, &pending, &response, &database)
        .unwrap();
    let current = writer.memory_snapshot(&target).unwrap();
    assert_eq!(
        current
            .conversation
            .state
            .visible_structured_publication_ids,
        vec![response.delta.publication.publication_id.clone()]
    );
    MemoryStore::open(&database)
        .unwrap()
        .verify_structured_delta(&response.delta)
        .unwrap();
}
async fn pending(writer: &mut Writer, target: &RunTarget) -> (SummaryWork, PendingSummary) {
    let snapshot = writer.memory_snapshot(target).unwrap();
    let work = writer.begin_summary(target, &snapshot, 1).unwrap();
    assert!(work.newly_started);
    let text = history()
        .summarize_turn(&work.source, work.turn)
        .await
        .unwrap();
    writer.save_summary(target, &work, &text).unwrap();
    let item = history()
        .embed_summary(&work.source, work.turn, text)
        .await
        .unwrap();
    (work, item)
}

#[tokio::test]
async fn eleven_turns_publish_search_and_originals_survive_new_raw_and_restart() {
    let (root, mut writer, target) = setup();
    let db = tempfile::tempdir().unwrap();
    let database = db.path().join("memory.sqlite3");
    let raw_before = writer.snapshot().unwrap();
    let (work, item) = pending(&mut writer, &target).await;
    assert_eq!(work.expected.conversation.recent_messages().len(), 19);
    assert_eq!(
        work.expected.conversation.recent_messages()[0].content,
        "turn 2: Rigel"
    );
    // A concurrent draft edit is preserved and does not invalidate the source.
    let state = writer.snapshot().unwrap();
    writer
        .apply(
            &request(
                "new-draft",
                RequestBody::DraftUpdate(
                    state.marker.session_id,
                    DraftUpdate {
                        expected_draft_revision: state.state.draft.draft_revision,
                        text: "unsubmitted".into(),
                        attachment_ids: vec![],
                    },
                ),
            ),
            None,
        )
        .unwrap();
    writer
        .publish_memory(&target, &work, &item, &database)
        .unwrap();
    let after = writer.snapshot().unwrap();
    assert_eq!(after.marker.raw_hash, raw_before.marker.raw_hash);
    assert_eq!(after.marker.raw_offset, raw_before.marker.raw_offset);
    assert_eq!(after.raw.len(), 21);
    assert_eq!(after.state.draft.text, "unsubmitted");
    let snapshot = writer.memory_snapshot(&target).unwrap();
    assert_eq!(snapshot.expired_turn().unwrap(), None);
    let hits = history()
        .retrieve(&snapshot.view().unwrap(), &database, "Rigel")
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    let uri = format!("conversation://{}?start=1&end=1", hits[0].source.id);
    let original = writer.read_memory_source(&target, &database, &uri).unwrap();
    assert!(original.contains("turn 1: Rigel"));
    assert!(original.len() <= 4096 && crate::budget::count_tokens(&original) <= 1024);
    finish(&mut writer, &target, 11);
    drop(writer);
    let mut reopened = root
        .open(
            &ProjectId::new("memory-project").unwrap(),
            &SessionId::new("memory-session").unwrap(),
        )
        .unwrap();
    let next = accept(&mut reopened, 12);
    reopened.bind_memory(&next, resources()).unwrap();
    assert_eq!(
        reopened.read_memory_source(&next, &database, &uri).unwrap(),
        original
    );
    assert_eq!(
        reopened
            .memory_snapshot(&next)
            .unwrap()
            .expired_turn()
            .unwrap(),
        Some(2)
    );
}

#[tokio::test]
async fn pending_index_recovery_reuses_saved_summary_and_does_not_publish_early() {
    let (_, mut writer, target) = setup();
    let db = tempfile::tempdir().unwrap();
    let database = db.path().join("memory.sqlite3");
    let (work, item) = pending(&mut writer, &target).await;
    super::super::disk::FAIL.with(|flag| flag.set(Some("marker_rename")));
    assert!(
        writer
            .publish_memory(&target, &work, &item, &database)
            .is_err()
    );
    super::super::disk::FAIL.with(|flag| flag.set(None));
    writer.recover().unwrap();
    let snapshot = writer.memory_snapshot(&target).unwrap();
    assert!(snapshot.view().unwrap().visible_ids.is_empty());
    assert!(
        history()
            .retrieve(&snapshot.view().unwrap(), &database, "Rigel")
            .await
            .unwrap()
            .is_empty()
    );
    let recovered = writer.begin_summary(&target, &snapshot, 1).unwrap();
    assert!(!recovered.newly_started);
    assert_eq!(recovered.summary.as_deref(), Some(item.summary.as_str()));
    writer
        .publish_memory(&target, &recovered, &item, &database)
        .unwrap();
    assert_eq!(
        writer
            .memory_snapshot(&target)
            .unwrap()
            .view()
            .unwrap()
            .visible_ids
            .len(),
        1
    );
}

#[test]
fn oversized_provenance_is_rejected_before_summary_save_without_authorizing_replay() {
    let (_, mut writer, target) = setup();
    let expected = writer.memory_snapshot(&target).unwrap();
    let work = writer.begin_summary(&target, &expected, 1).unwrap();
    let text = serde_json::json!({"facts":["word ".repeat(210)],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[1]}).to_string();
    crate::conversation_memory::validate_summary(&text, 1).unwrap();
    let before = writer.snapshot().unwrap();
    let error = writer.save_summary(&target, &work, &text).unwrap_err();
    assert!(error.to_string().contains("provenance exceeds 256"));
    let after = writer.snapshot().unwrap();
    assert_eq!(after.marker.raw_hash, before.marker.raw_hash);
    assert_eq!(
        after.marker.session_revision,
        before.marker.session_revision
    );
    let memory = after.state.conversation_memory.unwrap();
    assert!(memory.sources.is_empty());
    assert!(memory.attempts[0].summary.is_none());
    let repeated = writer.begin_summary(&target, &expected, 1).unwrap();
    assert!(!repeated.newly_started && repeated.summary.is_none());
}

#[test]
fn unknown_summary_is_not_authorized_again_and_configuration_cas_is_enforced() {
    let (_, mut writer, target) = setup();
    let expected = writer.memory_snapshot(&target).unwrap();
    let work = writer.begin_summary(&target, &expected, 1).unwrap();
    assert!(work.newly_started);
    let repeated = writer.begin_summary(&target, &expected, 1).unwrap();
    assert!(!repeated.newly_started && repeated.summary.is_none());
    let before = writer.snapshot().unwrap();
    writer
        .apply(
            &request(
                "mode-change",
                RequestBody::SessionConfigure(
                    before.marker.session_id,
                    SessionConfigure {
                        expected_configuration_revision: before
                            .state
                            .configuration
                            .configuration_revision,
                        provider: "codex".into(),
                        model: "gpt-6-astra".into(),
                        effort: "medium".into(),
                        history_mode: HistoryMode::Legacy,
                    },
                ),
            ),
            None,
        )
        .unwrap();
    assert!(writer.save_summary(&target, &work, &summary(1)).is_err());
    let after = writer.snapshot().unwrap();
    assert_eq!(
        after.state.runs.last().unwrap().configuration.history_mode,
        HistoryMode::Strict10
    );
    assert_eq!(after.state.configuration.history_mode, HistoryMode::Legacy);
    assert_eq!(after.marker.raw_hash, before.marker.raw_hash);
}

#[tokio::test]
async fn forgotten_source_fails_both_retrieval_and_uri_after_positive_control() {
    let (_, mut writer, target) = setup();
    let db = tempfile::tempdir().unwrap();
    let database = db.path().join("memory.sqlite3");
    let (work, item) = pending(&mut writer, &target).await;
    writer
        .publish_memory(&target, &work, &item, &database)
        .unwrap();
    let uri = format!("conversation://{}", item.source.id);
    assert!(writer.read_memory_source(&target, &database, &uri).is_ok());
    MemoryStore::open(&database)
        .unwrap()
        .delete_session("memory-project", "memory-session")
        .unwrap();
    assert!(writer.read_memory_source(&target, &database, &uri).is_err());
    assert!(
        history()
            .retrieve(
                &writer.memory_snapshot(&target).unwrap().view().unwrap(),
                &database,
                "Rigel"
            )
            .await
            .is_err()
    );
}

#[tokio::test]
async fn different_scopes_old_epoch_and_tombstone_cannot_retrieve_published_original() {
    let (_, mut writer, target) = setup();
    let db = tempfile::tempdir().unwrap();
    let database = db.path().join("memory.sqlite3");
    let (work, item) = pending(&mut writer, &target).await;
    writer
        .publish_memory(&target, &work, &item, &database)
        .unwrap();
    let uri = format!("conversation://{}", item.source.id);
    let expected = writer.memory_snapshot(&target).unwrap();
    assert_eq!(
        history()
            .retrieve(&expected.view().unwrap(), &database, "Rigel")
            .await
            .unwrap()
            .len(),
        1
    );
    assert!(writer.read_memory_source(&target, &database, &uri).is_ok());
    for (project, session) in [
        ("other-project", "memory-session"),
        ("memory-project", "other-session"),
    ] {
        let (_, other, other_target) = setup_named(project, session);
        assert!(
            history()
                .retrieve(
                    &other
                        .memory_snapshot(&other_target)
                        .unwrap()
                        .view()
                        .unwrap(),
                    &database,
                    "Rigel"
                )
                .await
                .unwrap()
                .is_empty()
        );
        assert!(
            other
                .read_memory_source(&other_target, &database, &uri)
                .is_err()
        );
    }
    let mut stale = expected.clone();
    stale.conversation.state.epoch += 1;
    assert!(writer.check_memory_snapshot(&target, &stale).is_err());
    assert!(
        history()
            .retrieve(&stale.view().unwrap(), &database, "Rigel")
            .await
            .unwrap()
            .is_empty()
    );
    // Unsupported ancestry cannot be supplied to either the CAS or URI parser.
    let mut imported = serde_json::to_value(&writer.snapshot().unwrap().state).unwrap();
    imported["ancestors"] = serde_json::json!([{"session_id":"other-session"}]);
    assert!(serde_json::from_value::<Sidecar>(imported).is_err());
    assert!(
        writer
            .read_memory_source(&target, &database, &(uri + "?session=other-session"))
            .is_err()
    );
    finish(&mut writer, &target, 11);
    writer.tombstone().unwrap();
    assert!(matches!(
        writer.memory_snapshot(&target),
        Err(StoreError::Deleted)
    ));
}

#[tokio::test]
async fn resource_rebinding_and_source_metadata_tampering_are_refused() {
    let (_, mut writer, target) = setup();
    let db = tempfile::tempdir().unwrap();
    let database = db.path().join("memory.sqlite3");
    let (work, item) = pending(&mut writer, &target).await;
    writer
        .publish_memory(&target, &work, &item, &database)
        .unwrap();
    let uri = format!("conversation://{}", item.source.id);
    assert!(writer.read_memory_source(&target, &database, &uri).is_ok());
    let mut changed = resources();
    changed.fingerprint = content_hash(b"changed runtime");
    assert!(writer.bind_memory(&target, changed).is_err());
    let mut stale = writer.memory_snapshot(&target).unwrap();
    stale.conversation.raw_hash = content_hash(b"wrong prefix");
    assert!(writer.check_memory_snapshot(&target, &stale).is_err());
    let mut corrupted = writer.snapshot().unwrap();
    let original_state = serde_json::to_vec(&corrupted.state).unwrap();
    corrupted
        .state
        .conversation_memory
        .as_mut()
        .unwrap()
        .sources[0]
        .source
        .raw_hash = content_hash(b"wrong published prefix");
    assert!(matches!(
        writer.commit(corrupted, false),
        Err(StoreError::Corrupt(_))
    ));
    assert_eq!(
        serde_json::to_vec(&writer.snapshot().unwrap().state).unwrap(),
        original_state
    );
    assert!(writer.read_memory_source(&target, &database, &uri).is_ok());
}

fn related_query_embedding() -> polaris_memory::conversation::QueryEmbedding {
    polaris_memory::conversation::QueryEmbedding {
        model: "fixture".into(),
        revision: "fixed".into(),
        dimension: 2,
        values: vec![1., 0.],
    }
}

#[tokio::test]
async fn related_desktop_candidates_bind_saved_response_and_raw_across_restart() {
    let (root, mut writer, target) = setup_mode(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
    );
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let (work, pending, response) = structured_pending(&mut writer, &target).await;
    writer
        .publish_structured_memory(&target, &work, &pending, &response, &database)
        .unwrap();
    let snapshot = writer.memory_snapshot(&target).unwrap();
    let before = writer.snapshot().unwrap();
    let candidates = writer
        .prepare_related_memory(&target, &snapshot, &database, related_query_embedding())
        .unwrap();
    assert!(candidates.keyword.is_empty());
    assert_eq!(candidates.semantic.len(), 1);
    assert_eq!(candidates.semantic[0].pending, pending);
    assert_eq!(writer.snapshot().unwrap().marker, before.marker);
    finish(&mut writer, &target, 11);
    drop(writer);
    let mut reopened = root
        .open(
            &ProjectId::new("memory-project").unwrap(),
            &SessionId::new("memory-session").unwrap(),
        )
        .unwrap();
    reopened.recover().unwrap();
    let previous_target = target;
    let target = accept(&mut reopened, 12);
    reopened.bind_memory(&target, resources()).unwrap();
    let snapshot = reopened.memory_snapshot(&target).unwrap();
    assert!(
        reopened
            .prepare_related_memory(
                &previous_target,
                &snapshot,
                &database,
                related_query_embedding()
            )
            .is_err()
    );
    assert_eq!(
        reopened
            .prepare_related_memory(&target, &snapshot, &database, related_query_embedding())
            .unwrap(),
        candidates
    );
    let raw_path = reopened.test_directory().join("raw.jsonl");
    let original = std::fs::read(&raw_path).unwrap();
    let mut altered = original.clone();
    let position = altered.windows(5).position(|w| w == b"Rigel").unwrap();
    altered[position..position + 5].copy_from_slice(b"Other");
    std::fs::write(&raw_path, altered).unwrap();
    let rejected =
        reopened.prepare_related_memory(&target, &snapshot, &database, related_query_embedding());
    std::fs::write(&raw_path, original).unwrap();
    assert!(rejected.is_err());
    assert_eq!(
        reopened
            .prepare_related_memory(&target, &snapshot, &database, related_query_embedding())
            .unwrap(),
        candidates
    );
    assert!(
        reopened
            .prepare_related_memory(
                &target,
                &work.expected,
                &database,
                related_query_embedding()
            )
            .is_err()
    );
    let mut wrong = related_query_embedding();
    wrong.revision = "different".into();
    assert!(
        reopened
            .prepare_related_memory(&target, &snapshot, &database, wrong)
            .is_err()
    );
}

#[tokio::test]
async fn related_desktop_candidates_reject_resealed_summary_before_filtering() {
    let (_, mut writer, target) = setup_mode(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
    );
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let (work, pending, response) = structured_pending(&mut writer, &target).await;
    writer
        .publish_structured_memory(&target, &work, &pending, &response, &database)
        .unwrap();
    let snapshot = writer.memory_snapshot(&target).unwrap();
    let foreign_path = directory.path().join("foreign.sqlite3");
    let mut foreign = MemoryStore::open(&foreign_path).unwrap();
    foreign.enable_structured_memory().unwrap();
    foreign
        .insert_structured_delta(&response.delta, &work.expected.structured_view().unwrap())
        .unwrap();
    let mut changed = pending.clone();
    changed.summary = changed
        .summary
        .replace("Rigel is a fixture", "Unrelated old material");
    changed.summary_hash = content_hash(changed.summary.as_bytes());
    changed.embedding.input_hash = changed.summary_hash.clone();
    changed.embedding.revision = "different".into();
    foreign.insert_pending_summary(&changed).unwrap();
    // The index alone silently hides this row because its vector revision differs.
    let index_only = foreign
        .related_summary_candidates(polaris_memory::conversation::ConversationQuery {
            published: &snapshot.view().unwrap(),
            keywords: "Rigel",
            embedding: related_query_embedding(),
        })
        .unwrap();
    assert_eq!(index_only, Default::default());
    // The owner still checks its saved response before the revision filter.
    assert!(
        writer
            .prepare_related_memory(&target, &snapshot, &foreign_path, related_query_embedding())
            .is_err()
    );
    assert_eq!(
        writer
            .prepare_related_memory(&target, &snapshot, &database, related_query_embedding())
            .unwrap()
            .semantic
            .len(),
        1
    );
}

#[tokio::test]
async fn structured_runtime_adds_latest_keyword_evidence_without_auxiliary_calls_or_no_hit_filler()
{
    use crate::desktop_memory::DesktopMemory;
    use polaris_desktop_protocol::snapshot::MemoryRetrievalStatus;
    struct NoAux(EmbeddingModel);
    impl StrictSummaryProvider for NoAux {
        fn summarize<'a>(&'a self, _: SummaryRequest) -> FutureResult<'a, String> {
            panic!("saved summary must be reused")
        }
    }
    impl StrictEmbedder for NoAux {
        fn metadata(&self) -> &EmbeddingModel {
            &self.0
        }
        fn embed_passage<'a>(&'a self, _: &'a str) -> FutureResult<'a, Vec<f32>> {
            panic!("saved passage must be reused")
        }
        fn embed_query<'a>(&'a self, _: &'a str) -> FutureResult<'a, Vec<Vec<f32>>> {
            panic!("literal supplements do not need query embedding")
        }
    }
    for (query, expected_status, include_summary) in [
        ("Rigel", MemoryRetrievalStatus::Found, true),
        (
            "Rigel sync.missing_key",
            MemoryRetrievalStatus::NotFound,
            false,
        ),
    ] {
        let (_root, mut writer, target) = setup_query_turns(
            "memory-project",
            "memory-session",
            HistoryMode::Structured10,
            11,
            query,
        );
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("memory.sqlite3");
        let (work, pending, response) = structured_pending(&mut writer, &target).await;
        writer
            .publish_structured_memory(&target, &work, &pending, &response, &database)
            .unwrap();
        let source_uri = format!("conversation://{}?start=1&end=1", pending.source.id);
        let raw_hash = writer.snapshot().unwrap().marker.raw_hash;
        let backend = Arc::new(NoAux(EmbeddingModel {
            model: "fixture".into(),
            revision: "fixed".into(),
            dimension: 2,
        }));
        let (runtime, mut owner) = DesktopMemory::pair(
            target.clone(),
            Arc::new(StrictHistory::new(backend.clone(), backend)),
            database.clone(),
            resources(),
        )
        .unwrap();
        let task = tokio::spawn(async move { runtime.prepare().await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !task.is_finished() {
                owner.poll(Some(&mut writer), false);
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let crate::desktop_memory::PreparedDesktopHistory {
            messages, evidence, ..
        } = task.await.unwrap().unwrap();
        assert!(
            messages
                .iter()
                .any(|m| m.role == polaris_provider::Role::User && m.content == query)
        );
        let evidence = evidence.unwrap();
        assert_eq!(evidence.content.contains(&pending.summary), include_summary);
        assert_eq!(evidence.content.contains(&source_uri), include_summary);
        let saved = writer.snapshot().unwrap();
        assert_eq!(saved.marker.raw_hash, raw_hash);
        let status = saved.state.runs.last().unwrap().memory.as_ref().unwrap();
        assert_eq!(status.retrieval_status, Some(expected_status));
        assert_eq!(status.phase, MemoryPhase::Ready);
        assert_eq!(
            status.reference_tokens.get(),
            crate::budget::count_tokens(&evidence.content) as u64
        );
        assert!(status.reference_tokens.get() <= 768);
        if include_summary {
            assert!(status.retrieval_sources.contains(&source_uri));
            assert!(
                status
                    .retrieval_sources
                    .iter()
                    .any(|s| s.starts_with("conversation://structured/"))
            );
            assert!(
                writer
                    .read_memory_source(&target, &database, &source_uri)
                    .unwrap()
                    .contains("turn 1: Rigel")
            );
        } else {
            assert!(status.retrieval_sources.is_empty());
        }
    }
}

struct SearchBackend {
    metadata: EmbeddingModel,
    expected: String,
    calls: std::sync::atomic::AtomicUsize,
    entered: tokio::sync::Notify,
    release: Option<Arc<tokio::sync::Notify>>,
    dropped: Arc<std::sync::atomic::AtomicBool>,
}
impl SearchBackend {
    fn new(query: &str, pause: bool) -> Arc<Self> {
        Arc::new(Self {
            metadata: EmbeddingModel {
                model: "fixture".into(),
                revision: "fixed".into(),
                dimension: 2,
            },
            expected: query.into(),
            calls: std::sync::atomic::AtomicUsize::new(0),
            entered: tokio::sync::Notify::new(),
            release: pause.then(|| Arc::new(tokio::sync::Notify::new())),
            dropped: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        })
    }
}
impl StrictSummaryProvider for SearchBackend {
    fn summarize<'a>(&'a self, _: SummaryRequest) -> FutureResult<'a, String> {
        panic!("explicit search must never summarize")
    }
}
impl StrictEmbedder for SearchBackend {
    fn metadata(&self) -> &EmbeddingModel {
        &self.metadata
    }
    fn embed_passage<'a>(&'a self, _: &'a str) -> FutureResult<'a, Vec<f32>> {
        panic!("explicit search must never embed passages")
    }
    fn embed_query<'a>(&'a self, text: &'a str) -> FutureResult<'a, Vec<Vec<f32>>> {
        Box::pin(async move {
            struct OnDrop(Arc<std::sync::atomic::AtomicBool>);
            impl Drop for OnDrop {
                fn drop(&mut self) {
                    self.0.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            let _guard = OnDrop(self.dropped.clone());
            assert_eq!(
                text, self.expected,
                "only latest accepted user text may be embedded"
            );
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            self.entered.notify_one();
            if let Some(release) = &self.release {
                release.notified().await;
            }
            Ok(vec![vec![0., 1.], vec![1., 0.]])
        })
    }
}

#[tokio::test]
async fn explicit_related_search_uses_frozen_query_and_preserves_owner_status() {
    use crate::{conversation_memory::related::SEARCH_URI, desktop_memory::DesktopMemory};
    for (query, classification, lane) in [
        ("Rigel", "found", "keyword"),
        ("Rigel sync.missing_key", "not_found", "semantic"),
        ("以前の制約を探す", "not_found", "semantic"),
    ] {
        let (_root, mut writer, target) = setup_query_turns(
            "memory-project",
            "memory-session",
            HistoryMode::Structured10,
            11,
            query,
        );
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("memory.sqlite3");
        let (work, pending, response) = structured_pending(&mut writer, &target).await;
        writer
            .publish_structured_memory(&target, &work, &pending, &response, &database)
            .unwrap();
        let snapshot = writer.memory_snapshot(&target).unwrap();
        let wrong = history()
            .embed_related_query("different accepted text")
            .await
            .unwrap();
        assert!(
            writer
                .search_related_memory(&target, &snapshot, &database, &wrong)
                .is_err()
        );
        let before = search_owner_bytes(&writer);
        let backend = SearchBackend::new(query, false);
        let (runtime, mut owner) = DesktopMemory::pair(
            target.clone(),
            Arc::new(StrictHistory::new(backend.clone(), backend.clone())),
            database.clone(),
            resources(),
        )
        .unwrap();
        for path in [
            format!("{SEARCH_URI}?offset=1"),
            format!("{SEARCH_URI}/more"),
        ] {
            assert!(
                tokio::time::timeout(std::time::Duration::from_secs(1), runtime.read(&path))
                    .await
                    .unwrap()
                    .is_err()
            );
        }
        let task = tokio::spawn(async move { runtime.read(SEARCH_URI).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !task.is_finished() {
                owner.poll(Some(&mut writer), false);
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let output = task.await.unwrap().unwrap();
        assert!(output.contains(&format!(
            "Structured retrieval classification: \"{classification}\""
        )));
        assert!(output.contains(lane));
        assert!(output.contains(&pending.summary));
        assert!(output.contains(&pending.source.raw_hash));
        let uri = format!("conversation://{}?start=1&end=1", pending.source.id);
        assert!(output.contains(&uri));
        assert!(
            writer
                .read_memory_source(&target, &database, &uri)
                .unwrap()
                .contains("turn 1: Rigel")
        );
        assert!(crate::budget::count_tokens(&output) <= 768);
        assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            search_owner_bytes(&writer),
            before,
            "explicit reads must not bind memory or write Ready/status"
        );
    }
}

#[tokio::test]
async fn explicit_related_search_cancellation_and_changed_run_discard_results() {
    use crate::{conversation_memory::related::SEARCH_URI, desktop_memory::DesktopMemory};
    for cancel in [false, true] {
        let query = "以前の制約を探す";
        let (_root, mut writer, target) = setup_query_turns(
            "memory-project",
            "memory-session",
            HistoryMode::Structured10,
            11,
            query,
        );
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("memory.sqlite3");
        let (work, pending, response) = structured_pending(&mut writer, &target).await;
        writer
            .publish_structured_memory(&target, &work, &pending, &response, &database)
            .unwrap();
        let backend = SearchBackend::new(query, true);
        let (runtime, mut owner) = DesktopMemory::pair(
            target.clone(),
            Arc::new(StrictHistory::new(backend.clone(), backend.clone())),
            database,
            resources(),
        )
        .unwrap();
        let task = tokio::spawn(async move { runtime.read(SEARCH_URI).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            loop {
                owner.poll(Some(&mut writer), false);
                if backend.calls.load(std::sync::atomic::Ordering::SeqCst) == 1 {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        if !cancel {
            finish(&mut writer, &target, 11);
        }
        let before = search_owner_bytes(&writer);
        if cancel {
            task.abort();
            assert!(task.await.unwrap_err().is_cancelled());
            owner.poll(Some(&mut writer), true);
        } else {
            backend.release.as_ref().unwrap().notify_one();
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                while !task.is_finished() {
                    owner.poll(Some(&mut writer), false);
                    tokio::task::yield_now().await;
                }
            })
            .await
            .unwrap();
            assert!(task.await.unwrap().is_err());
        }
        assert!(backend.dropped.load(std::sync::atomic::Ordering::SeqCst));
        assert_eq!(search_owner_bytes(&writer), before);
        assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    }
}

fn search_owner_bytes(writer: &Writer) -> Vec<u8> {
    let saved = writer.snapshot().unwrap();
    serde_json::to_vec(&(saved.marker, saved.state, saved.raw)).unwrap()
}

#[tokio::test]
async fn explicit_related_search_queued_cancellation_is_read_only_and_not_retried() {
    use crate::{conversation_memory::related::SEARCH_URI, desktop_memory::DesktopMemory};
    let query = "以前の制約を探す";
    let (_root, mut writer, target) = setup_query_turns(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
        11,
        query,
    );
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let (work, pending, response) = structured_pending(&mut writer, &target).await;
    writer
        .publish_structured_memory(&target, &work, &pending, &response, &database)
        .unwrap();
    let backend = SearchBackend::new(query, true);
    let (runtime, mut owner) = DesktopMemory::pair(
        target.clone(),
        Arc::new(StrictHistory::new(backend.clone(), backend.clone())),
        database.clone(),
        resources(),
    )
    .unwrap();
    let task = tokio::spawn(async move { runtime.read(SEARCH_URI).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while backend.calls.load(std::sync::atomic::Ordering::SeqCst) == 0 {
            owner.poll(Some(&mut writer), false);
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    backend.release.as_ref().unwrap().notify_one();
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while owner.pending_requests() != 1 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert!(backend.dropped.load(std::sync::atomic::Ordering::SeqCst));
    let before = search_owner_bytes(&writer);
    owner.poll(Some(&mut writer), true);
    assert_eq!(
        task.await.unwrap().unwrap_err().to_string(),
        "Conversation memory execution is stopped"
    );
    assert_eq!(search_owner_bytes(&writer), before);
    assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert_eq!(owner.pending_requests(), 0);
    let embedded = history().embed_related_query(query).await.unwrap();
    let snapshot = writer.memory_snapshot(&target).unwrap();
    let missing = directory.path().join("must-not-create.sqlite3");
    assert!(
        writer
            .search_related_memory(&target, &snapshot, &missing, &embedded)
            .is_err()
    );
    assert!(!missing.exists());
    assert_eq!(search_owner_bytes(&writer), before);
}

#[test]
fn structured_preparation_validates_owner_mode_and_query_before_database_creation() {
    let mut created = Vec::new();
    for case in ["finished", "strict10", "missing-user", "valid"] {
        let mode = if case == "strict10" {
            HistoryMode::Strict10
        } else {
            HistoryMode::Structured10
        };
        let (_root, mut writer, target) = setup_turns("memory-project", "memory-session", mode, 1);
        let mut snapshot = writer.memory_snapshot(&target).unwrap();
        if case == "finished" {
            finish(&mut writer, &target, 1);
        }
        if case == "missing-user" {
            snapshot.conversation.events.clear();
        }
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("new.sqlite3");
        let before = search_owner_bytes(&writer);
        let result = writer.prepare_structured_memory(&target, &snapshot, &database);
        if case == "valid" {
            assert_eq!(
                result.unwrap().status,
                polaris_desktop_protocol::snapshot::MemoryRetrievalStatus::NotFound
            );
        } else {
            assert!(result.is_err(), "invalid case must be rejected: {case}");
        }
        assert_eq!(search_owner_bytes(&writer), before);
        created.push(database.exists());
    }
    assert_eq!(
        created,
        [false, false, false, true],
        "only valid automatic preparation may create its database"
    );
}

#[tokio::test]
async fn structured_summary_only_saved_response_survives_restart_without_resending() {
    let (root, mut writer, target) = setup_turns(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
        12,
    );
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let (first, pending, response) = structured_pending(&mut writer, &target).await;
    writer
        .publish_structured_memory(&target, &first, &pending, &response, &database)
        .unwrap();
    let snapshot = writer.memory_snapshot(&target).unwrap();
    let work = writer.begin_summary(&target, &snapshot, 2).unwrap();
    let intent = work.structured_intent.as_ref().unwrap();
    assert!(intent.sources.is_empty());
    let response = crate::conversation_memory::structured::validate_response(
        &work.source, 2, intent,
        &serde_json::json!({
            "summary":{"facts":["Second summary"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[2]},
            "processed_source_ids":[],"entity_candidates":[],"fact_deltas":[]
        }).to_string(),
    ).unwrap();
    writer
        .save_structured_response(&target, &work, &response)
        .unwrap();
    // v2-only history cannot authorize an empty structured extraction.
    let mut forged = writer.snapshot().unwrap();
    let memory = forged.state.conversation_memory.as_mut().unwrap();
    memory.sources[0].structured_publication_id = None;
    memory.sources[0].structured_coverage = None;
    memory.attempts[0].structured = None;
    let mut v2_only = forged.clone();
    v2_only
        .state
        .conversation_memory
        .as_mut()
        .unwrap()
        .attempts
        .pop();
    super::validate(&v2_only).unwrap();
    assert!(super::validate(&forged).is_err());
    let before_restart = writer.snapshot().unwrap().marker.raw_hash;
    drop(writer);
    let mut writer = root
        .open(
            &ProjectId::new("memory-project").unwrap(),
            &SessionId::new("memory-session").unwrap(),
        )
        .unwrap();
    assert_eq!(writer.snapshot().unwrap().marker.raw_hash, before_restart);
    let next = accept(&mut writer, 13);
    writer.bind_memory(&next, resources()).unwrap();
    let before_publish = writer.snapshot().unwrap().marker.raw_hash;
    let recovered = writer
        .begin_summary(&next, &writer.memory_snapshot(&next).unwrap(), 2)
        .unwrap();
    assert!(!recovered.newly_started);
    assert!(
        recovered
            .structured_intent
            .as_ref()
            .unwrap()
            .sources
            .is_empty()
    );
    let saved = recovered.structured_response.as_ref().unwrap();
    assert_eq!(saved.delta, response.delta);
    assert_eq!(saved.summary, response.summary);
    struct OnlyNewSummary(std::sync::atomic::AtomicUsize);
    impl StrictSummaryProvider for OnlyNewSummary {
        fn summarize<'a>(&'a self, request: SummaryRequest) -> FutureResult<'a, String> {
            assert_eq!(request.source_turn_id, 3, "saved turn 2 must not be resent");
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                Ok(serde_json::json!({
                    "summary":{"facts":["Third summary"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[3]},
                    "processed_source_ids":request.structured_sources.unwrap().extraction_source_ids,
                    "entity_candidates":[],"fact_deltas":[]
                }).to_string())
            })
        }
    }
    let provider = Arc::new(OnlyNewSummary(std::sync::atomic::AtomicUsize::new(0)));
    let (runtime, mut owner) = crate::desktop_memory::DesktopMemory::pair(
        next.clone(),
        Arc::new(StrictHistory::new(
            provider.clone(),
            Arc::new(Embed(EmbeddingModel {
                model: "fixture".into(),
                revision: "fixed".into(),
                dimension: 2,
            })),
        )),
        database.clone(),
        resources(),
    )
    .unwrap();
    let task = tokio::spawn(async move { runtime.prepare().await });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !task.is_finished() {
            owner.poll(Some(&mut writer), false);
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.await.unwrap().unwrap();
    assert_eq!(provider.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    let final_snapshot = writer.memory_snapshot(&next).unwrap();
    assert_eq!(final_snapshot.conversation.raw_hash, before_publish);
    assert_eq!(
        final_snapshot.conversation.state.visible_summary_ids.len(),
        3
    );
    assert_eq!(
        final_snapshot
            .conversation
            .state
            .visible_structured_publication_ids
            .len(),
        3
    );
    MemoryStore::open(&database)
        .unwrap()
        .verify_structured_delta(&saved.delta)
        .unwrap();
}

#[tokio::test]
async fn structured_initial_backlog_finishes_summaries_after_extraction_catches_up() {
    struct BacklogSummary(std::sync::atomic::AtomicUsize);
    impl StrictSummaryProvider for BacklogSummary {
        fn summarize<'a>(&'a self, request: SummaryRequest) -> FutureResult<'a, String> {
            Box::pin(async move {
                let call = self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let input = request.structured_sources.unwrap();
                assert_eq!(input.extraction_source_ids.is_empty(), call == 1);
                assert!(!input.summary_target_source_ids.is_empty());
                assert!(input.summary_target_source_ids.iter().all(|id| {
                    input
                        .sources
                        .iter()
                        .any(|source| &source.source_id == id && !source.content.is_empty())
                }));
                Ok(serde_json::json!({
                    "summary": {"facts":["確認の記録"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[request.source_turn_id]},
                    "processed_source_ids":input.extraction_source_ids,
                    "entity_candidates":[],"fact_deltas":[]
                }).to_string())
            })
        }
    }
    let (_root, mut writer, target) = setup_turns(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
        12,
    );
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let raw_hash = writer.snapshot().unwrap().marker.raw_hash;
    let summaries = Arc::new(BacklogSummary(std::sync::atomic::AtomicUsize::new(0)));
    let embedder = Arc::new(Embed(EmbeddingModel {
        model: "fixture".into(),
        revision: "fixed".into(),
        dimension: 2,
    }));
    let (runtime, mut owner) = crate::desktop_memory::DesktopMemory::pair(
        target.clone(),
        Arc::new(StrictHistory::new(summaries.clone(), embedder)),
        database,
        resources(),
    )
    .unwrap();
    let task = tokio::spawn(async move { runtime.prepare().await });
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        while !task.is_finished() {
            owner.poll(Some(&mut writer), false);
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let crate::desktop_memory::PreparedDesktopHistory {
        messages: recent,
        evidence,
        ..
    } = task.await.unwrap().unwrap();
    assert_eq!(
        recent
            .iter()
            .filter(|m| m.role == polaris_provider::Role::User)
            .count(),
        10
    );
    assert!(evidence.unwrap().content.contains("not_found"));
    assert_eq!(summaries.0.load(std::sync::atomic::Ordering::SeqCst), 2);
    let snapshot = writer.memory_snapshot(&target).unwrap();
    assert_eq!(snapshot.expired_turn().unwrap(), None);
    assert_eq!(snapshot.conversation.state.visible_summary_ids.len(), 2);
    assert_eq!(
        snapshot
            .conversation
            .state
            .visible_structured_publication_ids
            .len(),
        2
    );
    assert_eq!(snapshot.conversation.raw_hash, raw_hash);
    let saved = writer.snapshot().unwrap();
    let delta = &saved.state.conversation_memory.as_ref().unwrap().attempts[1]
        .structured
        .as_ref()
        .unwrap()
        .response
        .as_ref()
        .unwrap()
        .delta;
    assert!(delta.entities.is_empty());
    assert!(delta.entity_revisions.is_empty());
    assert!(delta.facts.is_empty());
    assert!(delta.aliases.is_empty());
    assert!(delta.dependencies.is_empty());
    assert!(delta.relations.is_empty());
    assert_eq!(
        saved
            .state
            .runs
            .last()
            .unwrap()
            .memory
            .as_ref()
            .unwrap()
            .phase,
        MemoryPhase::Ready
    );
}

#[tokio::test]
async fn explicit_related_search_preflights_before_embedding_without_owner_writes() {
    use crate::{conversation_memory::related::SEARCH_URI, desktop_memory::DesktopMemory};
    for missing in [true, false] {
        let query = "Rigel";
        let (_root, mut writer, target) = setup_query_turns(
            "memory-project",
            "memory-session",
            HistoryMode::Structured10,
            11,
            query,
        );
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("memory.sqlite3");
        if !missing {
            std::fs::write(&database, b"synthetic invalid SQLite database").unwrap();
        }
        let before = search_owner_bytes(&writer);
        let backend = SearchBackend::new(query, false);
        let (runtime, mut owner) = DesktopMemory::pair(
            target.clone(),
            Arc::new(StrictHistory::new(backend.clone(), backend.clone())),
            database.clone(),
            resources(),
        )
        .unwrap();
        let task = tokio::spawn(async move { runtime.read(SEARCH_URI).await });
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !task.is_finished() {
                owner.poll(Some(&mut writer), false);
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert!(task.await.unwrap().is_err());
        assert_eq!(
            backend.calls.load(std::sync::atomic::Ordering::SeqCst),
            0,
            "unusable related index must fail before query embedding"
        );
        assert_eq!(search_owner_bytes(&writer), before);
        if missing {
            assert!(!database.exists());
        }
    }
}

// One-shot interleaving point for the automatic read transaction regression.
thread_local! {
    static AFTER_MANDATORY_READ: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

pub(super) fn after_mandatory_read() {
    let callback = AFTER_MANDATORY_READ.with(|hook| hook.borrow_mut().take());
    if let Some(callback) = callback {
        callback();
    }
}

#[tokio::test]
async fn related_automatic_read_keeps_mandatory_and_keyword_on_one_database_snapshot() {
    let (_root, mut writer, target) = setup_query_turns(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
        11,
        "Rigel",
    );
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let (work, pending, response) = structured_pending(&mut writer, &target).await;
    writer
        .publish_structured_memory(&target, &work, &pending, &response, &database)
        .unwrap();
    let mut other = rusqlite::Connection::open(&database).unwrap();
    let mode: String = other
        .query_row("PRAGMA journal_mode=WAL", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "wal");
    let snapshot = writer.memory_snapshot(&target).unwrap();
    let before = search_owner_bytes(&writer);
    let original = writer
        .prepare_structured_memory_with_related(&target, &snapshot, &database)
        .unwrap();
    let mut changed: serde_json::Value = serde_json::from_str(&pending.summary).unwrap();
    changed["facts"] = serde_json::json!(["Unrelated material"]);
    let changed = serde_json::to_string(&changed).unwrap();
    let changed_hash = content_hash(changed.as_bytes());
    let summary_id = pending.id.clone();
    let called = std::rc::Rc::new(std::cell::Cell::new(false));
    let observed = called.clone();
    AFTER_MANDATORY_READ.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            let tx = other.transaction().unwrap();
            assert_eq!(tx.execute("UPDATE conversation_summaries_v2 SET summary=?1,summary_hash=?2 WHERE summary_id=?3",
                rusqlite::params![changed, changed_hash, summary_id]).unwrap(), 1);
            assert_eq!(tx.execute("UPDATE conversation_vectors_v2 SET input_hash=?1 WHERE summary_id=?2",
                rusqlite::params![changed_hash, summary_id]).unwrap(), 1);
            tx.commit().unwrap();
            observed.set(true);
        }));
    });
    let result = writer.prepare_structured_memory_with_related(&target, &snapshot, &database);
    AFTER_MANDATORY_READ.with(|hook| hook.borrow_mut().take());
    assert!(
        called.get(),
        "the competing write must commit between mandatory and keyword reads"
    );
    let context = result.expect("one read must not mix index versions");
    assert_eq!(
        (
            context.status,
            &context.text,
            &context.source_uris,
            context.reference_tokens,
            context.related_omission
        ),
        (
            original.status,
            &original.text,
            &original.source_uris,
            original.reference_tokens,
            original.related_omission
        )
    );
    assert!(
        writer
            .prepare_structured_memory_with_related(&target, &snapshot, &database)
            .is_err(),
        "the next independent read must see and reject the changed index"
    );
    assert_eq!(search_owner_bytes(&writer), before);
}

#[tokio::test]
async fn related_automatic_read_checks_raw_receipts_before_keyword_filtering() {
    let (_root, mut writer, target) = setup_query_turns(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
        12,
        "Rigel",
    );
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let (first, pending, response) = structured_pending(&mut writer, &target).await;
    writer
        .publish_structured_memory(&target, &first, &pending, &response, &database)
        .unwrap();
    let snapshot = writer.memory_snapshot(&target).unwrap();
    let work = writer.begin_summary(&target, &snapshot, 2).unwrap();
    let intent = work.structured_intent.as_ref().unwrap();
    assert!(intent.sources.is_empty());
    let response = crate::conversation_memory::structured::validate_response(
        &work.source, 2, intent,
        &serde_json::json!({
            "summary":{"facts":["Second summary"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[2]},
            "processed_source_ids":[],"entity_candidates":[],"fact_deltas":[]
        }).to_string(),
    ).unwrap();
    writer
        .save_structured_response(&target, &work, &response)
        .unwrap();
    let pending = history()
        .embed_summary(&work.source, 2, response.summary.clone())
        .await
        .unwrap();
    let saved = SavedStructuredResponse {
        summary: response.summary,
        delta: response.delta,
        coverage: response.coverage,
    };
    writer
        .publish_structured_memory(&target, &work, &pending, &saved, &database)
        .unwrap();
    let snapshot = writer.memory_snapshot(&target).unwrap();
    let before = writer
        .prepare_structured_memory_with_related(&target, &snapshot, &database)
        .unwrap();
    assert_eq!(
        before.status,
        polaris_desktop_protocol::snapshot::MemoryRetrievalStatus::Found
    );
    let mut forged = writer.snapshot().unwrap();
    let memory = forged.state.conversation_memory.as_mut().unwrap();
    memory.sources[1].source.turn_hash = "0".repeat(64);
    memory.attempts[1].source.turn_hash = "0".repeat(64);
    super::validate(&forged).unwrap();
    writer.commit(forged, false).unwrap();
    let snapshot = writer.memory_snapshot(&target).unwrap();
    assert!(
        writer
            .prepare_structured_memory(&target, &snapshot, &database)
            .is_err(),
        "the shared receipt scan rejects every corrupt visible source before mandatory selection"
    );
    assert!(
        writer
            .prepare_structured_memory_with_related(&target, &snapshot, &database)
            .is_err(),
        "a nonmatching visible receipt must prove its original turn before filtering"
    );
}

#[tokio::test]
async fn related_automatic_read_preserves_mandatory_evidence_on_search_budget() {
    use polaris_memory::conversation::RelatedSummaryBudget;
    let query = format!(
        "Rigel {}",
        (0..128)
            .map(|i| format!("unused{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let (_root, mut writer, target) = setup_query_turns(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
        11,
        &query,
    );
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let (work, pending, response) = structured_pending(&mut writer, &target).await;
    writer
        .publish_structured_memory(&target, &work, &pending, &response, &database)
        .unwrap();
    let snapshot = writer.memory_snapshot(&target).unwrap();
    let before = search_owner_bytes(&writer);
    let mandatory = writer
        .prepare_structured_memory(&target, &snapshot, &database)
        .unwrap();
    assert_eq!(
        mandatory.status,
        polaris_desktop_protocol::snapshot::MemoryRetrievalStatus::Found
    );
    let mut context = writer
        .prepare_structured_memory_with_related(&target, &snapshot, &database)
        .expect("optional query limit must not discard mandatory evidence");
    assert_eq!(
        context.related_omission.take(),
        Some(RelatedSummaryBudget::QueryTerms)
    );
    assert_eq!(
        (
            context.status,
            &context.text,
            &context.source_uris,
            context.reference_tokens,
            context.related_omission
        ),
        (
            mandatory.status,
            &mandatory.text,
            &mandatory.source_uris,
            mandatory.reference_tokens,
            mandatory.related_omission
        )
    );
    assert_eq!(search_owner_bytes(&writer), before);
}

#[tokio::test]
async fn related_automatic_read_preserves_mandatory_at_real_raw_proof_work_limit() {
    use polaris_memory::conversation::RelatedSummaryBudget;
    use polaris_provider::{Message, ToolCall};
    let (_root, mut writer, first) = setup_query_turns(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
        1,
        "Rigel",
    );
    // A complete tool roundtrip makes the real raw prefix large without
    // inventing offsets or enlarging the extraction source-content budget.
    writer
        .finish_with_messages(
            &first,
            &OperationId::new("main-1").unwrap(),
            Observation::Succeeded,
            ResultId::new("result-1").unwrap(),
            vec![
                Message::assistant_with_tool_calls(
                    "",
                    vec![ToolCall {
                        id: "large-raw-call".into(),
                        name: "fixture".into(),
                        arguments: serde_json::json!({"payload":"x".repeat(4 * 1024 * 1024 - 128 * 1024)}),
                    }],
                ),
                Message::tool_result("large-raw-call", "done"),
                Message::assistant("answer 1"),
            ],
        )
        .unwrap();
    for turn in 2..26 {
        let target = accept(&mut writer, turn);
        finish(&mut writer, &target, turn);
    }
    let mut target = accept_text(&mut writer, 26, "Rigel");
    writer.bind_memory(&target, resources()).unwrap();
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let (work, pending, response) = structured_pending(&mut writer, &target).await;
    writer
        .publish_structured_memory(&target, &work, &pending, &response, &database)
        .unwrap();
    for turn in 2..=17 {
        if turn == 17 {
            let snapshot = writer.memory_snapshot(&target).unwrap();
            let saved = writer.snapshot().unwrap();
            let bytes: u64 = memory(&saved)
                .unwrap()
                .sources
                .iter()
                .map(|s| s.source.raw_offset.get())
                .sum();
            assert!(bytes <= 64 * 1024 * 1024);
            let context = writer
                .prepare_structured_memory_with_related(&target, &snapshot, &database)
                .unwrap();
            assert_eq!(
                context.status,
                polaris_desktop_protocol::snapshot::MemoryRetrievalStatus::Found
            );
            assert_eq!(context.related_omission, None);
            finish(&mut writer, &target, 26);
            target = accept_text(&mut writer, 27, "Rigel");
            writer.bind_memory(&target, resources()).unwrap();
        }
        let snapshot = writer.memory_snapshot(&target).unwrap();
        let work = writer.begin_summary(&target, &snapshot, turn).unwrap();
        let intent = work.structured_intent.as_ref().unwrap();
        let response = crate::conversation_memory::structured::validate_response(
            &work.source, turn, intent,
            &serde_json::json!({
                "summary":{"facts":["Other historical material"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[turn]},
                "processed_source_ids":intent.sources.iter().map(|s| s.source_id.clone()).collect::<Vec<_>>(),
                "entity_candidates":[],"fact_deltas":[]
            }).to_string(),
        ).unwrap();
        writer
            .save_structured_response(&target, &work, &response)
            .unwrap();
        let pending = history()
            .embed_summary(&work.source, turn, response.summary.clone())
            .await
            .unwrap();
        writer
            .publish_structured_memory(
                &target,
                &work,
                &pending,
                &SavedStructuredResponse {
                    summary: response.summary,
                    delta: response.delta,
                    coverage: response.coverage,
                },
                &database,
            )
            .unwrap();
    }
    let snapshot = writer.memory_snapshot(&target).unwrap();
    let saved = writer.snapshot().unwrap();
    let bytes: u64 = memory(&saved)
        .unwrap()
        .sources
        .iter()
        .map(|s| s.source.raw_offset.get())
        .sum();
    assert!(bytes > 64 * 1024 * 1024);
    let mandatory = writer
        .prepare_structured_memory(&target, &snapshot, &database)
        .unwrap();
    let before = search_owner_bytes(&writer);
    let mut context = writer
        .prepare_structured_memory_with_related(&target, &snapshot, &database)
        .unwrap();
    assert_eq!(
        context.related_omission.take(),
        Some(RelatedSummaryBudget::RawProofWork)
    );
    assert_eq!(
        (
            context.status,
            &context.text,
            &context.source_uris,
            context.reference_tokens
        ),
        (
            mandatory.status,
            &mandatory.text,
            &mandatory.source_uris,
            mandatory.reference_tokens
        )
    );
    assert!(matches!(
        writer.prepare_related_memory(&target, &snapshot, &database, related_query_embedding()),
        Err(StoreError::RelatedSummaryBudget(
            RelatedSummaryBudget::RawProofWork
        ))
    ));
    assert_eq!(search_owner_bytes(&writer), before);
}

#[tokio::test]
async fn desktop_related_omission_survives_ready_restart_and_clears_after_failed_prepare() {
    use crate::desktop_memory::DesktopMemory;
    use polaris_memory::conversation::RelatedSummaryBudget;
    let query = format!(
        "Rigel {}",
        (0..128)
            .map(|i| format!("unused{i}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    let (root, mut writer, target) = setup_query_turns(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
        11,
        &query,
    );
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let (work, pending, response) = structured_pending(&mut writer, &target).await;
    writer
        .publish_structured_memory(&target, &work, &pending, &response, &database)
        .unwrap();
    let raw_before = writer.snapshot().unwrap().marker.raw_hash;
    let backend = SearchBackend::new(&query, false);
    let (runtime, mut owner) = DesktopMemory::pair(
        target.clone(),
        Arc::new(StrictHistory::new(backend.clone(), backend.clone())),
        database,
        resources(),
    )
    .unwrap();
    let mut session = crate::session::Session::new();
    session.push_user(&query);
    session.desktop_memory = Some(runtime);
    let task = tokio::spawn(async move {
        let evidence = session.prepare_history().await;
        (session, evidence)
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !task.is_finished() {
            owner.poll(Some(&mut writer), false);
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (mut session, evidence) = task.await.unwrap();
    let evidence = evidence.unwrap().unwrap();
    assert_eq!(
        session
            .memory_observation
            .as_ref()
            .and_then(|o| o.related_omission),
        Some(RelatedSummaryBudget::QueryTerms)
    );
    let saved = writer.snapshot().unwrap();
    let status = saved.state.runs.last().unwrap().memory.as_ref().unwrap();
    let observation = session.memory_observation.as_ref().unwrap();
    assert_eq!(observation.mode, HistoryMode::Structured10);
    assert_eq!(observation.retrieval_status, status.retrieval_status);
    assert_eq!(observation.source_uris, status.retrieval_sources);
    assert_eq!(observation.reference_tokens, status.reference_tokens.get());
    let status_json = serde_json::to_value(status).unwrap();
    assert_eq!(status_json["related_omission"], "query_terms");
    assert_eq!(status_json["retrieval_status"], "found");
    assert_eq!(status.phase, MemoryPhase::Ready);
    assert_eq!(
        status.reference_tokens.get(),
        crate::budget::count_tokens(&evidence.content) as u64
    );
    assert!(evidence.content.contains("Rigel"));
    assert!(evidence.content.contains("conversation://structured/"));
    assert_eq!(saved.marker.raw_hash, raw_before);
    assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 0);

    let before = search_owner_bytes(&writer);
    let task = tokio::spawn(async move {
        let result = session.prepare_history().await;
        (session, result)
    });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while !task.is_finished() {
            owner.poll(Some(&mut writer), true);
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let (session, result) = task.await.unwrap();
    assert!(result.is_err());
    assert!(session.memory_observation.is_none());
    assert_eq!(search_owner_bytes(&writer), before);
    drop(session);
    drop(owner);
    drop(writer);
    let reopened = root
        .open(
            &ProjectId::new("memory-project").unwrap(),
            &SessionId::new("memory-session").unwrap(),
        )
        .unwrap();
    let reopened = reopened.snapshot().unwrap();
    assert_eq!(reopened.marker.raw_hash, raw_before);
    let restored = reopened.state.runs.last().unwrap().memory.as_ref().unwrap();
    assert_eq!(restored.phase, MemoryPhase::OutcomeUnknown);
    let mut expected = status.clone();
    expected.phase = MemoryPhase::OutcomeUnknown;
    expected.detail = restored.detail.clone();
    assert_eq!(*restored, expected);
    assert_eq!(backend.calls.load(std::sync::atomic::Ordering::SeqCst), 0);
}

#[test]
fn desktop_related_omission_requires_structured_success_without_invalid_writes() {
    use polaris_desktop_protocol::snapshot::{
        MemoryRelatedOmission as Omission, MemoryRetrievalStatus as Retrieval,
    };
    for mode in [HistoryMode::Strict10, HistoryMode::Structured10] {
        let (_root, mut writer, target) = setup_turns("memory-project", "memory-session", mode, 1);
        let base = writer
            .snapshot()
            .unwrap()
            .state
            .runs
            .last()
            .unwrap()
            .memory
            .clone()
            .unwrap();
        assert!(base.related_omission.is_none());
        for reason in [
            Omission::QueryTerms,
            Omission::Grants,
            Omission::SerializedRows,
            Omission::MatchingWork,
            Omission::VectorWork,
            Omission::RawProofWork,
        ] {
            for retrieval in [
                None,
                Some(Retrieval::Found),
                Some(Retrieval::PendingReconfirmation),
                Some(Retrieval::NotFound),
                Some(Retrieval::AmbiguousEntity),
                Some(Retrieval::ExtractionIncomplete),
                Some(Retrieval::EvidenceBudgetExceeded),
            ] {
                let mut status = base.clone();
                status.related_omission = Some(reason);
                status.retrieval_status = retrieval;
                let before = search_owner_bytes(&writer);
                let result = writer.record_memory_status(&target, status.clone());
                if mode == HistoryMode::Structured10
                    && matches!(
                        retrieval,
                        Some(Retrieval::Found | Retrieval::PendingReconfirmation)
                    )
                {
                    result.unwrap();
                    let mut saved = writer.snapshot().unwrap();
                    assert_eq!(
                        saved.state.runs.last().unwrap().memory.as_ref(),
                        Some(&status)
                    );
                    saved
                        .state
                        .runs
                        .last_mut()
                        .unwrap()
                        .configuration
                        .history_mode = HistoryMode::Legacy;
                    assert!(super::validate(&saved).is_err());
                } else {
                    assert!(result.is_err());
                    assert_eq!(search_owner_bytes(&writer), before);
                }
            }
        }
        writer.record_memory_status(&target, base).unwrap();
        assert!(
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
                .related_omission
                .is_none()
        );
    }
}

#[tokio::test]
async fn desktop_related_omission_clears_on_success_and_ready_refusal() {
    use polaris_memory::conversation::RelatedSummaryBudget;
    for cancel_ready in [false, true] {
        let (_root, mut writer, target) = setup_query_turns(
            "memory-project",
            "memory-session",
            HistoryMode::Structured10,
            11,
            "Rigel",
        );
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("memory.sqlite3");
        let (work, pending, response) = structured_pending(&mut writer, &target).await;
        writer
            .publish_structured_memory(&target, &work, &pending, &response, &database)
            .unwrap();
        let backend = SearchBackend::new("Rigel", false);
        let (runtime, mut owner) = crate::desktop_memory::DesktopMemory::pair(
            target,
            Arc::new(StrictHistory::new(backend.clone(), backend)),
            database,
            resources(),
        )
        .unwrap();
        let mut session = crate::session::Session::new();
        session.push_user("Rigel");
        session.desktop_memory = Some(runtime);
        session.memory_observation =
            Some(crate::conversation_memory::observation::MemoryObservation {
                mode: HistoryMode::Structured10,
                retrieval_status: Some(
                    polaris_desktop_protocol::snapshot::MemoryRetrievalStatus::Found,
                ),
                source_uris: vec![],
                reference_tokens: 0,
                related_omission: Some(RelatedSummaryBudget::RawProofWork),
            });
        let task = tokio::spawn(async move {
            let result = session.prepare_history().await;
            (session, result)
        });
        // Current-thread runtime: each synchronous owner poll replies to exactly
        // one awaited step (Snapshot, Structured, Check, then Ready).
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            for _ in 0..3 {
                while owner.pending_requests() == 0 {
                    tokio::task::yield_now().await;
                }
                owner.poll(Some(&mut writer), false);
            }
            while owner.pending_requests() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        let before_ready = search_owner_bytes(&writer);
        owner.poll(Some(&mut writer), cancel_ready);
        let (session, result) = task.await.unwrap();
        assert_eq!(
            session
                .memory_observation
                .as_ref()
                .and_then(|o| o.related_omission),
            None
        );
        if cancel_ready {
            assert!(result.is_err());
            assert!(session.memory_observation.is_none());
            assert_eq!(search_owner_bytes(&writer), before_ready);
        } else {
            assert!(result.unwrap().unwrap().content.contains("Rigel"));
            let saved = writer.snapshot().unwrap();
            let status = saved.state.runs.last().unwrap().memory.as_ref().unwrap();
            assert_eq!(status.phase, MemoryPhase::Ready);
            assert!(status.related_omission.is_none());
        }
    }
}
