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
                        text: format!("turn {number}: Rigel"),
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
                    history_mode: HistoryMode::Strict10,
                },
                policy_revision: DecimalU64::new(0),
            },
        )
        .unwrap();
    for number in 1..=10 {
        let target = accept(&mut writer, number);
        finish(&mut writer, &target, number);
    }
    let target = accept(&mut writer, 11);
    writer.bind_memory(&target, resources()).unwrap();
    (root, writer, target)
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
async fn resource_rebinding_and_raw_byte_tampering_are_refused() {
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
    corrupted
        .state
        .conversation_memory
        .as_mut()
        .unwrap()
        .sources[0]
        .source
        .raw_hash = content_hash(b"wrong published prefix");
    writer.commit(corrupted, false).unwrap();
    assert!(writer.read_memory_source(&target, &database, &uri).is_err());
}
