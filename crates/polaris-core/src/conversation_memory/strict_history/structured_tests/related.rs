//! Common saved-owner retrieval through synthetic backends and actual dispatch.
use super::*;
use crate::conversation_memory::{related::SEARCH_URI, structured};

struct RelatedSummary(Arc<Summary>);
impl StrictSummaryProvider for RelatedSummary {
    fn summarize<'a>(&'a self, request: SummaryRequest) -> StrictFuture<'a, String> {
        Box::pin(async move {
            let mut result: serde_json::Value =
                serde_json::from_str(&self.0.summarize(request).await?)?;
            result["summary"]["facts"] = serde_json::json!(["Aster saved fact"]);
            Ok(result.to_string())
        })
    }
}

async fn dispatch(session: &crate::session::Session) -> Result<String, String> {
    let call = polaris_provider::ToolCall {
        id: "r34-read".into(),
        name: "read".into(),
        arguments: serde_json::json!({"path": SEARCH_URI}),
    };
    crate::agent::dispatch_conversation_read(
        session.persistence.as_ref(),
        None,
        session.strict_history.as_deref(),
        &call,
        SEARCH_URI,
        None,
    )
    .await
}

#[tokio::test]
async fn r34_common_automatic_keyword_fills_verified_context_without_embedding() {
    let (_root, store, db) = setup(11);
    let (_, summary, embedding) = backend();
    let history = StrictHistory::new(Arc::new(RelatedSummary(summary)), embedding.clone());
    let prepared = history.prepare(&store, &db, "Aster").await.unwrap();
    let context = prepared.structured.unwrap();
    assert_eq!(context.status, structured::packing::RetrievalStatus::Found);
    assert!(
        context.text.contains("Aster saved fact"),
        "{}",
        context.text
    );
    assert!(context.request_text().unwrap().1 <= 768);
    assert_eq!(embedding.queries.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn r34_saved_owner_dispatch_returns_historical_candidates() {
    let (_root, mut session, summary, embedding) = session_fixture();
    session.recover_history().await.unwrap();
    let result = dispatch(&session).await.unwrap();
    assert!(result.contains("saved fact"), "{result}");
    assert!(result.contains("not_found"), "{result}");
    assert!(result.contains("semantic"), "{result}");
    assert!(crate::budget::count_tokens(&result) <= 768);
    assert_eq!(summary.calls.load(Ordering::SeqCst), 1);
    assert_eq!(embedding.queries.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn r34_no_hit_does_not_receive_automatic_semantic_filler() {
    let (_root, store, db) = setup(11);
    let (history, _, embedding) = backend();
    let prepared = history.prepare(&store, &db, "unknown name").await.unwrap();
    let context = prepared.structured.unwrap();
    assert_eq!(
        context.status,
        structured::packing::RetrievalStatus::NotFound
    );
    assert!(!context.text.contains("saved fact"));
    assert_eq!(context.related_omission, None);
    assert_eq!(embedding.queries.load(Ordering::SeqCst), 0);
}

type QueryAction = Box<dyn FnOnce() -> io::Result<()> + Send>;

struct DuringQuery {
    inner: Arc<Embedder>,
    action: std::sync::Mutex<Option<QueryAction>>,
}
impl StrictEmbedder for DuringQuery {
    fn metadata(&self) -> &EmbeddingModel {
        self.inner.metadata()
    }
    fn embed_passage<'a>(&'a self, text: &'a str) -> StrictFuture<'a, Vec<f32>> {
        self.inner.embed_passage(text)
    }
    fn embed_query<'a>(&'a self, text: &'a str) -> StrictFuture<'a, Vec<Vec<f32>>> {
        Box::pin(async move {
            let action = self.action.lock().unwrap().take().unwrap();
            action()?;
            self.inner.embed_query(text).await
        })
    }
}

#[tokio::test]
async fn r34_query_embedding_releases_lock_and_rejects_changed_owner_or_forget() {
    for change in ["none", "generation", "input", "forget"] {
        let (_root, mut session, summary, embedding) = session_fixture();
        session.recover_history().await.unwrap();
        let saved = session.persistence.clone().unwrap();
        let session_id = saved.snapshot().unwrap().state.session_id;
        let touched = Arc::new(AtomicBool::new(false));
        let check = touched.clone();
        let action = move || -> io::Result<()> {
            // lock() is nonblocking; retaining the outer lock fails this test.
            let mut lock = saved.store.lock()?;
            check.store(true, Ordering::SeqCst);
            if change == "input" {
                lock.append(Message::user("changed accepted input"), true)?;
            }
            if matches!(change, "input" | "generation") {
                let snapshot = lock.snapshot()?;
                lock.publish(&snapshot, snapshot.state.clone())?;
            }
            if change == "forget" {
                MemoryStore::open(&saved.database)
                    .map_err(io::Error::other)?
                    .delete_session("p", &session_id)
                    .map_err(io::Error::other)?;
            }
            Ok(())
        };
        session.strict_history = Some(Arc::new(StrictHistory::new(
            summary.clone(),
            Arc::new(DuringQuery {
                inner: embedding.clone(),
                action: std::sync::Mutex::new(Some(Box::new(action))),
            }),
        )));
        let result = dispatch(&session).await;
        assert!(touched.load(Ordering::SeqCst));
        if change == "none" {
            assert!(result.unwrap().contains("saved fact"));
        } else {
            assert!(result.is_err(), "{change}: {result:?}");
        }
        assert_eq!(summary.calls.load(Ordering::SeqCst), 1);
        assert_eq!(embedding.queries.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn r34_search_rejects_missing_input_backend_and_injected_arguments() {
    let (_root, store, db) = setup(0);
    let (history, _, embedding) = backend();
    assert!(
        history
            .search_related(&store, &db)
            .await
            .unwrap_err()
            .to_string()
            .contains("受理入力")
    );
    assert_eq!(embedding.queries.load(Ordering::SeqCst), 0);
    let (_root, mut session, _, embedding) = session_fixture();
    session.recover_history().await.unwrap();
    for arguments in [
        serde_json::json!({"path": SEARCH_URI, "query": "injected"}),
        serde_json::json!({"path": SEARCH_URI, "offset": 1}),
        serde_json::json!({"path": format!("{SEARCH_URI}?query=injected")}),
        serde_json::json!({}),
    ] {
        let path = arguments
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or(SEARCH_URI)
            .to_owned();
        let call = polaris_provider::ToolCall {
            id: "invalid".into(),
            name: "read".into(),
            arguments,
        };
        assert!(
            crate::agent::dispatch_conversation_read(
                session.persistence.as_ref(),
                None,
                session.strict_history.as_deref(),
                &call,
                &path,
                None
            )
            .await
            .unwrap_err()
            .contains("予約URI")
        );
    }
    assert_eq!(embedding.queries.load(Ordering::SeqCst), 0);
    session.strict_history = None;
    assert!(dispatch(&session).await.is_err());
    assert_eq!(embedding.queries.load(Ordering::SeqCst), 0);
}

fn rebuild_index(
    saved: &crate::session_store::PersistedSession,
    edit: impl FnOnce(&mut PendingSummary),
) -> crate::session_store::PersistedSession {
    let snapshot = saved.snapshot().unwrap();
    let view = published_view(&snapshot.state).unwrap();
    assert_eq!(view.visible_ids.len(), 1);
    let mut pending = MemoryStore::open(&saved.database)
        .unwrap()
        .pending_summary(&view.scope, &view.visible_ids[0])
        .unwrap()
        .unwrap();
    edit(&mut pending);
    let response = saved
        .store
        .read_structured_attempt(snapshot.state.epoch, 1)
        .unwrap()
        .unwrap()
        .response
        .unwrap();
    let mut copy = saved.clone();
    copy.database = saved.data_root.join("replacement-index.sqlite3");
    let mut index = MemoryStore::open(&copy.database).unwrap();
    index.enable_structured_memory().unwrap();
    index.insert_pending_summary(&pending).unwrap();
    let mut initial = structured::sources::publication_view(&snapshot.state).unwrap();
    initial.visible_publication_ids.clear();
    index
        .insert_structured_delta(&response.delta, &initial)
        .unwrap();
    copy.snapshot().unwrap();
    copy
}

#[tokio::test]
async fn r34_related_search_verifies_all_index_rows_against_saved_summary_and_resources() {
    for change in ["none", "summary", "revision", "model", "source"] {
        let (_root, mut session, summary, embedding) = session_fixture();
        session.recover_history().await.unwrap();
        let copy = rebuild_index(
            session.persistence.as_ref().unwrap(),
            |pending| match change {
                "summary" => {
                    let mut value: serde_json::Value =
                        serde_json::from_str(&pending.summary).unwrap();
                    value["facts"] = serde_json::json!(["unrelated forged content"]);
                    pending.summary = value.to_string();
                    pending.summary_hash = content_hash(pending.summary.as_bytes());
                    pending.embedding.input_hash = pending.summary_hash.clone();
                }
                "revision" => pending.embedding.revision = "other-revision".into(),
                "model" => pending.model = "other-summary-model".into(),
                "source" => pending.source.raw_hash = content_hash(b"wrong raw"),
                _ => {}
            },
        );
        session.persistence = Some(copy.clone());
        let result = dispatch(&session).await;
        let automatic = session
            .strict_history
            .as_ref()
            .unwrap()
            .prepare(&copy.store, &copy.database, "Aster")
            .await;
        if change == "none" {
            assert!(result.unwrap().contains("saved fact"));
            assert!(automatic.is_ok());
        } else {
            assert!(result.is_err(), "{change}: {result:?}");
            assert!(
                automatic.is_err(),
                "{change} must not be silently filtered or omitted"
            );
        }
        assert_eq!(summary.calls.load(Ordering::SeqCst), 1);
        assert_eq!(embedding.queries.load(Ordering::SeqCst), 1);
    }
}

#[tokio::test]
async fn r34_related_search_refuses_changed_receipt_and_raw() {
    for change in ["receipt", "raw"] {
        let (_root, mut session, _, _) = session_fixture();
        session.recover_history().await.unwrap();
        let directory = session.persistence.as_ref().unwrap().store.directory();
        if change == "receipt" {
            let path = directory.join("structured-attempts/0/1.json");
            let mut receipt: serde_json::Value =
                serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
            let summary = receipt["response"]["summary"].as_str().unwrap();
            let mut value: serde_json::Value = serde_json::from_str(summary).unwrap();
            value["facts"] = serde_json::json!(["changed saved response"]);
            receipt["response"]["summary"] = value.to_string().into();
            std::fs::write(path, serde_json::to_vec(&receipt).unwrap()).unwrap();
        } else {
            std::fs::write(directory.join("raw.jsonl"), b"changed raw\n").unwrap();
        }
        assert!(dispatch(&session).await.is_err(), "{change}");
    }
}

fn latest_source_id(saved: &crate::session_store::PersistedSession) -> String {
    let snapshot = saved.snapshot().unwrap();
    let view = published_view(&snapshot.state).unwrap();
    let memory = MemoryStore::open(&saved.database).unwrap();
    view.visible_ids
        .iter()
        .find_map(|id| {
            memory
                .pending_summary(&view.scope, id)
                .unwrap()
                .map(|row| row.source.id)
        })
        .unwrap()
}

#[tokio::test]
async fn r34_actual_fork_uses_frozen_parent_grants_and_refuses_forgotten_ancestor() {
    let (_root, mut parent, _, _) = session_fixture();
    parent.recover_history().await.unwrap();
    let parent_saved = parent.persistence.clone().unwrap();
    let granted_source = latest_source_id(&parent_saved);
    let child_saved = parent_saved.fork(&"a".repeat(64)).unwrap();
    let mut child = crate::session::Session::new();
    child.attach(child_saved).unwrap();
    child.strict_history = parent.strict_history.clone();
    child.push_user("Aster");
    child.recover_history().await.unwrap();
    let result = dispatch(&child).await.unwrap();
    assert!(result.contains(&granted_source), "{result}");
    assert!(result.contains("found"), "{result}");
    parent.push_user("Aster new parent input");
    parent.recover_history().await.unwrap();
    let later_source = latest_source_id(&parent_saved);
    assert_ne!(later_source, granted_source);
    let result = dispatch(&child).await.unwrap();
    assert!(result.contains(&granted_source));
    assert!(!result.contains(&later_source), "{result}");
    MemoryStore::open(&parent_saved.database)
        .unwrap()
        .delete_session("p", &parent_saved.snapshot().unwrap().state.session_id)
        .unwrap();
    assert!(dispatch(&child).await.is_err());
}

struct WideEmbedder {
    model: EmbeddingModel,
    queries: AtomicUsize,
}
impl StrictEmbedder for WideEmbedder {
    fn metadata(&self) -> &EmbeddingModel {
        &self.model
    }
    fn embed_passage<'a>(&'a self, _: &'a str) -> StrictFuture<'a, Vec<f32>> {
        Box::pin(async { Ok(vec![0.12345679; 4096]) })
    }
    fn embed_query<'a>(&'a self, _: &'a str) -> StrictFuture<'a, Vec<Vec<f32>>> {
        self.queries.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(vec![vec![0.12345679; 4096]]) })
    }
}

#[tokio::test]
async fn r34_real_published_row_cap_preserves_mandatory_context_and_prevents_query_embedding() {
    use polaris_memory::conversation::RelatedSummaryBudget;
    let (_root, store, db) = setup(110);
    let (_, summary, _) = backend();
    let embedding = Arc::new(WideEmbedder {
        model: EmbeddingModel {
            model: "wide-fixture".into(),
            revision: "r34".into(),
            dimension: 4096,
        },
        queries: AtomicUsize::new(0),
    });
    let history = StrictHistory::new(summary.clone(), embedding.clone());
    let prepared = history.prepare(&store, &db, "Aster").await.unwrap();
    assert_eq!(summary.calls.load(Ordering::SeqCst), 100);
    assert_eq!(prepared.snapshot.state.visible_summary_ids.len(), 100);
    let context = prepared.structured.unwrap();
    assert_eq!(context.status, structured::packing::RetrievalStatus::Found);
    assert_eq!(
        context.related_omission,
        Some(RelatedSummaryBudget::SerializedRows)
    );
    let expected = {
        let lock = store.lock().unwrap();
        let snapshot = lock.snapshot().unwrap();
        let memory = MemoryStore::open_read_only(&db).unwrap();
        crate::conversation_memory::structured_owner::with_verified_receipts(
            &store,
            &snapshot,
            &memory,
            |view| related_history::mandatory_context(&store, &snapshot, &memory, view, "Aster"),
        )
        .unwrap()
    };
    assert_eq!(
        context.request_text().unwrap(),
        expected.request_text().unwrap()
    );
    assert_eq!(context.source_uris, expected.source_uris);
    assert!(context.request_text().unwrap().1 <= 768);
    let error = history.search_related(&store, &db).await.unwrap_err();
    assert!(matches!(
        error
            .get_ref()
            .and_then(|e| e.downcast_ref::<polaris_memory::Error>()),
        Some(polaris_memory::Error::RelatedSummaryBudgetExceeded(
            RelatedSummaryBudget::SerializedRows
        ))
    ));
    assert_eq!(embedding.queries.load(Ordering::SeqCst), 0);
    // Rebuild the same valid published set through public index APIs. A
    // saved-owner mismatch in the scanned prefix must outrank optional omission.
    for corrupt in [false, true] {
        let replacement = _root.path().join(format!("cap-index-{corrupt}.sqlite3"));
        let original = MemoryStore::open_read_only(&db).unwrap();
        let mut index = MemoryStore::open(&replacement).unwrap();
        index.enable_structured_memory().unwrap();
        let mut view = structured::sources::publication_view(&prepared.snapshot.state).unwrap();
        view.visible_publication_ids.clear();
        for turn in 1..=100 {
            let response = store
                .read_structured_attempt(prepared.snapshot.state.epoch, turn)
                .unwrap()
                .unwrap()
                .response
                .unwrap();
            let publication = &response.delta.publication;
            let id = format!("strict10-{turn}-{}", &publication.raw_hash[..16]);
            let scope = Scope {
                project_id: publication.scope.project_id.clone(),
                session_id: publication.scope.session_id.clone(),
                epoch: publication.scope.epoch,
                generation: publication.scope.generation,
            };
            let mut row = original.pending_summary(&scope, &id).unwrap().unwrap();
            if corrupt && turn == 1 {
                row.model = "altered-summary-model".into();
            }
            index.insert_pending_summary(&row).unwrap();
            view.scope = publication.scope.clone();
            index
                .insert_structured_delta(&response.delta, &view)
                .unwrap();
            view.visible_publication_ids
                .push(publication.publication_id.clone());
        }
        drop(index);
        drop(original);
        let result = history.prepare(&store, &replacement, "Aster").await;
        if corrupt {
            assert!(
                result.is_err(),
                "saved-owner corruption must not become related omission"
            );
        } else {
            let control = result.unwrap().structured.unwrap();
            assert_eq!(
                control.related_omission,
                Some(RelatedSummaryBudget::SerializedRows)
            );
            assert_eq!(
                control.request_text().unwrap(),
                expected.request_text().unwrap()
            );
        }
    }
}

#[derive(Default)]
struct CaptureModel(std::sync::Mutex<Vec<polaris_provider::CompletionRequest>>);
#[async_trait::async_trait]
impl polaris_provider::Provider for CaptureModel {
    async fn complete(
        &self,
        request: polaris_provider::CompletionRequest,
    ) -> Result<polaris_provider::CompletionResponse, polaris_provider::ProviderError> {
        self.0.lock().unwrap().push(request);
        Ok(polaris_provider::CompletionResponse {
            text: "done".into(),
            ..Default::default()
        })
    }
}

#[tokio::test]
async fn r34_run_records_query_budget_omission_without_displacing_evidence_and_resets_it() {
    use polaris_memory::conversation::RelatedSummaryBudget;
    let (root, mut session, _, embedding) = session_fixture();
    session.recover_history().await.unwrap();
    let query = format!(
        "Aster {}",
        (0..128)
            .map(|n| format!("term{n}"))
            .collect::<Vec<_>>()
            .join(" ")
    );
    session.push_user(&query);
    let model = Arc::new(CaptureModel::default());
    let audit_path = root.path().join("audit.jsonl");
    let audit = Arc::new(tokio::sync::Mutex::new(
        crate::audit::AuditLog::open(&audit_path).unwrap(),
    ));
    let policy =
        polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[]).unwrap();
    let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
    let mut approver = crate::agent::AutoApprove;
    let mut tools = crate::agent::ToolContext {
        sandbox: &policy,
        helper: Path::new("/must-not-run"),
        gate: &mut gate,
        approver: &mut approver,
    };
    let prompt = crate::prompt::assemble_always_on("", "", &[]);
    crate::agent::run(
        model.as_ref(),
        &mut session,
        audit,
        &mut crate::stop::StopTracker::new(2),
        &prompt,
        &[],
        &[],
        model.clone(),
        1,
        1,
        None,
        &mut tools,
    )
    .await
    .unwrap();
    assert_eq!(
        session
            .memory_observation
            .as_ref()
            .and_then(|o| o.related_omission),
        Some(RelatedSummaryBudget::QueryTerms)
    );
    let audit_text = std::fs::read_to_string(audit_path).unwrap();
    assert!(audit_text.contains("related_omitted=query_terms"));
    let preparation: Vec<serde_json::Value> = audit_text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|record: &serde_json::Value| {
            record["tool"] == "conversation-memory" && record["detail"] == "preparation"
        })
        .collect();
    assert_eq!(
        preparation.len(),
        1,
        "one accepted memory observation must reach the run audit"
    );
    assert!(
        preparation[0]["result"]
            .as_str()
            .unwrap()
            .contains("status=found")
    );
    {
        let requests = model.0.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let evidence = requests[0]
            .messages
            .iter()
            .find(|message| message.content.contains("記憶の取得分類"))
            .unwrap();
        assert!(evidence.content.contains("production"));
        assert!(crate::budget::count_tokens(&evidence.content) <= 768);
        assert!(!evidence.content.contains("related_omitted"));
    }
    assert_eq!(embedding.queries.load(Ordering::SeqCst), 0);
    assert!(
        dispatch(&session)
            .await
            .unwrap_err()
            .contains("query_terms")
    );
    assert_eq!(embedding.queries.load(Ordering::SeqCst), 0);
    let mut failing = session.clone();
    failing.persistence_error = Some("fixture persistence failure".into());
    let failed_audit = Arc::new(tokio::sync::Mutex::new(
        crate::audit::AuditLog::open(&root.path().join("failed-audit.jsonl")).unwrap(),
    ));
    assert!(
        crate::agent::run(
            model.as_ref(),
            &mut failing,
            failed_audit,
            &mut crate::stop::StopTracker::new(2),
            &prompt,
            &[],
            &[],
            model.clone(),
            1,
            1,
            None,
            &mut tools
        )
        .await
        .is_err()
    );
    assert!(failing.memory_observation.is_none());
    assert_eq!(model.0.lock().unwrap().len(), 1);
    assert!(failing.prepare_history().await.is_err());
    assert_eq!(
        failing
            .memory_observation
            .as_ref()
            .and_then(|o| o.related_omission),
        None
    );
    session.push_user("Aster");
    session.prepare_history().await.unwrap();
    assert_eq!(
        session
            .memory_observation
            .as_ref()
            .and_then(|o| o.related_omission),
        None
    );
}
