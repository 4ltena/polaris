//! Exercises the CLI history constructor through persisted retrieval and a counting judge.

use super::build_strict_history;
use polaris_core::{
    agent::{self, ToolContext},
    audit::AuditLog,
    conversation_memory::{
        EmbeddingModel, StrictEmbedder, StrictFuture, StrictSummaryProvider, SummaryRequest,
    },
    conversation_state::HistoryMode,
    prompt,
    session::Session,
    session_store::PersistedSession,
    stop::StopTracker,
};
use polaris_provider::{
    CompletionRequest, CompletionResponse, Message, Provider, ToolCall,
    decision::{Answer, Decider, Decision, DecisionError, Question},
};
use std::{
    collections::{BTreeMap, VecDeque},
    io,
    sync::atomic::{AtomicUsize, Ordering},
    sync::{Arc, Mutex},
};

struct Summary;

impl StrictSummaryProvider for Summary {
    fn summarize<'a>(&'a self, request: SummaryRequest) -> StrictFuture<'a, String> {
        Box::pin(async move {
            let input = request
                .structured_sources
                .ok_or_else(|| io::Error::other("structured input was not requested"))?;
            Ok(serde_json::json!({
                "summary": {
                    "facts": [format!("saved fact {}", request.source_turn_id)],
                    "decisions": [], "constraints": [], "corrections": [], "open_items": [],
                    "source_turn_ids": [request.source_turn_id],
                },
                "processed_source_ids": input.extraction_source_ids,
                "entity_candidates": [], "fact_deltas": [],
            })
            .to_string())
        })
    }
}

struct Embedder(EmbeddingModel);

impl StrictEmbedder for Embedder {
    fn metadata(&self) -> &EmbeddingModel {
        &self.0
    }

    fn embed_passage<'a>(&'a self, _: &'a str) -> StrictFuture<'a, Vec<f32>> {
        Box::pin(async { Ok(vec![1.0, 0.0]) })
    }

    fn embed_query<'a>(&'a self, _: &'a str) -> StrictFuture<'a, Vec<Vec<f32>>> {
        Box::pin(async { Ok(vec![vec![1.0, 0.0]]) })
    }
}

struct CountingDecider(AtomicUsize);

#[async_trait::async_trait]
impl Decider for CountingDecider {
    fn capabilities(&self) -> polaris_provider::decision::DecisionCapabilities {
        // This fixture checks dispatch through the historical band policy.
        polaris_provider::decision::DecisionCapabilities {
            band_calibration: polaris_provider::decision::BandCalibration::LegacyJev,
            ..Default::default()
        }
    }

    async fn decide(
        &self,
        _: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Decision {
            answers: questions
                .keys()
                .map(|id| {
                    (
                        id.clone(),
                        Answer {
                            noul: Some(0.9),
                            ..Default::default()
                        },
                    )
                })
                .collect(),
            input_tokens: Some(1),
            output_tokens: Some(1),
        })
    }
}

struct Scripted(Mutex<VecDeque<CompletionResponse>>);

#[async_trait::async_trait]
impl Provider for Scripted {
    async fn complete(
        &self,
        _: CompletionRequest,
    ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
        self.0
            .lock()
            .expect("responses")
            .pop_front()
            .ok_or_else(|| {
                polaris_provider::ProviderError::Unsupported("unexpected request".into())
            })
    }
}

struct NeverCalled;

#[async_trait::async_trait]
impl Provider for NeverCalled {
    async fn complete(
        &self,
        _: CompletionRequest,
    ) -> Result<CompletionResponse, polaris_provider::ProviderError> {
        panic!("spawn provider was used")
    }
}

struct Allow;

impl polaris_core::approval::Approver for Allow {
    fn ask(&mut self, _: &str) -> polaris_core::approval::Decision {
        polaris_core::approval::Decision::Allow
    }
}

fn structured_session(root: &tempfile::TempDir, judge: Arc<CountingDecider>) -> Session {
    let saved = PersistedSession::create(
        root.path(),
        &root.path().join("memory.sqlite3"),
        "fixture",
        HistoryMode::Strict10,
        None,
    )
    .expect("session");
    let mut state = saved.snapshot().expect("snapshot").state;
    state.history_mode = HistoryMode::Structured10;
    std::fs::write(
        saved.store.directory().join("state.json"),
        serde_json::to_vec(&state).expect("state json"),
    )
    .expect("state");
    let mut lock = saved.store.lock().expect("lock");
    for turn in 1..=13 {
        lock.append(Message::user(format!("saved turn {turn}")), true)
            .expect("append");
    }
    let snapshot = lock.snapshot().expect("raw snapshot");
    lock.publish(&snapshot, snapshot.state.clone())
        .expect("publish");
    let published = lock.snapshot().expect("published snapshot");
    drop(lock);
    saved
        .accept_publication(&published)
        .expect("accept publication");

    let mut session = Session::new();
    session.attach(saved).expect("attach");
    session.strict_history = Some(build_strict_history(
        Arc::new(Summary),
        Arc::new(Embedder(EmbeddingModel {
            model: "fixture".into(),
            revision: "fixed".into(),
            dimension: 2,
        })),
        Some(judge),
    ));
    session
}

#[tokio::test]
async fn cli_strict_history_passes_its_decider_to_related_summary_ranking() {
    let root = tempfile::tempdir().expect("temp dir");
    let judge = Arc::new(CountingDecider(AtomicUsize::new(0)));
    let mut session = structured_session(&root, judge.clone());
    session.recover_history().await.expect("recover history");
    session.push_user("find saved facts");

    let provider = Scripted(Mutex::new(VecDeque::from([
        CompletionResponse {
            tool_calls: vec![ToolCall {
                id: "related".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path": "conversation://search/related"}),
            }],
            ..Default::default()
        },
        CompletionResponse {
            text: "done".into(),
            ..Default::default()
        },
    ])));
    let always_on = prompt::assemble_always_on("", "", &[]);
    let sandbox =
        polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::FullAccess, &[])
            .expect("sandbox");
    let mut gate = polaris_core::approval::Gate::new(polaris_core::approval::ApprovalPolicy::Never);
    let mut approver = Allow;
    let mut context = ToolContext {
        sandbox: &sandbox,
        helper: std::path::Path::new("/bin/true"),
        gate: &mut gate,
        approver: &mut approver,
    };
    let audit = Arc::new(tokio::sync::Mutex::new(
        AuditLog::open(&root.path().join("audit.jsonl")).expect("audit"),
    ));
    let mut stop = StopTracker::new(3);
    let outcome = agent::run(
        &provider,
        &mut session,
        audit,
        &mut stop,
        &always_on,
        &[],
        &[],
        Arc::new(NeverCalled),
        1,
        1,
        None,
        &mut context,
    )
    .await
    .expect("run");

    assert_eq!(outcome.text, "done");
    assert!(
        judge.0.load(Ordering::SeqCst) >= 1,
        "related candidates were not judged"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn linux_strict_history_refuses_before_provider_or_runtime_state() {
    let root = tempfile::tempdir().unwrap();
    let runtime_home = root.path().join("runtime-home");
    let private_root = root.path().join("private-state");
    let config = polaris_core::config::EmbeddingConfig {
        model_path: Some(root.path().join("model")),
        revision: Some("fixture-revision".into()),
        ..Default::default()
    };
    let error = super::strict_history(
        &config,
        &runtime_home,
        "never-provider-fixture",
        &private_root,
        polaris_provider::attempts::AttemptLedger::default(),
        "fixture-parent",
        None,
    )
    .err()
    .expect("Linux has no verified runtime image");
    assert_eq!(
        error,
        "The strict10 read-only runtime image is unavailable in this environment"
    );
    assert!(!runtime_home.exists());
    assert!(!private_root.exists());
}
