//! Supplemental user-role content never supplies decision authority.
use super::*;
use polaris_provider::Message;
use serde_json::{Value, json};

fn refresh(snapshot: &mut ConversationSnapshot) -> StructuredIntent {
    let mut turn = 0;
    for event in &mut snapshot.events {
        turn += u64::from(event.starts_turn);
        event.turn_id = turn;
    }
    let raw = snapshot_raw(snapshot).unwrap();
    snapshot.raw_hash = content_hash(&raw);
    snapshot.raw_offset = raw.len() as u64;
    snapshot.state.raw_commit_hash = snapshot.raw_hash.clone();
    snapshot.state.raw_commit_offset = snapshot.raw_offset;
    structured_intent(snapshot).unwrap().unwrap()
}

fn supplemental(snapshot: &mut ConversationSnapshot, indices: &[usize]) -> StructuredIntent {
    for &index in indices {
        snapshot.events[index].starts_turn = false;
    }
    refresh(snapshot)
}

fn rebind(
    response: &mut ValidatedStructuredResponse,
    snapshot: &ConversationSnapshot,
    intent: &StructuredIntent,
) {
    response.coverage = intent.coverage.clone();
    response.delta.publication.raw_hash = snapshot.raw_hash.clone();
    response.delta.publication.publication_id =
        format!("structured10-1-{}", &snapshot.raw_hash[..16]);
    response.delta.publication.delta_hash =
        polaris_memory::structured::delta_hash(&response.delta).unwrap();
}

fn saved(
    snapshot: &ConversationSnapshot,
    intent: &StructuredIntent,
    value: &ValidatedStructuredResponse,
) -> io::Result<()> {
    validate_saved_response(
        snapshot,
        1,
        intent,
        &value.summary,
        &value.delta,
        &value.coverage,
    )
}

#[test]
fn host_source_cannot_activate_correct_or_withdraw_real_user_decisions() {
    let (mut snapshot, original, mut response) = transitions_tests::fixture();
    let intent = supplemental(&mut snapshot, &[2, 3, 4]);
    // The old source wire is deliberately unchanged; authority comes from raw.
    assert_eq!(
        serde_json::to_value(&original.sources).unwrap(),
        serde_json::to_value(&intent.sources).unwrap()
    );
    let proposed = response["fact_deltas"]
        .as_array_mut()
        .unwrap()
        .iter_mut()
        .find(|v| v["source_id"] == intent.sources[2].source_id)
        .unwrap();
    proposed["speech_act"] = json!("adopt");
    proposed["relation"] = json!("assert");
    proposed["condition"] = json!("preview");
    let result = validate_response(&snapshot, 1, &intent, &response.to_string()).unwrap();
    for source in &intent.sources[2..] {
        let rows: Vec<_> = result
            .delta
            .facts
            .iter()
            .filter(|row| row.source_id == source.source_id)
            .collect();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].status, FactStatus::Proposed);
        assert!(rows[0].successor_fact_id.is_none());
    }
    assert!(
        result
            .delta
            .facts
            .iter()
            .filter(|row| row.source_id == intent.sources[0].source_id)
            .all(|row| row.status == FactStatus::Active)
    );
    saved(&snapshot, &intent, &result).unwrap();
}

#[test]
fn host_source_cannot_rename_even_an_entity_created_in_the_same_delta() {
    let (mut snapshot, original, response) = transitions_tests::fixture();
    validate_response(&snapshot, 1, &original, &response.to_string()).unwrap();
    let intent = supplemental(&mut snapshot, &[1]);
    let error = validate_response(&snapshot, 1, &intent, &response.to_string()).unwrap_err();
    assert!(error.to_string().contains("同一性変更"));
}

fn relation_fixture(user_input: bool) -> (ConversationSnapshot, StructuredIntent, Value, u64) {
    let (mut snapshot, _, _) = transitions_tests::fixture();
    snapshot.events.truncate(2);
    snapshot.events[0].message = Message::user("Inspect the following material.");
    snapshot.events[1].message =
        Message::user("The configuration key retry_limit belongs to product Rigel.");
    snapshot.events[1].starts_turn = user_input;
    let intent = refresh(&mut snapshot);
    let source = &intent.sources[1];
    let evidence = json!({"source_id":source.source_id, "evidence_span":{"start":0,"length":source.content.len()}, "evidence_text":source.content, "speech_act":"assert"});
    let reference = |kind, name| json!({"kind":kind,"name":name,"subject_project":"project"});
    let mut entities = vec![];
    for (ordinal, (kind, name)) in [("product", "Rigel"), ("configuration_key", "retry_limit")]
        .into_iter()
        .enumerate()
    {
        let mut row = evidence.clone();
        row.as_object_mut()
            .unwrap()
            .extend(reference(kind, name).as_object().unwrap().clone());
        row["ordinal"] = json!(ordinal);
        entities.push(row);
    }
    let mut relation = evidence;
    relation["from"] = reference("configuration_key", "retry_limit");
    relation["to"] = reference("product", "Rigel");
    relation["relation"] = json!("belongs_to");
    let turn = snapshot.events[1].turn_id;
    let response = json!({
        "summary":{"facts":["retry_limit belongs to Rigel"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[turn]},
        "processed_source_ids":intent.sources.iter().map(|s| &s.source_id).collect::<Vec<_>>(),
        "entity_candidates":entities,"entity_relations":[relation],"fact_deltas":[]
    });
    (snapshot, intent, response, turn)
}

#[test]
fn host_source_cannot_establish_entity_relations_but_real_user_input_can() {
    let (snapshot, intent, response, turn) = relation_fixture(true);
    let accepted = validate_response(&snapshot, turn, &intent, &response.to_string()).unwrap();
    assert_eq!(accepted.delta.relations.len(), 1);
    let (snapshot, intent, response, turn) = relation_fixture(false);
    let error = validate_response(&snapshot, turn, &intent, &response.to_string()).unwrap_err();
    assert!(error.to_string().contains("entity関係"));
}

#[test]
fn host_source_new_entity_observation_is_preserved_without_a_relation() {
    let (snapshot, intent, mut response, turn) = relation_fixture(false);
    response["entity_relations"] = json!([]);
    let accepted = validate_response(&snapshot, turn, &intent, &response.to_string()).unwrap();
    assert_eq!(accepted.delta.entities.len(), 2);
    assert!(accepted.delta.facts.is_empty() && accepted.delta.relations.is_empty());
    saved(&snapshot, &intent, &accepted).unwrap();
}

#[test]
fn host_source_saved_receipts_cannot_promote_actions_or_overwrite_existing_fact_ids() {
    let (snapshot, intent, response) = transitions_tests::fixture();
    let original = validate_response(&snapshot, 1, &intent, &response.to_string()).unwrap();
    saved(&snapshot, &intent, &original).unwrap();
    // Simulate the old role-only reader accepting the same source text against
    // a canonical raw prefix whose relevant message was supplemental content.
    for indices in [&[1][..], &[3, 4][..]] {
        let mut snapshot = snapshot.clone();
        let intent = supplemental(&mut snapshot, indices);
        let mut old = original.clone();
        // Isolate the revision check from the alias check: the entity remains
        // new in this delta, but its later rename has a different origin.
        old.delta.aliases.clear();
        rebind(&mut old, &snapshot, &intent);
        assert!(
            saved(&snapshot, &intent, &old)
                .unwrap_err()
                .to_string()
                .contains("利用者入力")
        );
    }
    let mut snapshot = snapshot;
    let intent = supplemental(&mut snapshot, &[2]);
    let mut old = original;
    let mut wrong = old
        .delta
        .facts
        .iter()
        .find(|f| f.source_id == intent.sources[0].source_id && f.fact_kind == "retry_limit")
        .unwrap()
        .clone();
    wrong.source_id = intent.sources[2].source_id.clone();
    wrong.status = FactStatus::Proposed;
    wrong.reason = "unconfirmed".into();
    wrong.successor_fact_id = None;
    old.delta.facts = vec![wrong];
    old.delta.dependencies.clear();
    rebind(&mut old, &snapshot, &intent);
    assert!(
        saved(&snapshot, &intent, &old)
            .unwrap_err()
            .to_string()
            .contains("利用者入力")
    );
}

#[test]
fn host_source_saved_aliases_and_dependencies_require_real_user_input() {
    let (mut snapshot, original_intent, response) = transitions_tests::fixture();
    let original =
        validate_response(&snapshot, 1, &original_intent, &response.to_string()).unwrap();
    let intent = supplemental(&mut snapshot, &[2]);
    for alias in [false, true] {
        let mut old = original.clone();
        if alias {
            old.delta.aliases[0].source_id = intent.sources[2].source_id.clone();
        } else {
            old.delta.dependencies[0].source_id = intent.sources[2].source_id.clone();
        }
        rebind(&mut old, &snapshot, &intent);
        validate_delta(&old.delta).unwrap();
        assert!(
            saved(&snapshot, &intent, &old)
                .unwrap_err()
                .to_string()
                .contains("利用者入力")
        );
    }
}

#[test]
fn host_source_saved_entity_relations_cannot_gain_authority_on_reopen() {
    let (mut snapshot, original_intent, response, turn) = relation_fixture(true);
    let mut old =
        validate_response(&snapshot, turn, &original_intent, &response.to_string()).unwrap();
    let intent = supplemental(&mut snapshot, &[1]);
    let mut summary: Value = serde_json::from_str(&old.summary).unwrap();
    summary["source_turn_ids"] = json!([1]);
    old.summary = summary.to_string();
    rebind(&mut old, &snapshot, &intent);
    validate_delta(&old.delta).unwrap();
    assert!(
        saved(&snapshot, &intent, &old)
            .unwrap_err()
            .to_string()
            .contains("利用者入力")
    );
    old.delta.relations.clear();
    rebind(&mut old, &snapshot, &intent);
    saved(&snapshot, &intent, &old).unwrap();
}

#[test]
fn host_source_change_preserves_legacy_user_intent_and_receipt_roundtrip() {
    let (snapshot, intent, response) = transitions_tests::fixture();
    let wire = serde_json::to_string(&intent).unwrap();
    assert!(
        serde_json::to_value(&intent).unwrap()["sources"][0]
            .get("starts_turn")
            .is_none()
    );
    let restored: StructuredIntent = serde_json::from_str(&wire).unwrap();
    assert_eq!(serde_json::to_string(&restored).unwrap(), wire);
    let accepted = validate_response(&snapshot, 1, &restored, &response.to_string()).unwrap();
    saved(&snapshot, &restored, &accepted).unwrap();
}
