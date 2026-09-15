//! Frozen-scope entity and fact overlay regression checks.

use polaris_memory::structured::{
    EntityKind, FactStatus, OriginScope, PublicationScope, StructuredAlias,
    StructuredAncestorPublicationRange, StructuredDelta, StructuredEntity,
    StructuredEntityRevision, StructuredFactDependency, StructuredFactRevision,
    StructuredPublication, StructuredPublicationView, delta_hash, stable_entity_id, stable_fact_id,
};
use polaris_memory::{MemoryStore, Result};

const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn scope(session: &str, generation: i64) -> PublicationScope {
    PublicationScope {
        project_id: "p".into(),
        session_id: session.into(),
        epoch: 0,
        generation,
    }
}

fn view(
    scope: PublicationScope,
    visible: &[&str],
    ancestors: &[(&str, i64, &[&str])],
) -> StructuredPublicationView {
    StructuredPublicationView {
        scope,
        visible_publication_ids: visible.iter().map(|value| (*value).into()).collect(),
        ancestors: ancestors
            .iter()
            .map(
                |(session, generation, publications)| StructuredAncestorPublicationRange {
                    session_id: (*session).into(),
                    epoch: 0,
                    generation: *generation,
                    visible_publication_ids: publications
                        .iter()
                        .map(|value| (*value).into())
                        .collect(),
                },
            )
            .collect(),
    }
}

fn delta(
    scope: PublicationScope,
    publication_id: &str,
    entities: Vec<StructuredEntity>,
    entity_revisions: Vec<StructuredEntityRevision>,
    aliases: Vec<StructuredAlias>,
    facts: Vec<StructuredFactRevision>,
    dependencies: Vec<StructuredFactDependency>,
) -> StructuredDelta {
    let mut delta = StructuredDelta {
        publication: StructuredPublication {
            scope,
            publication_id: publication_id.into(),
            raw_hash: HASH.into(),
            delta_hash: HASH.into(),
            prompt_version: "v3".into(),
        },
        entities,
        entity_revisions,
        aliases,
        facts,
        dependencies,
        relations: vec![],
    };
    delta.publication.delta_hash = delta_hash(&delta).unwrap();
    delta
}

fn insert(
    memory: &mut MemoryStore,
    delta: &StructuredDelta,
    grants: &StructuredPublicationView,
) -> Result<()> {
    memory.insert_structured_delta(delta, grants)
}

#[test]
fn overlay_revisions_are_current_per_id_and_scope() {
    let mut memory = MemoryStore::open(":memory:").unwrap();
    memory.enable_structured_memory().unwrap();

    let parent_origin = OriginScope {
        project_id: "p".into(),
        session_id: "parent".into(),
        epoch: 0,
    };
    let product =
        stable_entity_id(&parent_origin, "product-source", 0, EntityKind::Product).unwrap();
    let path = stable_entity_id(&parent_origin, "path-source", 0, EntityKind::Path).unwrap();
    let endpoint = stable_fact_id(&product, "endpoint", "", "fact-source").unwrap();
    let plan = stable_fact_id(&product, "plan", "", "plan-source").unwrap();
    let parent_scope = scope("parent", 1);
    let base = delta(
        parent_scope.clone(),
        "p1",
        vec![
            StructuredEntity {
                entity_id: product.clone(),
                origin: parent_origin.clone(),
                origin_source_id: "product-source".into(),
                origin_ordinal: 0,
                kind: EntityKind::Product,
            },
            StructuredEntity {
                entity_id: path.clone(),
                origin: parent_origin.clone(),
                origin_source_id: "path-source".into(),
                origin_ordinal: 0,
                kind: EntityKind::Path,
            },
        ],
        vec![
            StructuredEntityRevision {
                entity_id: product.clone(),
                source_id: "product-source".into(),
                canonical_name: "Old Name".into(),
                description: "d".into(),
                subject_project: "old-project".into(),
            },
            StructuredEntityRevision {
                entity_id: path.clone(),
                source_id: "path-source".into(),
                canonical_name: "/Src/Main".into(),
                description: "d".into(),
                subject_project: "child-project".into(),
            },
        ],
        vec![StructuredAlias {
            entity_id: product.clone(),
            display_alias: "Legacy Name".into(),
            match_alias: "Legacy Name".into(),
            relation: "alias".into(),
            source_id: "product-source".into(),
        }],
        vec![
            StructuredFactRevision {
                fact_id: endpoint.clone(),
                entity_id: product.clone(),
                fact_kind: "endpoint".into(),
                value_json: "\"old\"".into(),
                unit: "none".into(),
                condition: "".into(),
                status: FactStatus::Active,
                reason: "assert".into(),
                successor_fact_id: None,
                source_id: "fact-source".into(),
            },
            StructuredFactRevision {
                fact_id: plan.clone(),
                entity_id: product.clone(),
                fact_kind: "plan".into(),
                value_json: "\"plan\"".into(),
                unit: "none".into(),
                condition: "".into(),
                status: FactStatus::Active,
                reason: "assert".into(),
                successor_fact_id: None,
                source_id: "plan-source".into(),
            },
        ],
        vec![StructuredFactDependency {
            dependent_fact_id: plan.clone(),
            dependency_fact_id: endpoint.clone(),
            source_id: "plan-source".into(),
        }],
    );
    insert(&mut memory, &base, &view(parent_scope.clone(), &[], &[])).unwrap();

    for (generation, publication_id, status) in [
        (100, "p2", FactStatus::Superseded),
        (101, "p3", FactStatus::Active),
    ] {
        let revision = delta(
            scope("parent", generation),
            publication_id,
            vec![],
            vec![StructuredEntityRevision {
                entity_id: product.clone(),
                source_id: format!("parent-{generation}"),
                canonical_name: format!("Parent {generation}"),
                description: "d".into(),
                subject_project: "old-project".into(),
            }],
            vec![],
            vec![StructuredFactRevision {
                fact_id: endpoint.clone(),
                entity_id: product.clone(),
                fact_kind: "endpoint".into(),
                value_json: format!("\"parent-{generation}\""),
                unit: "none".into(),
                condition: "".into(),
                status,
                reason: "replace".into(),
                successor_fact_id: None,
                source_id: "fact-source".into(),
            }],
            vec![],
        );
        insert(
            &mut memory,
            &revision,
            &view(scope("parent", generation), &["p1"], &[]),
        )
        .unwrap();
    }

    let child_one_scope = scope("child", 1);
    let child_one = delta(
        child_one_scope.clone(),
        "c1",
        vec![],
        vec![StructuredEntityRevision {
            entity_id: product.clone(),
            source_id: "child-rename".into(),
            canonical_name: "MiXeD Name".into(),
            description: "d".into(),
            subject_project: "child-project".into(),
        }],
        vec![],
        vec![StructuredFactRevision {
            fact_id: endpoint.clone(),
            entity_id: product.clone(),
            fact_kind: "endpoint".into(),
            value_json: "\"child\"".into(),
            unit: "none".into(),
            condition: "".into(),
            status: FactStatus::Active,
            reason: "assert".into(),
            successor_fact_id: None,
            source_id: "fact-source".into(),
        }],
        vec![],
    );
    let parent_grants = [("parent", 100, &["p1", "p2"][..])];
    insert(
        &mut memory,
        &child_one,
        &view(child_one_scope.clone(), &[], &parent_grants),
    )
    .unwrap();

    let child_view = view(child_one_scope.clone(), &["c1"], &parent_grants);
    assert_eq!(
        memory
            .resolve_structured_entity(
                &child_view,
                EntityKind::Product,
                "  mixed name  ",
                "child-project",
            )
            .unwrap(),
        Some(product.clone()),
    );
    assert_eq!(
        memory
            .resolve_structured_entity(&child_view, EntityKind::Product, "Old Name", "old-project",)
            .unwrap(),
        None,
        "a local revision suppresses the same ID's parent canonical name",
    );
    assert_eq!(
        memory
            .resolve_structured_entity(
                &child_view,
                EntityKind::Product,
                " legacy name ",
                "child-project",
            )
            .unwrap(),
        Some(product.clone()),
    );
    assert_eq!(
        memory
            .resolve_structured_entity(
                &child_view,
                EntityKind::Product,
                "legacy name",
                "old-project",
            )
            .unwrap(),
        None,
        "an inherited alias uses the entity's current subject project",
    );
    assert_eq!(
        memory
            .resolve_structured_entity(&child_view, EntityKind::Path, "/src/main", "child-project")
            .unwrap(),
        None,
    );
    assert_eq!(
        memory
            .resolve_structured_entity(&child_view, EntityKind::Path, "/Src/Main", "child-project")
            .unwrap(),
        Some(path),
    );
    let fact = memory
        .resolve_structured_fact(&child_view, &product, "endpoint", "")
        .unwrap()
        .unwrap();
    assert_eq!(
        (fact.fact_id, fact.status, fact.scope.generation),
        (endpoint.clone(), FactStatus::Active, 1)
    );

    let child_two_scope = scope("child", 2);
    let child_two = delta(
        child_two_scope.clone(),
        "c2",
        vec![],
        vec![],
        vec![],
        vec![StructuredFactRevision {
            fact_id: endpoint.clone(),
            entity_id: product.clone(),
            fact_kind: "endpoint".into(),
            value_json: "\"withdrawn\"".into(),
            unit: "none".into(),
            condition: "".into(),
            status: FactStatus::Superseded,
            reason: "withdrawn".into(),
            successor_fact_id: None,
            source_id: "fact-source".into(),
        }],
        vec![],
    );
    insert(
        &mut memory,
        &child_two,
        &view(child_two_scope.clone(), &["c1"], &parent_grants),
    )
    .unwrap();
    let latest_child = view(child_two_scope, &["c1", "c2"], &parent_grants);
    assert_eq!(
        memory
            .structured_fact_revision(&latest_child, &endpoint)
            .unwrap(),
        Some(child_two.facts[0].clone())
    );
    assert!(
        memory
            .structured_fact_revision(&view(scope("unrelated", 2), &[], &[]), &endpoint)
            .unwrap()
            .is_none()
    );
    let evidence = memory
        .structured_fact_evidence(&latest_child, &endpoint)
        .unwrap()
        .unwrap();
    assert_eq!(evidence.fact, child_two.facts[0]);
    assert_eq!(evidence.publication, child_two.publication);
    let inherited = memory
        .structured_fact_evidence(&view(scope("child", 0), &[], &parent_grants), &endpoint)
        .unwrap()
        .unwrap();
    assert_eq!(inherited.publication.scope.session_id, "parent");
    assert_eq!(inherited.publication.publication_id, "p2");
    assert_eq!(inherited.publication.scope.generation, 100);
    assert_eq!(inherited.publication.raw_hash, HASH);
    assert!(
        memory
            .structured_fact_evidence(&view(scope("parent", 1), &[], &[]), &endpoint,)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        memory
            .structured_fact_candidates(&latest_child, &product, "endpoint", "")
            .unwrap(),
        child_two.facts
    );
    assert_eq!(
        memory
            .resolve_structured_fact(&latest_child, &product, "endpoint", "")
            .unwrap()
            .unwrap()
            .status,
        FactStatus::Superseded,
    );

    let duplicate_origin = OriginScope {
        project_id: "p".into(),
        session_id: "child".into(),
        epoch: 0,
    };
    let duplicate = stable_entity_id(
        &duplicate_origin,
        "duplicate-source",
        0,
        EntityKind::Product,
    )
    .unwrap();
    let child_three_scope = scope("child", 3);
    let child_three = delta(
        child_three_scope.clone(),
        "c3",
        vec![StructuredEntity {
            entity_id: duplicate.clone(),
            origin: duplicate_origin,
            origin_source_id: "duplicate-source".into(),
            origin_ordinal: 0,
            kind: EntityKind::Product,
        }],
        vec![StructuredEntityRevision {
            entity_id: duplicate,
            source_id: "duplicate-source".into(),
            canonical_name: "MiXeD Name".into(),
            description: "d".into(),
            subject_project: "child-project".into(),
        }],
        vec![],
        vec![],
        vec![],
    );
    insert(
        &mut memory,
        &child_three,
        &view(child_three_scope.clone(), &["c1", "c2"], &parent_grants),
    )
    .unwrap();
    let ambiguous_child = view(child_three_scope, &["c1", "c2", "c3"], &parent_grants);
    assert!(
        memory
            .resolve_structured_entity(
                &ambiguous_child,
                EntityKind::Product,
                "mixed name",
                "child-project",
            )
            .is_err()
    );

    assert_eq!(
        memory
            .structured_dependents(&latest_child, &endpoint)
            .unwrap(),
        vec![plan.clone()],
    );
    memory.delete_session("p", "parent").unwrap();
    assert!(
        memory
            .structured_dependents(&latest_child, &endpoint)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn fact_evidence_rejects_orphaned_or_corrupt_publication_provenance() {
    let temporary = tempfile::tempdir().unwrap();
    let path = temporary.path().join("memory.sqlite");
    let mut memory = MemoryStore::open(&path).unwrap();
    memory.enable_structured_memory().unwrap();
    let origin = OriginScope {
        project_id: "p".into(),
        session_id: "s".into(),
        epoch: 0,
    };
    let entity_id = stable_entity_id(&origin, "entity", 0, EntityKind::ConfigurationKey).unwrap();
    let fact_id = stable_fact_id(&entity_id, "limit", "production", "source-v3:p:s:0:9").unwrap();
    let publication = delta(
        scope("s", 1),
        "pub1",
        vec![StructuredEntity {
            entity_id: entity_id.clone(),
            origin,
            origin_source_id: "entity".into(),
            origin_ordinal: 0,
            kind: EntityKind::ConfigurationKey,
        }],
        vec![StructuredEntityRevision {
            entity_id: entity_id.clone(),
            source_id: "entity".into(),
            canonical_name: "retry_limit".into(),
            description: "Retry limit".into(),
            subject_project: "p".into(),
        }],
        vec![],
        vec![StructuredFactRevision {
            fact_id: fact_id.clone(),
            entity_id,
            fact_kind: "limit".into(),
            value_json: "3".into(),
            unit: "retries".into(),
            condition: "production".into(),
            status: FactStatus::Active,
            reason: "assert".into(),
            successor_fact_id: None,
            source_id: "source-v3:p:s:0:9".into(),
        }],
        vec![],
    );
    insert(&mut memory, &publication, &view(scope("s", 0), &[], &[])).unwrap();
    let grants = view(scope("s", 1), &["pub1"], &[]);
    let evidence = memory
        .structured_fact_evidence(&grants, &fact_id)
        .unwrap()
        .unwrap();
    assert_eq!(evidence.fact, publication.facts[0]);
    assert_eq!(evidence.publication, publication.publication);
    let mut foreign = grants.clone();
    foreign.scope.project_id = "other".into();
    assert!(
        memory
            .structured_fact_evidence(&foreign, &fact_id)
            .unwrap()
            .is_none()
    );
    let connection = rusqlite::Connection::open(&path).unwrap();
    connection.execute("UPDATE conversation_publications_v3 SET raw_hash='invalid' WHERE publication_id='pub1'", []).unwrap();
    assert!(memory.structured_fact_evidence(&grants, &fact_id).is_err());
    connection
        .execute(
            "UPDATE conversation_publications_v3 SET raw_hash=?1 WHERE publication_id='pub1'",
            [HASH],
        )
        .unwrap();
    assert_eq!(
        memory
            .structured_fact_evidence(&grants, &fact_id)
            .unwrap()
            .unwrap()
            .publication
            .raw_hash,
        HASH
    );
    // Preserve the fact row while simulating an incomplete/corrupt index.
    connection
        .execute_batch("PRAGMA foreign_keys=OFF;")
        .unwrap();
    connection
        .execute(
            "DELETE FROM conversation_publications_v3 WHERE publication_id='pub1'",
            [],
        )
        .unwrap();
    assert!(memory.structured_fact_evidence(&grants, &fact_id).is_err());
    assert!(memory.structured_fact_revision(&grants, &fact_id).is_err());
}
