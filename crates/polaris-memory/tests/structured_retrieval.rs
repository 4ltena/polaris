//! Frozen structured retrieval preserves identity, withdrawal and dependencies.

use polaris_memory::{MemoryStore, structured::*};

const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn scope(session: &str, generation: i64) -> PublicationScope {
    PublicationScope {
        project_id: "project".into(),
        session_id: session.into(),
        epoch: 0,
        generation,
    }
}
fn view(session: &str, generation: i64, ids: &[&str]) -> StructuredPublicationView {
    StructuredPublicationView {
        scope: scope(session, generation),
        visible_publication_ids: ids.iter().map(|s| s.to_string()).collect(),
        ancestors: vec![],
    }
}
fn delta(generation: i64, id: &str) -> StructuredDelta {
    StructuredDelta {
        publication: StructuredPublication {
            scope: scope("s", generation),
            publication_id: id.into(),
            raw_hash: HASH.into(),
            delta_hash: HASH.into(),
            prompt_version: "fixture".into(),
        },
        entities: vec![],
        entity_revisions: vec![],
        aliases: vec![],
        facts: vec![],
        dependencies: vec![],
        relations: vec![],
    }
}
fn publish(db: &mut MemoryStore, delta: &mut StructuredDelta, ids: &[&str]) {
    delta.publication.delta_hash = delta_hash(delta).unwrap();
    db.insert_structured_delta(
        delta,
        &view("s", delta.publication.scope.generation - 1, ids),
    )
    .unwrap();
}
fn entity(delta: &mut StructuredDelta, name: &str, kind: EntityKind, ordinal: u32) -> String {
    let origin = OriginScope {
        project_id: "project".into(),
        session_id: "s".into(),
        epoch: 0,
    };
    let id = stable_entity_id(&origin, "source-v3:project:s:0:1", ordinal, kind).unwrap();
    delta.entities.push(StructuredEntity {
        entity_id: id.clone(),
        origin,
        origin_source_id: "source-v3:project:s:0:1".into(),
        origin_ordinal: ordinal,
        kind,
    });
    delta.entity_revisions.push(StructuredEntityRevision {
        entity_id: id.clone(),
        source_id: "source-v3:project:s:0:1".into(),
        canonical_name: name.into(),
        description: name.into(),
        subject_project: "A".into(),
    });
    id
}
fn fact(entity: &str, kind: &str, value: &str, source: &str) -> StructuredFactRevision {
    StructuredFactRevision {
        fact_id: stable_fact_id(entity, kind, "production", source).unwrap(),
        entity_id: entity.into(),
        fact_kind: kind.into(),
        value_json: value.into(),
        unit: "retries".into(),
        condition: "production".into(),
        status: FactStatus::Active,
        reason: "assert".into(),
        successor_fact_id: None,
        source_id: source.into(),
    }
}

#[test]
fn retrieval_keeps_exact_rename_withdrawal_and_dependency_provenance() {
    let mut db = MemoryStore::open(":memory:").unwrap();
    db.enable_structured_memory().unwrap();
    let mut first = delta(1, "first");
    let product = entity(&mut first, "Lumen", EntityKind::Product, 0);
    let setting = entity(
        &mut first,
        "sync.retry_limit",
        EntityKind::ConfigurationKey,
        1,
    );
    let decision = fact(&setting, "decision", "3", "source-v3:project:s:0:1");
    let plan = fact(&setting, "plan", "\"P1\"", "source-v3:project:s:0:1");
    first.facts = vec![decision.clone(), plan.clone()];
    first.dependencies.push(StructuredFactDependency {
        dependent_fact_id: plan.fact_id.clone(),
        dependency_fact_id: decision.fact_id.clone(),
        source_id: plan.source_id.clone(),
    });
    first.relations.push(StructuredEntityRelation {
        source_entity_id: setting.clone(),
        target_entity_id: product.clone(),
        relation: "belongs_to".into(),
        source_id: decision.source_id.clone(),
    });
    publish(&mut db, &mut first, &[]);

    let mut rename = delta(2, "rename");
    rename.entity_revisions.push(StructuredEntityRevision {
        entity_id: product.clone(),
        source_id: "source-v3:project:s:0:2".into(),
        canonical_name: "Aster".into(),
        description: "Renamed product".into(),
        subject_project: "A".into(),
    });
    rename.aliases.push(StructuredAlias {
        entity_id: product.clone(),
        display_alias: "Lumen".into(),
        match_alias: "lumen".into(),
        relation: "rename".into(),
        source_id: "source-v3:project:s:0:2".into(),
    });
    publish(&mut db, &mut rename, &["first"]);

    let mut withdraw = delta(3, "withdraw");
    let mut old = decision.clone();
    old.status = FactStatus::Superseded;
    old.reason = "withdrawn".into();
    old.source_id = "source-v3:project:s:0:5".into();
    let mut dependent = plan.clone();
    dependent.status = FactStatus::NeedsReconfirmation;
    dependent.reason = "dependency withdrawn".into();
    dependent.source_id = old.source_id.clone();
    let mut unresolved = fact(&setting, "unresolved", "null", &old.source_id);
    unresolved.reason = format!("withdrawn_without_replacement:{}", old.fact_id);
    withdraw.facts = vec![old, dependent, unresolved];
    publish(&mut db, &mut withdraw, &["first", "rename"]);
    let grants = view("s", 3, &["first", "rename", "withdraw"]);
    let result = db
        .structured_retrieval_evidence(&grants, std::slice::from_ref(&product))
        .unwrap();
    assert_eq!(result.entities.len(), 2);
    let identity = result
        .entities
        .iter()
        .find(|r| r.row.revision.entity_id == product)
        .unwrap();
    assert_eq!(identity.row.revision.canonical_name, "Aster");
    assert_eq!(identity.publication, rename.publication);
    assert_eq!(result.aliases[0].row.display_alias, "Lumen");
    assert_eq!(result.relations[0].publication, first.publication);
    assert_eq!(result.dependencies[0].publication, first.publication);
    assert_eq!(result.facts.len(), 3);
    for row in &result.facts {
        assert_eq!(row.publication, withdraw.publication);
    }
    assert!(
        result
            .facts
            .iter()
            .any(|r| r.row.status == FactStatus::NeedsReconfirmation)
    );
    assert!(
        result
            .facts
            .iter()
            .any(|r| r.row.fact_kind == "unresolved" && r.row.value_json == "null")
    );

    let mut fork = view("child", 0, &[]);
    fork.ancestors.push(StructuredAncestorPublicationRange {
        session_id: "s".into(),
        epoch: 0,
        generation: 2,
        visible_publication_ids: vec!["first".into(), "rename".into()],
    });
    let frozen = db
        .structured_retrieval_evidence(&fork, std::slice::from_ref(&product))
        .unwrap();
    assert_eq!(frozen.facts.len(), 2);
    assert!(
        frozen
            .facts
            .iter()
            .all(|r| r.row.status == FactStatus::Active && r.publication.publication_id == "first")
    );
    assert!(
        db.structured_retrieval_evidence(&view("other", 3, &[]), std::slice::from_ref(&product))
            .is_err()
    );
    assert!(
        db.structured_retrieval_evidence(&view("s", 3, &["missing"]), &[])
            .is_err()
    );
    db.delete_session("project", "s").unwrap();
    assert!(
        db.structured_retrieval_evidence(&fork, std::slice::from_ref(&product))
            .is_err()
    );
}

#[test]
fn retrieval_caps_reject_the_whole_set_without_selecting_a_winner() {
    let mut db = MemoryStore::open(":memory:").unwrap();
    db.enable_structured_memory().unwrap();
    let mut first = delta(1, "first");
    let product = entity(&mut first, "Lumen", EntityKind::Product, 0);
    for n in 0..64 {
        first.facts.push(fact(
            &product,
            &format!("limit-{n}"),
            "3",
            "source-v3:project:s:0:1",
        ));
    }
    publish(&mut db, &mut first, &[]);
    let grants = view("s", 1, &["first"]);
    assert_eq!(
        db.structured_retrieval_evidence(&grants, std::slice::from_ref(&product))
            .unwrap()
            .facts
            .len(),
        64
    );
    let mut second = delta(2, "second");
    second.facts.push(fact(
        &product,
        "limit-overflow",
        "3",
        "source-v3:project:s:0:2",
    ));
    publish(&mut db, &mut second, &["first"]);
    assert!(
        db.structured_retrieval_evidence(
            &view("s", 2, &["first", "second"]),
            std::slice::from_ref(&product)
        )
        .is_err()
    );
    assert_eq!(
        db.structured_retrieval_evidence(&grants, std::slice::from_ref(&product))
            .unwrap()
            .facts
            .len(),
        64
    );
    assert!(
        db.structured_retrieval_evidence(&grants, &vec![product; 65])
            .is_err()
    );
}

#[test]
fn dependency_revision_ties_reject_across_publications_but_keep_one_complete_set() {
    let mut db = MemoryStore::open(":memory:").unwrap();
    db.enable_structured_memory().unwrap();
    let mut first = delta(1, "first");
    let product = entity(&mut first, "Lumen", EntityKind::Product, 0);
    let a = fact(&product, "a", "1", "source-v3:project:s:0:1");
    let b = fact(&product, "b", "2", "source-v3:project:s:0:1");
    let plan = fact(&product, "plan", "\"P1\"", "source-v3:project:s:0:1");
    first.facts = vec![a.clone(), b.clone(), plan.clone()];
    publish(&mut db, &mut first, &[]);
    let mut one = delta(2, "one");
    for target in [&a, &b] {
        one.dependencies.push(StructuredFactDependency {
            dependent_fact_id: plan.fact_id.clone(),
            dependency_fact_id: target.fact_id.clone(),
            source_id: "source-v3:project:s:0:2".into(),
        });
    }
    publish(&mut db, &mut one, &["first"]);
    let positive = db
        .structured_retrieval_evidence(
            &view("s", 2, &["first", "one"]),
            std::slice::from_ref(&product),
        )
        .unwrap();
    assert_eq!(positive.dependencies.len(), 2);
    assert!(
        positive
            .dependencies
            .iter()
            .all(|r| r.publication == one.publication)
    );
    let mut other = delta(2, "other");
    other.dependencies = vec![one.dependencies[0].clone()];
    publish(&mut db, &mut other, &["first"]);
    for ids in [["first", "one", "other"], ["other", "one", "first"]] {
        assert!(matches!(
            db.structured_retrieval_evidence(&view("s", 2, &ids), std::slice::from_ref(&product)),
            Err(polaris_memory::Error::InvalidInput(
                "structured dependency revision is ambiguous"
            ))
        ));
    }
}

#[test]
fn oversized_related_alias_evidence_returns_no_partial_dictionary() {
    let mut db = MemoryStore::open(":memory:").unwrap();
    db.enable_structured_memory().unwrap();
    let mut first = delta(1, "first");
    let product = entity(&mut first, "Lumen", EntityKind::Product, 0);
    publish(&mut db, &mut first, &[]);
    let mut large = delta(2, "large");
    for n in 0..512 {
        let name = format!("{n}-{}", "x".repeat(600));
        large.aliases.push(StructuredAlias {
            entity_id: product.clone(),
            display_alias: name.clone(),
            match_alias: name,
            relation: "alias".into(),
            source_id: "source-v3:project:s:0:2".into(),
        });
    }
    publish(&mut db, &mut large, &["first"]);
    assert!(matches!(
        db.structured_retrieval_evidence(
            &view("s", 2, &["first", "large"]),
            std::slice::from_ref(&product)
        ),
        Err(polaris_memory::Error::InvalidInput(
            "structured retrieval retained byte limit"
        ))
    ));
    assert!(
        db.structured_retrieval_evidence(&view("s", 1, &["first"]), &[product])
            .unwrap()
            .aliases
            .is_empty()
    );
}

#[test]
fn no_hit_validates_grants_without_scanning_unrelated_fact_rows() {
    let mut db = MemoryStore::open(":memory:").unwrap();
    db.enable_structured_memory().unwrap();
    let mut first = delta(1, "first");
    let product = entity(&mut first, "Lumen", EntityKind::Product, 0);
    publish(&mut db, &mut first, &[]);
    let mut grants = view("s", 1, &["first"]);
    // 16,448 valid facts exceed the retrieval scan cap but remain within the
    // 65,536 session identity limit. An unrelated query must still be no-hit.
    for n in 0..257 {
        let mut batch = delta(n + 2, &format!("facts-{n}"));
        let source = format!("source-v3:project:s:0:{}", n + 2);
        for index in 0..64 {
            batch
                .facts
                .push(fact(&product, &format!("decision-{index}"), "3", &source));
        }
        batch.publication.delta_hash = delta_hash(&batch).unwrap();
        // Each independent new fact only needs the original entity grant;
        // retaining all earlier grants here adds quadratic fixture setup work.
        let origin_view = view("s", grants.scope.generation, &["first"]);
        db.insert_structured_delta(&batch, &origin_view).unwrap();
        grants.scope.generation = batch.publication.scope.generation;
        grants
            .visible_publication_ids
            .push(batch.publication.publication_id.clone());
    }
    assert!(
        db.structured_entity_hints(&grants, &["unknown.value"])
            .unwrap()
            .is_empty()
    );
    let result = db.structured_retrieval_evidence(&grants, &[]).unwrap();
    assert!(
        result.entities.is_empty()
            && result.facts.is_empty()
            && result.aliases.is_empty()
            && result.dependencies.is_empty()
            && result.relations.is_empty()
    );
    let mut missing = grants.clone();
    missing.visible_publication_ids.push("missing".into());
    assert!(db.structured_retrieval_evidence(&missing, &[]).is_err());
    db.delete_session("project", "s").unwrap();
    assert!(db.structured_retrieval_evidence(&grants, &[]).is_err());
}

#[test]
fn raw_source_callback_receives_only_exact_granted_and_referenced_publications() {
    let mut db = MemoryStore::open(":memory:").unwrap();
    db.enable_structured_memory().unwrap();
    let mut first = delta(1, "first");
    let product = entity(&mut first, "Lumen", EntityKind::Product, 0);
    publish(&mut db, &mut first, &[]);
    let source = "source-v3:project:s:0:1";
    let calls = std::cell::Cell::new(0);
    let read = |publication: StructuredPublication| {
        calls.set(calls.get() + 1);
        Ok(publication)
    };
    let grants = view("s", 1, &["first"]);
    assert_eq!(
        db.with_structured_source(&grants, "first", source, read)
            .unwrap(),
        first.publication
    );
    assert_eq!(calls.get(), 1);
    for (publication, source) in [
        ("absent", source),
        ("first", "source-v3:project:s:0:2"),
        ("first", "source-v3:project:other:0:1"),
        ("first", "source-v3:project:s:0:01"),
        ("first", "source-v3:project:s:0:0"),
        ("first", "../../raw.jsonl"),
    ] {
        assert!(
            db.with_structured_source(&grants, publication, source, read)
                .is_err()
        );
    }
    assert_eq!(calls.get(), 1);
    let mut child = view("child", 0, &[]);
    assert!(
        db.with_structured_source(&child, "first", source, read)
            .is_err()
    );
    child.ancestors.push(StructuredAncestorPublicationRange {
        session_id: "s".into(),
        epoch: 0,
        generation: 1,
        visible_publication_ids: vec!["first".into()],
    });
    assert_eq!(
        db.with_structured_source(&child, "first", source, read)
            .unwrap(),
        first.publication
    );
    let mut later = delta(2, "later");
    later
        .facts
        .push(fact(&product, "decision", "7", "source-v3:project:s:0:2"));
    publish(&mut db, &mut later, &["first"]);
    assert!(
        db.with_structured_source(&child, "later", "source-v3:project:s:0:2", read)
            .is_err()
    );
    assert_eq!(calls.get(), 2);
    let mut missing = grants.clone();
    missing.visible_publication_ids = vec!["missing".into()];
    assert!(
        db.with_structured_source(&missing, "missing", source, read)
            .is_err()
    );
    db.delete_session("project", "s").unwrap();
    assert!(
        db.with_structured_source(&child, "first", source, read)
            .is_err()
    );
    assert_eq!(calls.get(), 2);
}
