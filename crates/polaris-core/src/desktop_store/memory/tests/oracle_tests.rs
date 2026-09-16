//! Authored source annotations exercise durable memory; they do not measure model extraction.

use super::*;
use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};

mod action_tests;
mod authority_tests;
mod budget_tests;
mod failure_tests;

const FIXTURE: &str = include_str!("../../../../tests/fixtures/m3-rename-withdrawal.json");

fn fixture() -> Value {
    assert_eq!(
        content_hash(FIXTURE.as_bytes()),
        "6a4f337538754c8430eb471e4660c8b22750815723ae3f6740ddaf063f857825"
    );
    let value: Value = serde_json::from_str(FIXTURE).unwrap();
    for input in value["inputs"].as_array().unwrap() {
        assert_eq!(
            content_hash(input["text"].as_str().unwrap().as_bytes()),
            input["input_sha256"]
        );
    }
    assert_eq!(
        content_hash(
            value["setting_fixture"]["initial_content"]
                .as_str()
                .unwrap()
                .as_bytes()
        ),
        value["setting_fixture"]["initial_sha256"]
    );
    value
}

struct AnnotatedSummary {
    fixture: Value,
    calls: AtomicUsize,
}
impl StrictSummaryProvider for AnnotatedSummary {
    fn summarize<'a>(&'a self, request: SummaryRequest) -> FutureResult<'a, String> {
        assert!(request.messages.is_empty());
        self.calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            let input = request.structured_sources.unwrap();
            let mut entities = vec![];
            let mut facts = vec![];
            let mut relations = vec![];
            // Match immutable authored inputs only within the actual extraction scope.
            // Summary-only targets and the later query cannot introduce annotations.
            for source in input
                .sources
                .iter()
                .filter(|source| input.extraction_source_ids.contains(&source.source_id))
            {
                if source.role != polaris_provider::Role::User {
                    continue;
                }
                let Some(index) =
                    self.fixture["inputs"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .position(|input| {
                            input["input_sha256"] == content_hash(source.content.as_bytes())
                        })
                else {
                    continue;
                };
                let base = json!({"source_id":source.source_id,"evidence_span":{"start":0,"length":source.content.len()},"evidence_text":source.content,"speech_act":"assert","ordinal":0});
                let entity = |kind: &str, name: &str, project: &str, ordinal: u32| {
                    let mut candidate = base.clone();
                    candidate.as_object_mut().unwrap().extend(json!({"kind":kind,"name":name,"subject_project":project,"ordinal":ordinal}).as_object().unwrap().clone());
                    candidate
                };
                let fact = |kind: &str,
                            value: Value,
                            relation: &str,
                            speech: &str,
                            ordinal: u32| {
                    let mut candidate = base.clone();
                    candidate.as_object_mut().unwrap().extend(json!({"entity_kind":"configuration_key","entity_name":"sync.retry_limit","subject_project":"A","fact_kind":kind,"value":value,"unit":"retries","condition":"production","relation":relation,"speech_act":speech,"ordinal":ordinal}).as_object().unwrap().clone());
                    candidate
                };
                match index {
                    0 => {
                        entities.push(entity("product", "Lumen", "A", 0));
                        entities.push(entity("configuration_key", "sync.retry_limit", "A", 1));
                        facts.push(fact("retry_limit", json!(3), "assert", "assert", 0));
                        let mut plan = fact("plan", json!("P1"), "depends_on", "assert", 1);
                        plan["depends_on"] = json!([{"entity_kind":"configuration_key","entity_name":"sync.retry_limit","subject_project":"A","fact_kind":"retry_limit","condition":"production"}]);
                        facts.push(plan);
                        let mut relation = base.clone();
                        relation.as_object_mut().unwrap().remove("ordinal");
                        relation["from"] = json!({"kind":"configuration_key","name":"sync.retry_limit","subject_project":"A"});
                        relation["to"] =
                            json!({"kind":"product","name":"Lumen","subject_project":"A"});
                        relation["relation"] = json!("belongs_to");
                        relations.push(relation);
                    }
                    1 => {
                        let mut rename = entity("product", "Aster", "A", 0);
                        rename["renamed_from"] = json!("Lumen");
                        rename["speech_act"] = json!("correct");
                        entities.push(rename);
                        let mut same_setting =
                            entity("configuration_key", "sync.retry_limit", "A", 1);
                        same_setting["same_as"] = json!("sync.retry_limit");
                        entities.push(same_setting);
                    }
                    2 => {
                        entities.push(entity("product", "Lumen", "B", 0));
                        let mut relation = base.clone();
                        relation.as_object_mut().unwrap().remove("ordinal");
                        relation["from"] =
                            json!({"kind":"product","name":"Lumen","subject_project":"B"});
                        relation["to"] =
                            json!({"kind":"product","name":"Lumen","subject_project":"A"});
                        relation["relation"] = json!("distinct_from");
                        relations.push(relation);
                    }
                    3 => facts.push(fact("retry_limit", json!(5), "propose", "propose", 0)),
                    4 => facts.push(fact("retry_limit", Value::Null, "withdraw", "withdraw", 0)),
                    _ => unreachable!(),
                }
            }
            Ok(json!({
                "summary":{"facts":["確認"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[request.source_turn_id]},
                "processed_source_ids":input.extraction_source_ids,
                "entity_candidates":entities,"fact_deltas":facts,"entity_relations":relations
            }).to_string())
        })
    }
}

struct CountedEmbed {
    model: EmbeddingModel,
    passages: AtomicUsize,
}
impl StrictEmbedder for CountedEmbed {
    fn metadata(&self) -> &EmbeddingModel {
        &self.model
    }
    fn embed_passage<'a>(&'a self, _: &'a str) -> FutureResult<'a, Vec<f32>> {
        self.passages.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(vec![1.0, 0.0]) })
    }
    fn embed_query<'a>(&'a self, _: &'a str) -> FutureResult<'a, Vec<Vec<f32>>> {
        panic!("automatic structured retrieval must not embed the query")
    }
}

fn new_writer() -> (PrototypeRoot, Writer) {
    new_writer_named("project-A", "session-A")
}

fn new_writer_named(project: &str, session: &str) -> (PrototypeRoot, Writer) {
    let root = PrototypeRoot::new().unwrap();
    let writer = root
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
                    provider: "fixture".into(),
                    model: "fixture".into(),
                    effort: "medium".into(),
                    history_mode: HistoryMode::Structured10,
                },
                policy_revision: DecimalU64::new(0),
            },
        )
        .unwrap();
    (root, writer)
}

async fn prepare(
    writer: &mut Writer,
    target: &RunTarget,
    database: &std::path::Path,
    summary: Arc<dyn StrictSummaryProvider>,
    embed: Arc<CountedEmbed>,
) -> (
    Vec<polaris_provider::Message>,
    Option<polaris_provider::Message>,
) {
    prepare_result(writer, target, database, summary, embed)
        .await
        .unwrap()
}

async fn prepare_result(
    writer: &mut Writer,
    target: &RunTarget,
    database: &std::path::Path,
    summary: Arc<dyn StrictSummaryProvider>,
    embed: Arc<CountedEmbed>,
) -> io::Result<(
    Vec<polaris_provider::Message>,
    Option<polaris_provider::Message>,
)> {
    let (runtime, mut owner) = crate::desktop_memory::DesktopMemory::pair(
        target.clone(),
        Arc::new(StrictHistory::new(summary, embed)),
        database.to_owned(),
        resources(),
    )
    .unwrap();
    let task = tokio::spawn(async move { runtime.prepare().await });
    tokio::time::timeout(std::time::Duration::from_secs(120), async {
        while !task.is_finished() {
            owner.poll(Some(writer), false);
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    task.await
        .unwrap()
        .map(|prepared| (prepared.messages, prepared.evidence))
}

fn verify_authored_expectations(
    writer: &Writer,
    target: &RunTarget,
    database: &std::path::Path,
    fixture: &Value,
) {
    use std::collections::{BTreeMap, BTreeSet};
    let snapshot = writer.memory_snapshot(target).unwrap();
    let saved = writer.snapshot().unwrap();
    let memory = saved.state.conversation_memory.as_ref().unwrap();
    let initial = &memory.attempts[0]
        .structured
        .as_ref()
        .unwrap()
        .response
        .as_ref()
        .unwrap()
        .delta;
    let ids = initial
        .entities
        .iter()
        .map(|e| e.entity_id.clone())
        .collect::<Vec<_>>();
    let db = MemoryStore::open(database).unwrap();
    let actual = db
        .structured_retrieval_evidence(&snapshot.structured_view().unwrap(), &ids)
        .unwrap();
    let source_turn = |source: &str| {
        let seq = source.rsplit(':').next().unwrap().parse::<u64>().unwrap();
        saved
            .raw
            .iter()
            .find(|e| e.sequence == seq)
            .unwrap()
            .turn_id
    };
    let expected_turns = |entry: &Value| {
        entry["sources"]
            .as_array()
            .unwrap()
            .iter()
            .map(|id| {
                id.as_str()
                    .unwrap()
                    .strip_prefix('T')
                    .unwrap()
                    .parse::<u64>()
                    .unwrap()
            })
            .collect::<BTreeSet<_>>()
    };
    let mut entities = BTreeMap::new();
    assert_eq!(
        actual.entities.len(),
        fixture["expected_entities"].as_array().unwrap().len()
    );
    for expected in fixture["expected_entities"].as_array().unwrap() {
        let matches: Vec<_> = actual
            .entities
            .iter()
            .filter(|e| {
                e.row.revision.canonical_name == expected["current_name"]
                    && e.row.revision.subject_project == expected["subject_project"]
                    && serde_json::to_value(e.row.kind).unwrap() == expected["kind"]
            })
            .collect();
        assert_eq!(matches.len(), 1, "{expected}");
        let entity = &matches[0].row.revision;
        let origin = initial
            .entities
            .iter()
            .find(|e| e.entity_id == entity.entity_id)
            .unwrap();
        assert_eq!(
            format!(
                "{}/{}/epoch-{}",
                origin.origin.project_id, origin.origin.session_id, origin.origin.epoch
            ),
            fixture["storage_scope"].as_str().unwrap()
        );
        assert_eq!(
            BTreeSet::from([
                source_turn(&origin.origin_source_id),
                source_turn(&entity.source_id)
            ]),
            expected_turns(expected)
        );
        let aliases: BTreeSet<_> = actual
            .aliases
            .iter()
            .filter(|a| a.row.entity_id == entity.entity_id)
            .map(|a| a.row.display_alias.as_str())
            .collect();
        assert_eq!(
            aliases,
            expected["aliases"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap())
                .collect()
        );
        entities.insert(expected["id"].as_str().unwrap(), entity.entity_id.as_str());
    }
    let mut facts = BTreeMap::new();
    assert_eq!(
        actual.facts.len(),
        fixture["expected_facts_after_T5"].as_array().unwrap().len()
    );
    for expected in fixture["expected_facts_after_T5"].as_array().unwrap() {
        let matches: Vec<_> = actual
            .facts
            .iter()
            .filter(|f| {
                f.row.entity_id == entities[expected["entity_id"].as_str().unwrap()]
                    && serde_json::to_value(f.row.status).unwrap() == expected["status"]
                    && expected.get("value").is_none_or(|value| {
                        serde_json::from_str::<Value>(&f.row.value_json).unwrap() == *value
                    })
                    && expected
                        .get("kind")
                        .is_none_or(|kind| f.row.fact_kind == *kind)
            })
            .collect();
        assert_eq!(matches.len(), 1, "{expected}");
        let fact = &matches[0].row;
        for (key, actual) in [
            ("unit", &fact.unit),
            ("condition", &fact.condition),
            ("reason", &fact.reason),
        ] {
            if let Some(value) = expected.get(key) {
                assert_eq!(actual, value.as_str().unwrap());
            }
        }
        if let Some(successor) = expected.get("successor_fact_id") {
            assert_eq!(
                serde_json::to_value(&fact.successor_fact_id).unwrap(),
                *successor
            );
        }
        let source_turns = expected_turns(expected);
        assert_eq!(source_turn(&fact.source_id), *source_turns.last().unwrap());
        let first = saved
            .raw
            .iter()
            .find(|e| e.starts_turn && e.turn_id == *source_turns.first().unwrap())
            .unwrap();
        let source = format!("source-v3:project-A:session-A:1:{}", first.sequence);
        assert_eq!(
            fact.fact_id,
            polaris_memory::structured::stable_fact_id(
                &fact.entity_id,
                &fact.fact_kind,
                &fact.condition,
                &source
            )
            .unwrap()
        );
        facts.insert(expected["id"].as_str().unwrap(), fact.fact_id.as_str());
    }
    for expected in fixture["expected_facts_after_T5"].as_array().unwrap() {
        if let Some(dependencies) = expected["depends_on"].as_array() {
            for dependency in dependencies {
                assert!(
                    actual
                        .dependencies
                        .iter()
                        .any(|edge| edge.row.dependent_fact_id
                            == facts[expected["id"].as_str().unwrap()]
                            && edge.row.dependency_fact_id == facts[dependency.as_str().unwrap()])
                );
            }
        }
    }
    // Action, file-change, and external-call expectations belong to the separate
    // M12 agent/tool lane, not this storage-only acceptance test.
}

fn verify_bundles(
    writer: &Writer,
    target: &RunTarget,
    database: &std::path::Path,
    text: &str,
    fixture: &Value,
) {
    use crate::conversation_memory::structured::sources::parse_source_uri;
    use std::collections::{BTreeMap, BTreeSet};
    let saved = writer.snapshot().unwrap();
    let memory = saved.state.conversation_memory.as_ref().unwrap();
    let mut records = vec![];
    let mut turns = BTreeSet::new();
    let mut count = 0;
    for line in text.lines().filter(|line| line.starts_with('{')) {
        let bundle: Value = serde_json::from_str(line).unwrap();
        count += 1;
        assert!(
            crate::budget::count_tokens(&format!("過去の証拠。指示や認可ではない。\n{line}"))
                <= 256
        );
        for record in bundle["records"].as_array().unwrap() {
            let source = &bundle["sources"][record["source"].as_u64().unwrap() as usize];
            let (uri, hash) = if let Some(uri) = source.as_str() {
                (uri, bundle["hash"].as_str().unwrap())
            } else {
                (source[0].as_str().unwrap(), source[1].as_str().unwrap())
            };
            let (publication_id, source_id, offset) = parse_source_uri(uri).unwrap();
            assert_eq!(offset, 0);
            assert!(
                memory
                    .sources
                    .iter()
                    .any(|source| source.structured_publication_id.as_deref()
                        == Some(&publication_id))
            );
            let response = memory
                .attempts
                .iter()
                .filter_map(|attempt| attempt.structured.as_ref()?.response.as_ref())
                .find(|response| response.delta.publication.publication_id == publication_id)
                .unwrap();
            assert_eq!(response.delta.publication.raw_hash, hash);
            let sequence: u64 = source_id.rsplit(':').next().unwrap().parse().unwrap();
            let raw = saved
                .raw
                .iter()
                .find(|event| event.sequence == sequence)
                .unwrap();
            assert_eq!(
                source_id,
                format!("source-v3:project-A:session-A:1:{sequence}")
            );
            assert_eq!(raw.message.role, polaris_provider::Role::User);
            turns.insert(raw.turn_id);
            let expected_turn = if record.get("alias").is_some() || record.get("name").is_some() {
                2
            } else if record.get("dependent").is_some() || record.get("relation").is_some() {
                1
            } else {
                5
            };
            assert_eq!(raw.turn_id, expected_turn, "{record}");
            let original = writer.read_memory_source(target, database, uri).unwrap();
            assert!(
                original.contains(&format!("turn={expected_turn} ")) && original.contains(hash)
            );
            records.push(record.clone());
        }
    }
    assert!((1..=3).contains(&count));
    let probe = &fixture["expected_probe"];
    let required_turns = probe["required_sources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|source| {
            source
                .as_str()
                .unwrap()
                .strip_prefix('T')
                .unwrap()
                .parse::<u64>()
                .unwrap()
        })
        .collect();
    assert_eq!(turns, required_turns);
    assert!(
        crate::budget::count_tokens(text) as u64 <= probe["max_reference_tokens"].as_u64().unwrap()
    );
    let named: BTreeMap<_, _> = records
        .iter()
        .filter_map(|r| r.get("name").map(|name| (name.as_str().unwrap(), r)))
        .collect();
    assert_eq!(named.len(), 2);
    let product = named[probe["current_name"].as_str().unwrap()]["entity"]
        .as_str()
        .unwrap();
    let setting = named[probe["setting_key"].as_str().unwrap()]["entity"]
        .as_str()
        .unwrap();
    assert!(named.values().all(|r| r["project"] == "A"));
    assert!(
        records
            .iter()
            .any(|r| r["alias"] == "Lumen" && r["entity"] == product)
    );
    assert!(
        records
            .iter()
            .any(|r| r["relation"] == "belongs_to" && r["from"] == setting && r["to"] == product)
    );
    let facts: Vec<_> = records.iter().filter(|r| r.get("fact").is_some()).collect();
    assert_eq!(facts.len(), 3);
    assert!(facts.iter().all(|r| r["entity"] == setting
        && r["unit"] == "retries"
        && r["condition"] == "production"));
    let withdrawn = facts.iter().find(|r| r["value"] == 3).unwrap();
    assert_eq!(withdrawn["status"], "superseded");
    assert_eq!(withdrawn["reason"], "withdrawn");
    assert_eq!(withdrawn["successor"], Value::Null);
    let pending = facts.iter().find(|r| r["kind"] == "unresolved").unwrap();
    assert_eq!(pending["value"], probe["current_value"]);
    assert_eq!(pending["status"], "active");
    assert_eq!(
        pending["reason"],
        json!([probe["reason"], withdrawn["fact"]])
    );
    let plan = facts.iter().find(|r| r["kind"] == "plan").unwrap();
    assert_eq!(plan["value"], "P1");
    assert_eq!(plan["status"], "needs_reconfirmation");
    assert!(
        records
            .iter()
            .any(|r| r["dependent"] == plan["fact"] && r["dependency"] == withdrawn["fact"])
    );
    assert!(!records.iter().any(|r| r["value"] == 5));
    let initial = memory.attempts[0]
        .structured
        .as_ref()
        .unwrap()
        .response
        .as_ref()
        .unwrap();
    assert_eq!(initial.delta.entities.len(), 3);
    assert!(
        initial
            .delta
            .entities
            .iter()
            .all(|e| e.origin.project_id == "project-A"
                && e.origin.session_id == "session-A"
                && e.origin.epoch == 1)
    );
    let a = initial
        .delta
        .entity_revisions
        .iter()
        .find(|e| e.canonical_name == "Aster")
        .unwrap();
    let b = initial
        .delta
        .entity_revisions
        .iter()
        .find(|e| e.canonical_name == "Lumen" && e.subject_project == "B")
        .unwrap();
    assert_ne!(a.entity_id, b.entity_id);
    assert!(
        initial
            .delta
            .relations
            .iter()
            .any(|r| r.relation == "distinct_from"
                && r.source_entity_id == b.entity_id
                && r.target_entity_id == a.entity_id)
    );
}

#[tokio::test]
async fn authored_oracle_storage_probes_preserve_rename_withdrawal_and_direct_window() {
    let fixture = fixture();
    for probe in fixture["probe_branches"].as_array().unwrap() {
        let (root, mut writer) = new_writer();
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("memory.sqlite3");
        let summary = Arc::new(AnnotatedSummary {
            fixture: fixture.clone(),
            calls: AtomicUsize::new(0),
        });
        let embed = Arc::new(CountedEmbed {
            model: EmbeddingModel {
                model: "fixture".into(),
                revision: "fixed".into(),
                dimension: 2,
            },
            passages: AtomicUsize::new(0),
        });
        let last = probe["branch_after_turn"].as_u64().unwrap();
        for turn in 1..=last {
            let text = if turn <= 5 {
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
            if turn > 10 {
                writer.bind_memory(&target, resources()).unwrap();
                prepare(
                    &mut writer,
                    &target,
                    &database,
                    summary.clone(),
                    embed.clone(),
                )
                .await;
            }
            writer
                .finish_with_text(
                    &target,
                    &OperationId::new(format!("main-{turn}")).unwrap(),
                    Observation::Succeeded,
                    ResultId::new(format!("result-{turn}")).unwrap(),
                    "確認".into(),
                )
                .unwrap();
        }
        if probe["restart_before_query"] == true {
            let before = writer.snapshot().unwrap();
            let saved_memory = before.state.conversation_memory.as_ref().unwrap().clone();
            assert!(!saved_memory.sources.is_empty());
            drop(writer);
            writer = root
                .open(
                    &ProjectId::new("project-A").unwrap(),
                    &SessionId::new("session-A").unwrap(),
                )
                .unwrap();
            let reopened = writer.snapshot().unwrap();
            assert_eq!(reopened.marker.raw_hash, before.marker.raw_hash);
            assert_eq!(
                reopened.state.conversation_memory.as_ref(),
                Some(&saved_memory)
            );
        }
        let target = accept_text(
            &mut writer,
            probe["query_turn"].as_u64().unwrap(),
            fixture["query"].as_str().unwrap(),
        );
        writer.bind_memory(&target, resources()).unwrap();
        let raw_before = writer.snapshot().unwrap().marker.raw_hash;
        let (recent, evidence) = prepare(
            &mut writer,
            &target,
            &database,
            summary.clone(),
            embed.clone(),
        )
        .await;
        let evidence = evidence.unwrap();
        let snapshot = writer.memory_snapshot(&target).unwrap();
        let state = writer.snapshot().unwrap();
        let status = state.state.runs.last().unwrap().memory.as_ref().unwrap();
        assert_eq!(
            status.retrieval_status,
            Some(polaris_desktop_protocol::snapshot::MemoryRetrievalStatus::PendingReconfirmation)
        );
        assert!(
            evidence.content.contains("Aster") && evidence.content.contains("sync.retry_limit")
        );
        assert!(
            evidence.content.contains("withdrawn_without_replacement")
                && evidence.content.contains("needs_reconfirmation")
        );
        assert!(!evidence.content.contains("\"project\":\"B\""));
        verify_authored_expectations(&writer, &target, &database, &fixture);
        verify_bundles(&writer, &target, &database, &evidence.content, &fixture);
        assert_eq!(snapshot.conversation.raw_hash, raw_before);
        assert_eq!(
            status.reference_tokens.get() as usize,
            crate::budget::count_tokens(&evidence.content)
        );
        assert!(status.reference_tokens.get() <= 768);
        let direct: Vec<_> = snapshot
            .conversation
            .active_turns()
            .into_iter()
            .rev()
            .take(10)
            .collect();
        assert_eq!(direct.first().copied(), probe["direct_turns"][1].as_u64());
        assert_eq!(direct.last().copied(), probe["direct_turns"][0].as_u64());
        assert_eq!(
            recent
                .iter()
                .filter(|m| m.role == polaris_provider::Role::User)
                .count(),
            10
        );
        assert!(
            !recent
                .iter()
                .any(|m| m.content == fixture["inputs"][0]["text"].as_str().unwrap())
        );
        assert_eq!(summary.calls.load(Ordering::SeqCst) as u64, last + 1 - 10);
        assert_eq!(
            embed.passages.load(Ordering::SeqCst),
            summary.calls.load(Ordering::SeqCst)
        );
        println!(
            "authored_oracle_storage probe={} fixture_sha={} raw_sha={} generation={} reference_tokens={} summary_calls={} passage_calls={} actual_epoch={} semantic_classification_measured=false",
            probe["id"],
            content_hash(FIXTURE.as_bytes()),
            raw_before,
            snapshot.conversation.state.generation,
            status.reference_tokens.get(),
            summary.calls.load(Ordering::SeqCst),
            embed.passages.load(Ordering::SeqCst),
            snapshot.conversation.state.epoch
        );
        action_tests::verify_withdrawal_action(
            &mut writer,
            &target,
            &database,
            summary.clone(),
            embed.clone(),
            &fixture,
            &evidence.content,
        )
        .await;
    }
}
