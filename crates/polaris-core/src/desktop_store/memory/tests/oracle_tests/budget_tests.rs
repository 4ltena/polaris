//! An authored long-plan variant tests the real preparation budget boundary.
use super::*;
use polaris_desktop_protocol::snapshot::MemoryRetrievalStatus;

const OVERFLOW: &str = include_str!("../../../../../tests/fixtures/m3-budget-overflow.json");

struct BudgetSummary {
    base: AnnotatedSummary,
    overflow: Value,
    annotated: AtomicUsize,
}

impl StrictSummaryProvider for BudgetSummary {
    fn summarize<'a>(&'a self, mut request: SummaryRequest) -> FutureResult<'a, String> {
        Box::pin(async move {
            let input = request.structured_sources.as_mut().unwrap();
            let mut expanded_source = None;
            for source in &mut input.sources {
                if !input.extraction_source_ids.contains(&source.source_id)
                    || content_hash(source.content.as_bytes())
                        != self.overflow["source_text_sha256"]
                {
                    continue;
                }
                assert_eq!(source.role, polaris_provider::Role::User);
                let prefix = self.base.fixture["inputs"][0]["text"].as_str().unwrap();
                assert!(source.content.starts_with(prefix));
                assert!(
                    source
                        .content
                        .contains(self.overflow["plan_value"].as_str().unwrap())
                );
                assert!(expanded_source.replace(source.source_id.clone()).is_none());
                // Reuse the authored annotations for the unchanged T1 prefix. This
                // mock-internal copy does not change host raw bytes, IDs or coverage.
                // Its evidence span remains a literal prefix of the expanded source.
                source.content = prefix.to_owned();
                self.annotated.fetch_add(1, Ordering::SeqCst);
            }
            let mut response: Value = serde_json::from_str(&self.base.summarize(request).await?)?;
            if let Some(source) = expanded_source {
                let plans: Vec<_> = response["fact_deltas"]
                    .as_array_mut()
                    .unwrap()
                    .iter_mut()
                    .filter(|fact| fact["source_id"] == source && fact["fact_kind"] == "plan")
                    .collect();
                assert_eq!(plans.len(), 1);
                // The value is an exact literal in the received source; no query,
                // expected status or measured token total selects this annotation.
                for plan in plans {
                    plan["value"] = self.overflow["plan_value"].clone();
                }
            }
            Ok(response.to_string())
        })
    }
}

#[tokio::test]
async fn authored_oracle_mandatory_budget_blocks_partial_preparation_after_positive_control() {
    let fixture = fixture();
    assert_eq!(
        content_hash(OVERFLOW.as_bytes()),
        "90a8ba478ef680a3383a0907762e13c14511aff04300b1640b30028fce9919ac"
    );
    let overflow: Value = serde_json::from_str(OVERFLOW).unwrap();
    assert_eq!(
        overflow["base_fixture_sha256"],
        content_hash(FIXTURE.as_bytes())
    );
    assert_eq!(overflow["replacement_input"], "T1");
    assert_eq!(
        content_hash(overflow["source_text"].as_str().unwrap().as_bytes()),
        overflow["source_text_sha256"]
    );
    assert_eq!(overflow["case_id"], fixture["negative_cases"][6]["id"]);
    assert_eq!(
        fixture["negative_cases"][6]["expect"],
        "evidence_budget_exceeded_no_setting_write"
    );
    let plan_tokens = crate::budget::count_tokens(overflow["plan_value"].as_str().unwrap());
    assert!(plan_tokens > overflow["reference_limit"].as_u64().unwrap() as usize);
    for expanded in [false, true] {
        let (_root, mut writer) = new_writer();
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("memory.sqlite3");
        let summary = Arc::new(BudgetSummary {
            base: AnnotatedSummary {
                fixture: fixture.clone(),
                calls: AtomicUsize::new(0),
            },
            overflow: overflow.clone(),
            annotated: AtomicUsize::new(0),
        });
        let embed = Arc::new(CountedEmbed {
            model: EmbeddingModel {
                model: "fixture".into(),
                revision: "fixed".into(),
                dimension: 2,
            },
            passages: AtomicUsize::new(0),
        });
        for turn in 1..=10 {
            let text = if turn == 1 && expanded {
                overflow["source_text"].as_str().unwrap().to_owned()
            } else if turn <= 5 {
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
            let target = accept_text(&mut writer, turn, &text);
            finish(&mut writer, &target, turn);
        }
        let target = accept_text(
            &mut writer,
            overflow["query_turn"].as_u64().unwrap(),
            fixture["query"].as_str().unwrap(),
        );
        writer.bind_memory(&target, resources()).unwrap();
        let before = writer.snapshot().unwrap().marker.raw_hash;
        let result = prepare_result(
            &mut writer,
            &target,
            &database,
            summary.clone(),
            embed.clone(),
        )
        .await;
        assert_eq!(summary.base.calls.load(Ordering::SeqCst), 1);
        assert_eq!(
            summary.annotated.load(Ordering::SeqCst),
            usize::from(expanded)
        );
        assert_eq!(embed.passages.load(Ordering::SeqCst), 1);
        let saved = writer.snapshot().unwrap();
        assert_eq!(saved.marker.raw_hash, before);
        let status = saved.state.runs.last().unwrap().memory.as_ref().unwrap();
        verify_agent_observation(
            &mut writer,
            &target,
            &database,
            summary.clone(),
            embed.clone(),
            expanded,
        )
        .await;
        if !expanded {
            let evidence = result.unwrap().1.unwrap();
            assert_eq!(
                status.retrieval_status,
                Some(MemoryRetrievalStatus::PendingReconfirmation)
            );
            verify_authored_expectations(&writer, &target, &database, &fixture);
            verify_bundles(&writer, &target, &database, &evidence.content, &fixture);
            continue;
        }
        let error = result.unwrap_err();
        let blocked = error
            .get_ref()
            .unwrap()
            .downcast_ref::<crate::conversation_memory::observation::MemoryPreparationBlocked>()
            .unwrap();
        assert_eq!(
            blocked.observation.retrieval_status,
            Some(MemoryRetrievalStatus::EvidenceBudgetExceeded)
        );
        assert_eq!(blocked.observation.source_uris, status.retrieval_sources);
        assert_eq!(
            blocked.observation.reference_tokens,
            status.reference_tokens.get()
        );
        assert!(blocked.observation.related_omission.is_none());
        assert_eq!(
            status.retrieval_status,
            Some(MemoryRetrievalStatus::EvidenceBudgetExceeded)
        );
        assert_eq!(status.reference_tokens.get(), 0);
        assert!(status.retrieval_sources.is_empty());
        let snapshot = writer.memory_snapshot(&target).unwrap();
        let context = writer
            .prepare_structured_memory(&target, &snapshot, &database)
            .unwrap();
        assert_eq!(
            context.status,
            MemoryRetrievalStatus::EvidenceBudgetExceeded
        );
        assert!(context.text.is_empty() && context.source_uris.is_empty());
        let memory = saved.state.conversation_memory.as_ref().unwrap();
        let delta = &memory.attempts[0]
            .structured
            .as_ref()
            .unwrap()
            .response
            .as_ref()
            .unwrap()
            .delta;
        let ids = delta
            .entities
            .iter()
            .map(|e| e.entity_id.clone())
            .collect::<Vec<_>>();
        let rows = MemoryStore::open(&database)
            .unwrap()
            .structured_retrieval_evidence(&snapshot.structured_view().unwrap(), &ids)
            .unwrap();
        let plan = rows
            .facts
            .iter()
            .find(|f| f.row.fact_kind == "plan")
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&plan.row.value_json).unwrap(),
            overflow["plan_value"]
        );
        assert_eq!(
            serde_json::to_value(plan.row.status).unwrap(),
            "needs_reconfirmation"
        );
        let old = rows.facts.iter().find(|f| f.row.value_json == "3").unwrap();
        assert_eq!(serde_json::to_value(old.row.status).unwrap(), "superseded");
        assert_eq!(old.row.reason, "withdrawn");
        assert!(
            rows.dependencies
                .iter()
                .any(|d| d.row.dependent_fact_id == plan.row.fact_id
                    && d.row.dependency_fact_id == old.row.fact_id)
        );
        // Overflow withholds request evidence, not durable state or diagnostic raw access.
        let uri = crate::conversation_memory::structured::sources::source_uri(
            &delta.publication.publication_id,
            &delta.entities[0].origin_source_id,
        )
        .unwrap();
        let page = writer.read_memory_source(&target, &database, &uri).unwrap();
        assert!(page.contains("turn=1 ") && page.contains(&delta.publication.raw_hash));
        println!(
            "authored_oracle_budget plan_reference_tokens={plan_tokens} reference_tokens=0 preparation_blocked=true semantic_classification_measured=false"
        );
        // The actual agent/tool file-action assertion remains in M12; no expected
        // file bytes are written here to manufacture that acceptance result.
    }
}

async fn verify_agent_observation(
    writer: &mut Writer,
    target: &RunTarget,
    database: &std::path::Path,
    summary: Arc<BudgetSummary>,
    embed: Arc<CountedEmbed>,
    blocked: bool,
) {
    use polaris_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
    struct Main {
        calls: AtomicUsize,
        planned_write: Option<polaris_provider::ToolCall>,
    }
    #[async_trait::async_trait]
    impl Provider for Main {
        async fn complete(
            &self,
            _: CompletionRequest,
        ) -> Result<CompletionResponse, ProviderError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Ok(CompletionResponse {
                text: "observed".into(),
                tool_calls: self.planned_write.iter().cloned().collect(),
                ..Default::default()
            })
        }
    }
    let (runtime, mut owner) = crate::desktop_memory::DesktopMemory::pair(
        target.clone(),
        Arc::new(StrictHistory::new(summary, embed)),
        database.to_owned(),
        resources(),
    )
    .unwrap();
    let root = tempfile::tempdir().unwrap();
    let fixture = fixture();
    let setting = root.path().join(
        fixture["setting_fixture"]["relative_path"]
            .as_str()
            .unwrap(),
    );
    std::fs::write(
        &setting,
        fixture["setting_fixture"]["initial_content"]
            .as_str()
            .unwrap(),
    )
    .unwrap();
    let before = content_hash(&std::fs::read(&setting).unwrap());
    assert_eq!(before, fixture["setting_fixture"]["initial_sha256"]);
    let audit_path = root.path().join("audit.jsonl");
    let audit = Arc::new(tokio::sync::Mutex::new(
        crate::audit::AuditLog::open(&audit_path).unwrap(),
    ));
    let main = Arc::new(Main {
        calls: AtomicUsize::new(0),
        // If preparation ever leaks through, this provider would try to write.
        planned_write: blocked.then(|| polaris_provider::ToolCall {
            id: "m12-budget-must-not-dispatch".into(),
            name: "write".into(),
            arguments: json!({"path": setting, "content": fixture["positive_control"]["expected_content"]}),
        }),
    });
    let model = main.clone();
    let (events, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let task = async move {
        let mut session = crate::session::Session::new();
        session.push_user("旧Lumenの本番再試行設定を反映して。");
        session.desktop_memory = Some(runtime);
        let sandbox =
            polaris_sandbox::SandboxPolicy::new(polaris_sandbox::SandboxMode::ReadOnly, &[])
                .unwrap();
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Never);
        let mut approver = crate::agent::AutoApprove;
        let mut tools = crate::agent::ToolContext {
            sandbox: &sandbox,
            helper: std::path::Path::new("/must-not-run"),
            gate: &mut gate,
            approver: &mut approver,
        };
        let result = crate::agent::run(
            model.as_ref(),
            &mut session,
            audit,
            &mut crate::stop::StopTracker::new(2),
            &crate::prompt::assemble_always_on("", "", &[]),
            &[],
            &[],
            model.clone(),
            1,
            1,
            Some(events),
            &mut tools,
        )
        .await;
        (session, result)
    };
    tokio::pin!(task);
    let (session, result) = tokio::time::timeout(std::time::Duration::from_secs(20), async {
        loop {
            tokio::select! {
                result = &mut task => break result,
                _ = tokio::task::yield_now() => { owner.poll(Some(writer), false); }
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(result.is_err(), blocked);
    assert_eq!(main.calls.load(Ordering::SeqCst), usize::from(!blocked));
    assert_eq!(content_hash(&std::fs::read(&setting).unwrap()), before);
    let observation = session.memory_observation.as_ref().unwrap();
    let saved = writer.snapshot().unwrap();
    let status = saved.state.runs.last().unwrap().memory.as_ref().unwrap();
    assert_eq!(observation.retrieval_status, status.retrieval_status);
    assert_eq!(observation.source_uris, status.retrieval_sources);
    assert_eq!(observation.reference_tokens, status.reference_tokens.get());
    let mut observed_events = vec![];
    while let Ok(event) = receiver.try_recv() {
        assert!(!matches!(
            &event,
            crate::AgentEvent::ToolStarted { .. }
                | crate::AgentEvent::ToolFinished { .. }
                | crate::AgentEvent::SpawnStarted { .. }
        ));
        if let crate::AgentEvent::MemoryPrepared(observed) = event {
            observed_events.push(observed);
        }
    }
    assert_eq!(
        observed_events.as_slice(),
        std::slice::from_ref(observation)
    );
    let audit_rows: Vec<serde_json::Value> = std::fs::read_to_string(audit_path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert!(audit_rows.iter().all(|row| {
        polaris_tools::all_specs()
            .iter()
            .all(|tool| row["tool"] != tool.name)
    }));
    let rows: Vec<_> = audit_rows
        .iter()
        .filter(|row| row["tool"] == "conversation-memory" && row["detail"] == "preparation")
        .collect();
    assert_eq!(rows.len(), 1);
    let audit = rows[0]["result"].as_str().unwrap();
    assert!(audit.contains(if blocked {
        "status=evidence_budget_exceeded"
    } else {
        "status=pending_reconfirmation"
    }));
    assert!(
        !audit.contains("conversation://")
            && !audit.contains("Lumen")
            && !audit.contains("sync.retry_limit")
    );
    println!(
        "authored_m12_budget blocked={blocked} file_before_sha={before} file_after_sha={} main_calls={} tool_calls=0 semantic_classification_measured=false",
        content_hash(&std::fs::read(&setting).unwrap()),
        main.calls.load(Ordering::SeqCst)
    );
}
