//! Regression checks for source authority, identity transitions, and bounded summary hints.

use super::*;
use crate::conversation_state::ConversationStateV2;
use polaris_desktop_protocol::snapshot::HistoryMode;
use polaris_memory::{MemoryStore, structured::StructuredPublicationView};
use polaris_provider::Message;
use serde_json::{Value, json};

pub(super) fn fixture() -> (ConversationSnapshot, StructuredIntent, Value) {
    let mut state = ConversationStateV2::new(
        "project".into(),
        "00000000-0000-4000-8000-000000000001".into(),
    )
    .unwrap();
    state.history_mode = HistoryMode::Structured10;
    let texts = [
        "Rigel retry_limit is 3 for production. Deploy depends on retry_limit.",
        "Rename Rigel to Aster, the same product.",
        "Aster retry_limit 5 is only a proposal.",
        "Correct Aster retry_limit to 2 for production.",
        "Withdraw Aster retry_limit for production. Replacement is undecided.",
    ];
    let events = texts
        .iter()
        .enumerate()
        .map(|(i, text)| RawEventV2 {
            schema_version: 2,
            sequence: i as u64 + 1,
            epoch: 0,
            turn_id: i as u64 + 1,
            starts_turn: true,
            message: Message::user(*text),
        })
        .collect();
    let mut snapshot = ConversationSnapshot {
        state,
        raw_hash: String::new(),
        raw_offset: 0,
        events,
    };
    let raw = snapshot_raw(&snapshot).unwrap();
    snapshot.raw_hash = content_hash(&raw);
    snapshot.raw_offset = raw.len() as u64;
    snapshot.state.raw_commit_hash = snapshot.raw_hash.clone();
    snapshot.state.raw_commit_offset = snapshot.raw_offset;
    let intent = structured_intent(&snapshot).unwrap().unwrap();
    let source = |i: usize| json!({"source_id":intent.sources[i].source_id, "evidence_span":{"start":0,"length":texts[i].len()},"evidence_text":texts[i],"ordinal":0});
    let mut entity = source(0);
    entity.as_object_mut().unwrap().extend(
        json!({"speech_act":"assert","kind":"product","name":"Rigel","subject_project":"project"})
            .as_object()
            .unwrap()
            .clone(),
    );
    let mut rename = source(1);
    rename.as_object_mut().unwrap().extend(json!({"speech_act":"correct","kind":"product","name":"Aster","subject_project":"project","renamed_from":"Rigel"}).as_object().unwrap().clone());
    let fact = |i, name, kind, value, relation, speech| {
        let mut row = source(i);
        row.as_object_mut().unwrap().extend(json!({"speech_act":speech,"entity_kind":"product","entity_name":name,"subject_project":"project","fact_kind":kind,"value":value,"unit":"retries","condition":"production","relation":relation}).as_object().unwrap().clone());
        row
    };
    let original = fact(0, "Rigel", "retry_limit", json!(3), "assert", "assert");
    let mut plan = fact(0, "Rigel", "plan", json!("deploy"), "depends_on", "assert");
    plan["ordinal"] = json!(1);
    plan["depends_on"] = json!([{"entity_kind":"product","entity_name":"Rigel","subject_project":"project","fact_kind":"retry_limit","condition":"production"}]);
    let proposed = fact(2, "Aster", "retry_limit", json!(5), "propose", "propose");
    let corrected = fact(3, "Aster", "retry_limit", json!(2), "replace", "correct");
    let withdrawn = fact(
        4,
        "Aster",
        "retry_limit",
        Value::Null,
        "withdraw",
        "withdraw",
    );
    let response = json!({
        "summary":{"facts":["Rigel retry_limit is 3"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[1]},
        "processed_source_ids":intent.sources.iter().map(|s| &s.source_id).collect::<Vec<_>>(),
        // Reverse candidate order to prove durable source ordering.
        "entity_candidates":[rename, entity], "fact_deltas":[withdrawn, corrected, proposed, plan, original]
    });
    (snapshot, intent, response)
}

#[test]
fn structured_same_identity_reaffirmation_omits_canonical_alias_but_keeps_case_variant() {
    let (mut snapshot, _, _) = fixture();
    snapshot.events.truncate(2);
    for event in &mut snapshot.events {
        event.message =
            Message::user("The configuration key retry_limit is also named Retry_Limit.");
    }
    let raw = snapshot_raw(&snapshot).unwrap();
    snapshot.raw_offset = raw.len() as u64;
    snapshot.raw_hash = content_hash(&raw);
    snapshot.state.raw_commit_offset = snapshot.raw_offset;
    snapshot.state.raw_commit_hash = snapshot.raw_hash.clone();
    let intent = structured_intent(&snapshot).unwrap().unwrap();
    let mut entities = vec![];
    for (index, source) in intent.sources.iter().enumerate() {
        let mut candidate = json!({
            "source_id":source.source_id,"evidence_span":{"start":0,"length":source.content.len()},
            "evidence_text":source.content,"speech_act":"assert","ordinal":0,
            "kind":"configuration_key","name":"retry_limit","subject_project":"project"
        });
        if index == 1 {
            candidate["same_as"] = json!("retry_limit");
            candidate["aliases"] = json!(["retry_limit", "Retry_Limit"]);
        }
        entities.push(candidate);
    }
    let response = json!({
        "summary":{"facts":["The key is unchanged"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[1]},
        "processed_source_ids":intent.sources.iter().map(|s| &s.source_id).collect::<Vec<_>>(),
        "entity_candidates":entities,"fact_deltas":[]
    });
    let result = validate_response(&snapshot, 1, &intent, &response.to_string()).unwrap();
    assert_eq!(result.delta.entities.len(), 1);
    assert_eq!(result.delta.entity_revisions.len(), 1);
    assert_eq!(
        result.delta.entity_revisions[0].source_id,
        intent.sources[1].source_id
    );
    assert_eq!(result.delta.aliases.len(), 1);
    assert_eq!(result.delta.aliases[0].display_alias, "Retry_Limit");
    assert_eq!(result.delta.aliases[0].match_alias, "Retry_Limit");
}

#[test]
fn structured_summary_only_intent_preserves_targets_but_rejects_new_rows() {
    let (mut snapshot, original_intent, original_response) = fixture();
    let original_snapshot = snapshot.clone();
    let initial = validate_response(
        &snapshot,
        1,
        &original_intent,
        &original_response.to_string(),
    )
    .unwrap();
    let resolve =
        |_: EntityKind, _: &str, _: &str| Ok(Some(initial.delta.entities[0].entity_id.clone()));
    snapshot.state.structured_coverage = Some(original_intent.coverage.clone());
    snapshot.state.generation = 1;
    snapshot.state.visible_structured_publication_ids = vec!["published".into()];
    let intent = structured_summary_intent(&snapshot).unwrap().unwrap();
    assert!(intent.sources.is_empty());
    let input = summary_input(&snapshot, 2, &intent).unwrap();
    assert!(input.extraction_source_ids.is_empty());
    assert_eq!(
        input.summary_target_source_ids,
        vec![original_intent.sources[1].source_id.clone()]
    );
    assert_eq!(input.sources, vec![original_intent.sources[1].clone()]);
    let response = json!({
        "summary":{"facts":["Rename Rigel to Aster"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[2]},
        "processed_source_ids":[],"entity_candidates":[],"fact_deltas":[],"entity_relations":[]
    });
    let accepted = validate_response(&snapshot, 2, &intent, &response.to_string()).unwrap();
    validate_saved_response(
        &snapshot,
        2,
        &intent,
        &accepted.summary,
        &accepted.delta,
        &accepted.coverage,
    )
    .unwrap();
    for (key, candidate) in [
        (
            "entity_candidates",
            original_response["entity_candidates"][1].clone(),
        ),
        (
            "fact_deltas",
            original_response["fact_deltas"]
                .as_array()
                .unwrap()
                .last()
                .unwrap()
                .clone(),
        ),
    ] {
        let mut candidate_response = response.clone();
        candidate_response[key] = json!([candidate]);
        candidate_response["processed_source_ids"] =
            original_response["processed_source_ids"].clone();
        validate_response_with_entities(
            &original_snapshot,
            2,
            &original_intent,
            &candidate_response.to_string(),
            &resolve,
        )
        .unwrap();
        candidate_response["processed_source_ids"] = json!([]);
        let error = validate_response_with_entities(
            &snapshot,
            2,
            &intent,
            &candidate_response.to_string(),
            &resolve,
        )
        .unwrap_err();
        assert_eq!(
            error.to_string(),
            "The structured candidate source is outside the extraction range",
            "{key}"
        );
    }
}

#[test]
fn structured_summary_only_intent_requires_complete_published_committed_coverage() {
    let (mut snapshot, original, _) = fixture();
    // Without coverage, all original sources remain eligible for extraction.
    assert_eq!(
        structured_summary_intent(&snapshot).unwrap(),
        Some(original.clone())
    );
    snapshot.state.structured_coverage = Some(original.coverage.clone());
    assert!(structured_summary_intent(&snapshot).unwrap().is_none());
    snapshot.state.visible_structured_publication_ids = vec!["published".into()];
    assert!(
        structured_summary_intent(&snapshot)
            .unwrap()
            .unwrap()
            .sources
            .is_empty()
    );
    let mut invalid = snapshot.clone();
    invalid.state.raw_commit_offset -= 1;
    assert!(structured_summary_intent(&invalid).unwrap().is_none());
    let mut invalid = snapshot.clone();
    invalid
        .state
        .structured_coverage
        .as_mut()
        .unwrap()
        .raw_offset -= 1;
    assert!(structured_summary_intent(&invalid).unwrap().is_none());
    let mut invalid = snapshot.clone();
    invalid.state.raw_commit_hash = content_hash(b"wrong committed prefix");
    assert!(structured_summary_intent(&invalid).is_err());
    let mut invalid = snapshot.clone();
    invalid
        .state
        .structured_coverage
        .as_mut()
        .unwrap()
        .prefix_hash = content_hash(b"wrong coverage");
    assert!(structured_summary_intent(&invalid).is_err());
    // New committed sources always take precedence over the summary-only path.
    snapshot.events.push(RawEventV2 {
        schema_version: 2,
        sequence: 6,
        epoch: 0,
        turn_id: 6,
        starts_turn: true,
        message: Message::user("A new source"),
    });
    let raw = snapshot_raw(&snapshot).unwrap();
    snapshot.raw_offset = raw.len() as u64;
    snapshot.raw_hash = content_hash(&raw);
    snapshot.state.raw_commit_offset = snapshot.raw_offset;
    snapshot.state.raw_commit_hash = snapshot.raw_hash.clone();
    let next = structured_summary_intent(&snapshot).unwrap().unwrap();
    assert_eq!(next.sources.len(), 1);
    assert_eq!(next.sources[0].sequence, 6);
    // An oversized uncovered source cannot be replaced by an empty extraction.
    snapshot.events.last_mut().unwrap().message.content = "uncovered ".repeat(40_000);
    let raw = snapshot_raw(&snapshot).unwrap();
    snapshot.raw_offset = raw.len() as u64;
    snapshot.raw_hash = content_hash(&raw);
    snapshot.state.raw_commit_offset = snapshot.raw_offset;
    snapshot.state.raw_commit_hash = snapshot.raw_hash.clone();
    assert!(structured_summary_intent(&snapshot).unwrap().is_none());
}

#[test]
fn structured_state_retrieval_orders_withdrawal_before_values_and_proposals() {
    let (snapshot, intent, response) = fixture();
    let result = validate_response(&snapshot, 1, &intent, &response.to_string()).unwrap();
    let mut database = MemoryStore::open(":memory:").unwrap();
    database.enable_structured_memory().unwrap();
    let mut view = StructuredPublicationView {
        scope: result.delta.publication.scope.clone(),
        visible_publication_ids: vec![],
        ancestors: vec![],
    };
    database
        .insert_structured_delta(&result.delta, &view)
        .unwrap();
    view.visible_publication_ids
        .push(result.delta.publication.publication_id.clone());
    let retrieved =
        retrieval::retrieve_state_evidence(&database, &view, "旧Rigelの本番設定").unwrap();
    let evidence = retrieved.evidence.unwrap();
    assert_eq!(evidence.entities[0].row.revision.canonical_name, "Aster");
    assert_eq!(evidence.facts.len(), 5);
    let states: Vec<_> = evidence
        .facts
        .iter()
        .map(|f| {
            (
                f.row.fact_kind.as_str(),
                f.row.status,
                f.row.value_json.as_str(),
            )
        })
        .collect();
    assert_eq!(
        states,
        vec![
            ("unresolved", FactStatus::Active, "null"),
            ("plan", FactStatus::NeedsReconfirmation, "\"deploy\""),
            ("retry_limit", FactStatus::Superseded, "2"),
            ("retry_limit", FactStatus::Superseded, "3"),
            ("retry_limit", FactStatus::Proposed, "5"),
        ]
    );
    assert!(!evidence.dependencies.is_empty());
    for edge in &evidence.dependencies {
        assert!(
            evidence
                .facts
                .iter()
                .any(|f| f.row.fact_id == edge.row.dependent_fact_id)
        );
        assert!(
            evidence
                .facts
                .iter()
                .any(|f| f.row.fact_id == edge.row.dependency_fact_id)
        );
    }
    assert_eq!(
        serde_json::to_value(&evidence).unwrap(),
        serde_json::to_value(
            retrieval::retrieve_state_evidence(&database, &view, "旧Rigelの本番設定")
                .unwrap()
                .evidence
                .unwrap()
        )
        .unwrap(),
    );
}

#[tokio::test]
async fn structured_hint_overflow_stops_before_the_summary_provider() {
    use crate::conversation_memory::strict_history::{
        EmbeddingModel, StrictEmbedder, StrictFuture, StrictHistory, StrictSummaryProvider,
        SummaryRequest,
    };
    use std::sync::{Arc, Mutex};

    struct Probe(Mutex<Vec<usize>>);
    impl StrictSummaryProvider for Probe {
        fn summarize<'a>(&'a self, request: SummaryRequest) -> StrictFuture<'a, String> {
            self.0.lock().unwrap().push(
                request
                    .structured_sources
                    .unwrap()
                    .related_entities
                    .iter()
                    .map(|e| e.matching_aliases.len())
                    .sum(),
            );
            Box::pin(async { Err(io::Error::other("provider reached")) })
        }
    }
    struct NoEmbedding(EmbeddingModel);
    impl StrictEmbedder for NoEmbedding {
        fn metadata(&self) -> &EmbeddingModel {
            &self.0
        }
        fn embed_passage<'a>(&'a self, _: &'a str) -> StrictFuture<'a, Vec<f32>> {
            panic!("summary preparation must not embed");
        }
        fn embed_query<'a>(&'a self, _: &'a str) -> StrictFuture<'a, Vec<Vec<f32>>> {
            panic!("summary preparation must not embed");
        }
    }
    let (mut snapshot, intent, response) = fixture();
    let mut delta = validate_response(&snapshot, 1, &intent, &response.to_string())
        .unwrap()
        .delta;
    let entity = delta.entities[0].entity_id.clone();
    delta.aliases = (0..33)
        .map(|n| StructuredAlias {
            entity_id: entity.clone(),
            display_alias: format!("alias{n}"),
            match_alias: format!("alias{n}"),
            relation: "alias".into(),
            source_id: intent.sources[0].source_id.clone(),
        })
        .collect();
    delta.publication.delta_hash = polaris_memory::structured::delta_hash(&delta).unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("memory.sqlite");
    let mut db = MemoryStore::open(&path).unwrap();
    db.enable_structured_memory().unwrap();
    let mut view = StructuredPublicationView {
        scope: delta.publication.scope.clone(),
        visible_publication_ids: vec![],
        ancestors: vec![],
    };
    db.insert_structured_delta(&delta, &view).unwrap();
    view.visible_publication_ids
        .push(delta.publication.publication_id.clone());
    drop(db);
    let probe = Arc::new(Probe(Mutex::new(vec![])));
    let history = StrictHistory::new(
        probe.clone(),
        Arc::new(NoEmbedding(EmbeddingModel {
            model: "fixture".into(),
            revision: "v1".into(),
            dimension: 384,
        })),
    );
    for count in [33, 32] {
        snapshot.events.truncate(1);
        snapshot.events[0].message = Message::user(
            (0..count)
                .map(|n| format!("alias{n}"))
                .collect::<Vec<_>>()
                .join(" "),
        );
        let raw = snapshot_raw(&snapshot).unwrap();
        snapshot.raw_hash = content_hash(&raw);
        snapshot.raw_offset = raw.len() as u64;
        snapshot.state.raw_commit_hash = snapshot.raw_hash.clone();
        snapshot.state.raw_commit_offset = snapshot.raw_offset;
        let input = structured_intent(&snapshot).unwrap().unwrap();
        let error = history
            .summarize_structured_turn(&snapshot, 1, &input, &path, &view)
            .await
            .unwrap_err();
        if count == 33 {
            assert!(
                error.to_string().contains("structured hint alias limit"),
                "{error}"
            );
            assert!(
                probe.0.lock().unwrap().is_empty(),
                "no partial hints reach provider"
            );
        } else {
            assert_eq!(error.to_string(), "provider reached");
            assert_eq!(*probe.0.lock().unwrap(), vec![32]);
        }
    }
    let mut db = MemoryStore::open(&path).unwrap();
    for generation in 2..=3 {
        let mut next = delta.clone();
        next.publication.scope.generation = generation;
        next.publication.publication_id = format!("hint-{generation}");
        next.entities.clear();
        next.entity_revisions.clear();
        next.facts.clear();
        next.dependencies.clear();
        next.aliases = (0..1024)
            .map(|n| StructuredAlias {
                entity_id: entity.clone(),
                display_alias: format!("unused{generation}_{n}"),
                match_alias: format!("unused{generation}_{n}"),
                relation: "alias".into(),
                source_id: intent.sources[0].source_id.clone(),
            })
            .collect();
        next.publication.delta_hash = polaris_memory::structured::delta_hash(&next).unwrap();
        db.insert_structured_delta(&next, &view).unwrap();
        view.scope.generation = generation;
        view.visible_publication_ids
            .push(next.publication.publication_id);
    }
    drop(db);
    probe.0.lock().unwrap().clear();
    for text in ["x".repeat(48 * 1024), "unknown".into()] {
        snapshot.events[0].message = Message::user(&text);
        let raw = snapshot_raw(&snapshot).unwrap();
        snapshot.raw_hash = content_hash(&raw);
        snapshot.raw_offset = raw.len() as u64;
        snapshot.state.raw_commit_hash = snapshot.raw_hash.clone();
        snapshot.state.raw_commit_offset = snapshot.raw_offset;
        let input = structured_intent(&snapshot).unwrap().unwrap();
        let error = history
            .summarize_structured_turn(&snapshot, 1, &input, &path, &view)
            .await
            .unwrap_err();
        if text.len() > 1024 {
            assert!(
                error
                    .to_string()
                    .contains("structured hint comparison limit"),
                "{error}"
            );
            assert!(probe.0.lock().unwrap().is_empty());
        } else {
            assert_eq!(error.to_string(), "provider reached");
            assert_eq!(*probe.0.lock().unwrap(), vec![0]);
        }
    }
}

#[test]
fn structured_cross_kind_relationships_require_explicit_resolved_user_evidence() {
    let (mut snapshot, _, _) = fixture();
    let text = "The configuration key retry_limit belongs to product Rigel.";
    snapshot.events.truncate(1);
    snapshot.events[0].message = Message::user(text);
    let refresh = |snapshot: &mut ConversationSnapshot| {
        let raw = snapshot_raw(snapshot).unwrap();
        snapshot.raw_hash = content_hash(&raw);
        snapshot.raw_offset = raw.len() as u64;
        snapshot.state.raw_commit_hash = snapshot.raw_hash.clone();
        snapshot.state.raw_commit_offset = snapshot.raw_offset;
        structured_intent(snapshot).unwrap().unwrap()
    };
    let intent = refresh(&mut snapshot);
    let source = json!({"source_id": intent.sources[0].source_id, "evidence_span":{"start":0,"length":text.len()}, "evidence_text":text, "speech_act":"assert"});
    let reference = |kind, name| json!({"kind":kind,"name":name,"subject_project":"project"});
    let mut candidates = vec![];
    for (ordinal, (kind, name)) in [("product", "Rigel"), ("configuration_key", "retry_limit")]
        .into_iter()
        .enumerate()
    {
        let mut entry = source.clone();
        entry
            .as_object_mut()
            .unwrap()
            .extend(reference(kind, name).as_object().unwrap().clone());
        entry["ordinal"] = json!(ordinal);
        candidates.push(entry);
    }
    candidates.reverse();
    let mut relation = source;
    relation["from"] = reference("configuration_key", "retry_limit");
    relation["to"] = reference("product", "Rigel");
    relation["relation"] = json!("belongs_to");
    let response = json!({
        "summary":{"facts":["retry_limit belongs to Rigel"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[1]},
        "processed_source_ids":[intent.sources[0].source_id],
        "entity_candidates":candidates,"fact_deltas":[],"entity_relations":[relation]
    });
    let result = validate_response(&snapshot, 1, &intent, &response.to_string()).unwrap();
    assert_eq!(result.delta.relations.len(), 1);
    let row = &result.delta.relations[0];
    let from = result
        .delta
        .entities
        .iter()
        .find(|e| e.kind == EntityKind::ConfigurationKey)
        .unwrap();
    let to = result
        .delta
        .entities
        .iter()
        .find(|e| e.kind == EntityKind::Product)
        .unwrap();
    assert_eq!(
        (
            &row.source_entity_id,
            &row.target_entity_id,
            row.relation.as_str()
        ),
        (&from.entity_id, &to.entity_id, "belongs_to")
    );
    let mut db = MemoryStore::open(":memory:").unwrap();
    db.enable_structured_memory().unwrap();
    db.insert_structured_delta(
        &result.delta,
        &StructuredPublicationView {
            scope: result.delta.publication.scope.clone(),
            visible_publication_ids: vec![],
            ancestors: vec![],
        },
    )
    .unwrap();
    db.verify_structured_delta(&result.delta).unwrap();
    let view = StructuredPublicationView {
        scope: result.delta.publication.scope.clone(),
        visible_publication_ids: vec![result.delta.publication.publication_id.clone()],
        ancestors: vec![],
    };
    let resolver = PublishedResolver {
        memory: &db,
        view: &view,
    };
    let mut relation_only = response.clone();
    relation_only["entity_candidates"] = json!([]);
    validate_response_with_resolver(&snapshot, 1, &intent, &relation_only.to_string(), &resolver)
        .unwrap();
    let mut covered = snapshot.clone();
    covered.state.structured_coverage = Some(intent.coverage.clone());
    covered.state.visible_structured_publication_ids = view.visible_publication_ids.clone();
    covered.state.generation = 1;
    let empty = structured_summary_intent(&covered).unwrap().unwrap();
    relation_only["processed_source_ids"] = json!([]);
    let error =
        validate_response_with_resolver(&covered, 1, &empty, &relation_only.to_string(), &resolver)
            .unwrap_err();
    assert_eq!(
        error.to_string(),
        "The structured candidate source is outside the extraction range"
    );
    for relation in ["owns", "belongs_to", "distinct_from", "execute"] {
        let mut delta = result.delta.clone();
        delta.relations[0].relation = relation.into();
        delta.publication.delta_hash = polaris_memory::structured::delta_hash(&delta).unwrap();
        let mut isolated = MemoryStore::open(":memory:").unwrap();
        isolated.enable_structured_memory().unwrap();
        let view = StructuredPublicationView {
            scope: delta.publication.scope.clone(),
            visible_publication_ids: vec![],
            ancestors: vec![],
        };
        if relation == "execute" {
            assert!(validate_delta(&delta).is_err());
            assert!(
                validate_saved_response(
                    &snapshot,
                    1,
                    &intent,
                    &result.summary,
                    &delta,
                    &result.coverage
                )
                .is_err()
            );
            assert!(isolated.insert_structured_delta(&delta, &view).is_err());
            assert!(
                isolated
                    .structured_publication(&view.scope, &delta.publication.publication_id)
                    .unwrap()
                    .is_none()
            );
        } else {
            validate_delta(&delta).unwrap();
            isolated.insert_structured_delta(&delta, &view).unwrap();
            isolated.verify_structured_delta(&delta).unwrap();
        }
    }
    for speech in ["quote", "hypothetical", "reject", "propose", "uncertain"] {
        let mut bad = response.clone();
        bad["entity_relations"][0]["speech_act"] = json!(speech);
        assert!(
            validate_response(&snapshot, 1, &intent, &bad.to_string()).is_err(),
            "{speech}"
        );
    }
    for (key, value) in [("name", "missing"), ("subject_project", "foreign")] {
        let mut bad = response.clone();
        bad["entity_relations"][0]["to"][key] = json!(value);
        assert!(validate_response(&snapshot, 1, &intent, &bad.to_string()).is_err());
    }
    let mut bad = response.clone();
    bad["entity_relations"][0]["to"]["entity_id"] = json!(to.entity_id);
    assert!(validate_response(&snapshot, 1, &intent, &bad.to_string()).is_err());
    let mut bad = response.clone();
    bad["entity_relations"][0]["to"] = bad["entity_relations"][0]["from"].clone();
    assert!(validate_response(&snapshot, 1, &intent, &bad.to_string()).is_err());
    let mut bad = response.clone();
    bad["entity_relations"]
        .as_array_mut()
        .unwrap()
        .push(response["entity_relations"][0].clone());
    assert!(validate_response(&snapshot, 1, &intent, &bad.to_string()).is_err());
    for role in [Role::Assistant, Role::Tool] {
        snapshot.events[0].message.role = role;
        let intent = refresh(&mut snapshot);
        assert!(validate_response(&snapshot, 1, &intent, &response.to_string()).is_err());
    }
}

#[test]
fn structured_rename_replace_withdraw_preserves_history_proposals_and_dependents() {
    let (snapshot, intent, response) = fixture();
    let result = validate_response(&snapshot, 1, &intent, &response.to_string()).unwrap();
    assert_eq!(result.delta.entities.len(), 1);
    assert_eq!(result.delta.entity_revisions.len(), 1);
    assert_eq!(result.delta.entity_revisions[0].canonical_name, "Aster");
    assert!(
        result
            .delta
            .aliases
            .iter()
            .any(|a| a.display_alias == "Rigel")
    );
    let mut db = MemoryStore::open(":memory:").unwrap();
    db.enable_structured_memory().unwrap();
    let mut view = StructuredPublicationView {
        scope: result.delta.publication.scope.clone(),
        visible_publication_ids: vec![],
        ancestors: vec![],
    };
    db.insert_structured_delta(&result.delta, &view).unwrap();
    db.verify_structured_delta(&result.delta).unwrap();
    view.visible_publication_ids
        .push(result.delta.publication.publication_id.clone());
    let id = db
        .resolve_structured_entity(&view, EntityKind::Product, "Rigel", "project")
        .unwrap()
        .unwrap();
    assert_eq!(
        db.resolve_structured_entity(&view, EntityKind::Product, "Aster", "project")
            .unwrap(),
        Some(id.clone())
    );
    let facts = db
        .structured_fact_candidates(&view, &id, "retry_limit", "production")
        .unwrap();
    let old = facts.iter().find(|f| f.value_json == "3").unwrap();
    let corrected = facts.iter().find(|f| f.value_json == "2").unwrap();
    let proposal = facts.iter().find(|f| f.value_json == "5").unwrap();
    assert_eq!(
        (old.status, old.successor_fact_id.as_deref()),
        (FactStatus::Superseded, Some(corrected.fact_id.as_str()))
    );
    assert_eq!(
        (
            corrected.status,
            corrected.reason.as_str(),
            corrected.successor_fact_id.as_deref()
        ),
        (FactStatus::Superseded, "withdrawn", None)
    );
    assert_eq!(proposal.status, FactStatus::Proposed);
    let unresolved = db
        .structured_fact_candidates(&view, &id, "unresolved", "production")
        .unwrap();
    assert_eq!(unresolved.len(), 1);
    assert_eq!(
        (unresolved[0].status, unresolved[0].value_json.as_str()),
        (FactStatus::Active, "null")
    );
    let plan = db
        .structured_fact_candidates(&view, &id, "plan", "production")
        .unwrap();
    assert_eq!(plan[0].status, FactStatus::NeedsReconfirmation);
    assert!(
        result
            .delta
            .facts
            .iter()
            .any(|f| f.fact_id == old.fact_id && f.status == FactStatus::Active)
    );
}

#[test]
fn structured_fact_transition_rejects_forged_ids_and_invalid_authority() {
    let (snapshot, intent, response) = fixture();
    for (field, value) in [
        ("target_fact_id", json!("forged")),
        ("relation", json!("assert")),
        ("value", json!(99)),
    ] {
        let mut bad = response.clone();
        bad["fact_deltas"][0][field] = value;
        assert!(
            validate_response(&snapshot, 1, &intent, &bad.to_string()).is_err(),
            "{field}"
        );
    }
    for speech in ["quote", "hypothetical", "reject", "uncertain"] {
        let mut quoted = response.clone();
        quoted["fact_deltas"][0]["speech_act"] = json!(speech);
        let result = validate_response(&snapshot, 1, &intent, &quoted.to_string()).unwrap();
        assert!(!result.delta.facts.iter().any(|f| f.reason == "withdrawn"));
        assert!(
            result
                .delta
                .facts
                .iter()
                .any(|f| f.value_json == "2" && f.status == FactStatus::Active)
        );
    }
}

#[test]
fn structured_fact_correction_uses_existing_entity_without_a_rename_candidate() {
    let (snapshot, intent, mut response) = fixture();
    let original = response["fact_deltas"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    response["entity_candidates"] = json!([response["entity_candidates"][1].clone()]);
    response["fact_deltas"] = json!([original]);
    let first = validate_response(&snapshot, 1, &intent, &response.to_string()).unwrap();
    let mut db = MemoryStore::open(":memory:").unwrap();
    db.enable_structured_memory().unwrap();
    let mut view = StructuredPublicationView {
        scope: first.delta.publication.scope.clone(),
        visible_publication_ids: vec![],
        ancestors: vec![],
    };
    db.insert_structured_delta(&first.delta, &view).unwrap();
    view.visible_publication_ids
        .push(first.delta.publication.publication_id.clone());
    let mut next = response;
    next["entity_candidates"] = json!([]);
    next["fact_deltas"][0]["source_id"] = json!(intent.sources[3].source_id);
    next["fact_deltas"][0]["evidence_span"] =
        json!({"start":0,"length":intent.sources[3].content.len()});
    next["fact_deltas"][0]["evidence_text"] = json!(intent.sources[3].content);
    next["fact_deltas"][0]["relation"] = json!("replace");
    next["fact_deltas"][0]["speech_act"] = json!("correct");
    next["fact_deltas"][0]["value"] = json!(2);
    let result = validate_response_with_resolver(
        &snapshot,
        1,
        &intent,
        &next.to_string(),
        &PublishedResolver {
            memory: &db,
            view: &view,
        },
    )
    .unwrap();
    assert!(result.delta.entities.is_empty());
    assert_eq!(
        result
            .delta
            .facts
            .iter()
            .filter(|f| f.status == FactStatus::Superseded)
            .count(),
        1
    );
    assert_eq!(
        result
            .delta
            .facts
            .iter()
            .filter(|f| f.status == FactStatus::Active)
            .count(),
        1
    );
}

#[test]
fn structured_withdrawal_requires_explicit_replacement_before_reusing_a_value() {
    let (mut snapshot, _, mut response) = fixture();
    let content = "Adopt Aster retry_limit 4 for production.";
    snapshot.events.push(RawEventV2 {
        schema_version: 2,
        sequence: 6,
        epoch: 0,
        turn_id: 6,
        starts_turn: true,
        message: Message::user(content),
    });
    let raw = snapshot_raw(&snapshot).unwrap();
    snapshot.raw_hash = content_hash(&raw);
    snapshot.raw_offset = raw.len() as u64;
    snapshot.state.raw_commit_offset = snapshot.raw_offset;
    snapshot.state.raw_commit_hash = snapshot.raw_hash.clone();
    let intent = structured_intent(&snapshot).unwrap().unwrap();
    response["processed_source_ids"] = json!(
        intent
            .sources
            .iter()
            .map(|s| &s.source_id)
            .collect::<Vec<_>>()
    );
    let next = json!({"source_id":intent.sources[5].source_id,"evidence_span":{"start":0,"length":content.len()},"evidence_text":content,"ordinal":0,"speech_act":"adopt","entity_kind":"product","entity_name":"Aster","subject_project":"project","fact_kind":"retry_limit","value":4,"unit":"retries","condition":"production","relation":"assert"});
    response["fact_deltas"].as_array_mut().unwrap().push(next);
    assert!(validate_response(&snapshot, 1, &intent, &response.to_string()).is_err());
    response["fact_deltas"]
        .as_array_mut()
        .unwrap()
        .last_mut()
        .unwrap()["relation"] = json!("replace");
    let result = validate_response(&snapshot, 1, &intent, &response.to_string()).unwrap();
    assert!(
        result
            .delta
            .facts
            .iter()
            .any(|f| f.fact_kind == "unresolved" && f.status == FactStatus::Superseded)
    );
    assert!(
        result
            .delta
            .facts
            .iter()
            .any(|f| f.value_json == "4" && f.status == FactStatus::Active)
    );
    assert!(
        result
            .delta
            .facts
            .iter()
            .any(|f| f.value_json == "5" && f.status == FactStatus::Proposed)
    );
}

#[test]
fn structured_replacement_plan_keeps_explicit_dependencies_and_invalidates_on_withdrawal() {
    let (snapshot, intent, mut response) = fixture();
    let mut plan = response["fact_deltas"][3].clone();
    plan["source_id"] = json!(intent.sources[3].source_id);
    plan["evidence_span"] = json!({"start":0,"length":intent.sources[3].content.len()});
    plan["evidence_text"] = json!(intent.sources[3].content);
    plan["entity_name"] = json!("Aster");
    plan["depends_on"][0]["entity_name"] = json!("Aster");
    plan["relation"] = json!("replace");
    plan["speech_act"] = json!("correct");
    response["fact_deltas"]
        .as_array_mut()
        .unwrap()
        .push(plan.clone());
    let result = validate_response(&snapshot, 1, &intent, &response.to_string()).unwrap();
    let new_limit = result
        .delta
        .facts
        .iter()
        .find(|f| f.fact_kind == "retry_limit" && f.value_json == "2")
        .unwrap();
    let new_plan = result
        .delta
        .facts
        .iter()
        .find(|f| f.fact_kind == "plan" && f.source_id == intent.sources[4].source_id)
        .unwrap();
    assert_eq!(new_plan.status, FactStatus::NeedsReconfirmation);
    assert!(
        result
            .delta
            .dependencies
            .iter()
            .any(|d| d.dependent_fact_id == new_plan.fact_id
                && d.dependency_fact_id == new_limit.fact_id)
    );
    for target in ["missing", "unresolved"] {
        let mut bad = response.clone();
        bad["fact_deltas"]
            .as_array_mut()
            .unwrap()
            .last_mut()
            .unwrap()["depends_on"][0]["fact_kind"] = json!(target);
        assert!(validate_response(&snapshot, 1, &intent, &bad.to_string()).is_err());
    }
}

#[test]
fn structured_same_source_conflicting_fact_ordinals_reject_the_whole_chunk() {
    let (snapshot, intent, mut response) = fixture();
    let mut conflict = response["fact_deltas"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .clone();
    conflict["ordinal"] = json!(2);
    conflict["speech_act"] = json!("correct");
    conflict["relation"] = json!("replace");
    conflict["value"] = json!(2);
    response["fact_deltas"]
        .as_array_mut()
        .unwrap()
        .push(conflict);
    assert!(validate_response(&snapshot, 1, &intent, &response.to_string()).is_err());
}

#[test]
fn structured_related_entity_hints_have_a_separate_complete_token_budget() {
    let hint = polaris_memory::structured::StructuredEntityHint {
        entity_id: "a".repeat(64),
        kind: EntityKind::Product,
        name: "Rigel".into(),
        subject_project: "project".into(),
        matching_aliases: vec![],
    };
    hint_budget(std::slice::from_ref(&hint)).unwrap();
    let mut oversized = hint.clone();
    oversized.matching_aliases = vec!["\u{10000}".repeat(1500)];
    assert!(hint_budget(&[oversized]).is_err());
    assert!(hint_budget(&vec![hint; 65]).is_err());
}
