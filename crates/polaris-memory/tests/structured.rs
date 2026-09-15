//! Structured-memory schema, publication, identity, and bounded lookup regressions.

use polaris_memory::structured::{
    EntityKind, FactRelation, FactStatus, OriginScope, PublicationScope, StructuredDelta,
    StructuredEntity, StructuredEntityRevision, StructuredFactDependency, StructuredFactRevision,
    StructuredPublication, StructuredPublicationView, stable_entity_id, stable_fact_id,
    validate_fact_transition, validate_publication,
};
use polaris_memory::{Error, MemoryStore, Record};

const HASH: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

fn empty_view(scope: &PublicationScope) -> StructuredPublicationView {
    StructuredPublicationView {
        scope: scope.clone(),
        visible_publication_ids: vec![],
        ancestors: vec![],
    }
}

fn entity_delta(session: &str, publication: &str) -> StructuredDelta {
    let origin = OriginScope {
        project_id: "p".into(),
        session_id: session.into(),
        epoch: 0,
    };
    let id = stable_entity_id(&origin, publication, 0, EntityKind::Product).unwrap();
    let mut delta = StructuredDelta {
        publication: StructuredPublication {
            scope: PublicationScope {
                project_id: "p".into(),
                session_id: session.into(),
                epoch: 0,
                generation: 1,
            },
            publication_id: publication.into(),
            raw_hash: HASH.into(),
            delta_hash: HASH.into(),
            prompt_version: "v3".into(),
        },
        entities: vec![StructuredEntity {
            entity_id: id.clone(),
            origin,
            origin_source_id: publication.into(),
            origin_ordinal: 0,
            kind: EntityKind::Product,
        }],
        entity_revisions: vec![StructuredEntityRevision {
            entity_id: id,
            source_id: publication.into(),
            canonical_name: "Rigel".into(),
            description: "fixture".into(),
            subject_project: "p".into(),
        }],
        aliases: vec![],
        facts: vec![],
        dependencies: vec![],
        relations: vec![],
    };
    seal(&mut delta);
    delta
}
fn seal(delta: &mut StructuredDelta) {
    delta.publication.delta_hash = polaris_memory::structured::delta_hash(delta).unwrap();
}

#[test]
fn structured_identity_ceiling_survives_epoch_changes_without_partial_publication() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("limits.sqlite");
    let mut db = MemoryStore::open(&path).unwrap();
    db.enable_structured_memory().unwrap();
    let base = entity_delta("s", "base");
    db.insert_structured_delta(&base, &empty_view(&base.publication.scope))
        .unwrap();
    let conn = rusqlite::Connection::open(&path).unwrap();
    // Populate only the index, without creating 65,536 provider interactions.
    conn.execute("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<65536) INSERT INTO conversation_fact_revisions_v3(project_id,session_id,epoch,generation,publication_id,fact_id,entity_id,fact_kind,value_json,unit,condition_text,status,reason,source_id) SELECT 'p','s',0,1,'base',printf('%064x',x),?1,'limit','1','count','','active','fixture','source' FROM n", [&base.entities[0].entity_id]).unwrap();
    let mut next = entity_delta("s", "next");
    next.publication.scope.epoch = 1;
    next.entities[0].origin.epoch = 1;
    let id = stable_entity_id(&next.entities[0].origin, "next", 0, EntityKind::Product).unwrap();
    next.entities[0].entity_id = id.clone();
    next.entity_revisions[0].entity_id = id.clone();
    next.facts.push(StructuredFactRevision {
        fact_id: stable_fact_id(&id, "limit", "", "new-source").unwrap(),
        entity_id: id,
        fact_kind: "limit".into(),
        value_json: "2".into(),
        unit: "count".into(),
        condition: String::new(),
        status: FactStatus::Active,
        reason: "assert".into(),
        successor_fact_id: None,
        source_id: "new-source".into(),
    });
    seal(&mut next);
    assert!(matches!(
        db.insert_structured_delta(&next, &empty_view(&next.publication.scope)),
        Err(Error::InvalidInput("structured session identity limit"))
    ));
    assert!(
        db.structured_publication(&next.publication.scope, "next")
            .unwrap()
            .is_none()
    );
    conn.execute(
        "DELETE FROM conversation_fact_revisions_v3 WHERE fact_id=printf('%064x',65536)",
        [],
    )
    .unwrap();
    db.insert_structured_delta(&next, &empty_view(&next.publication.scope))
        .unwrap();
    db.verify_structured_delta(&next).unwrap();
}

#[test]
fn structured_entity_hints_preserve_ambiguity_and_identifier_boundaries() {
    let mut db = MemoryStore::open(":memory:").unwrap();
    db.enable_structured_memory().unwrap();
    let first = entity_delta("hints", "first");
    db.insert_structured_delta(&first, &empty_view(&first.publication.scope))
        .unwrap();
    let mut second = entity_delta("hints", "second");
    second.entity_revisions[0].subject_project = "other-project".into();
    seal(&mut second);
    db.insert_structured_delta(&second, &empty_view(&second.publication.scope))
        .unwrap();
    let mut view = empty_view(&first.publication.scope);
    view.visible_publication_ids = vec!["first".into(), "second".into()];
    let hints = db
        .structured_entity_hints(&view, &["rigelについて確認"])
        .unwrap();
    assert_eq!(hints.len(), 2);
    assert_ne!(hints[0].entity_id, hints[1].entity_id);
    assert!(
        db.structured_entity_hints(&view, &["Rigelson"])
            .unwrap()
            .is_empty()
    );
    assert!(
        db.structured_entity_hints(&view, &["unknown"])
            .unwrap()
            .is_empty()
    );
    let mut code = entity_delta("hints", "code");
    code.entities[0].kind = EntityKind::ConfigurationKey;
    let id = stable_entity_id(
        &code.entities[0].origin,
        "code",
        0,
        EntityKind::ConfigurationKey,
    )
    .unwrap();
    code.entities[0].entity_id = id.clone();
    code.entity_revisions[0].entity_id = id;
    code.entity_revisions[0].canonical_name = "sync.retry_limit".into();
    seal(&mut code);
    db.insert_structured_delta(&code, &empty_view(&code.publication.scope))
        .unwrap();
    view.visible_publication_ids.push("code".into());
    assert_eq!(
        db.structured_entity_hints(&view, &["`sync.retry_limit`を確認"])
            .unwrap()
            .len(),
        1
    );
    for query in [
        "SYNC.RETRY_LIMIT",
        "sync.retry_limit_extra",
        "prefix.sync.retry_limit",
    ] {
        assert!(
            db.structured_entity_hints(&view, &[query])
                .unwrap()
                .is_empty()
        );
    }
    let hidden = empty_view(&first.publication.scope);
    assert!(
        db.structured_entity_hints(&hidden, &["Rigel"])
            .unwrap()
            .is_empty()
    );
    db.delete_session("p", "hints").unwrap();
    assert!(
        db.structured_entity_hints(&view, &["Rigel"])
            .unwrap()
            .is_empty()
    );
}

#[test]
fn structured_entity_hints_reject_oversized_ambiguity_without_selecting_a_winner() {
    let mut db = MemoryStore::open(":memory:").unwrap();
    db.enable_structured_memory().unwrap();
    let mut view = empty_view(&entity_delta("hints", "initial").publication.scope);
    for n in 0..65 {
        let p = format!("p{n}");
        let d = entity_delta("hints", &p);
        db.insert_structured_delta(&d, &empty_view(&d.publication.scope))
            .unwrap();
        view.visible_publication_ids.push(p);
        if n == 63 {
            assert_eq!(
                db.structured_entity_hints(&view, &["Rigel"]).unwrap().len(),
                64
            );
        }
    }
    assert!(db.structured_entity_hints(&view, &["Rigel"]).is_err());
}

#[test]
fn structured_entity_hints_bound_matching_and_nonmatching_alias_work() {
    let mut db = MemoryStore::open(":memory:").unwrap();
    db.enable_structured_memory().unwrap();
    let mut base = entity_delta("aliases", "base");
    let id = base.entities[0].entity_id.clone();
    base.aliases = (0..33)
        .map(|n| polaris_memory::structured::StructuredAlias {
            entity_id: id.clone(),
            display_alias: format!("alias{n}"),
            match_alias: format!("alias{n}"),
            relation: "alias".into(),
            source_id: "base".into(),
        })
        .collect();
    seal(&mut base);
    db.insert_structured_delta(&base, &empty_view(&base.publication.scope))
        .unwrap();
    let mut view = empty_view(&base.publication.scope);
    view.visible_publication_ids = vec!["base".into()];
    let query = (0..32)
        .map(|n| format!("alias{n}"))
        .collect::<Vec<_>>()
        .join(" ");
    assert_eq!(
        db.structured_entity_hints(&view, &[&query]).unwrap()[0]
            .matching_aliases
            .len(),
        32
    );
    assert!(matches!(
        db.structured_entity_hints(&view, &[&format!("{query} alias32")]),
        Err(Error::InvalidInput("structured hint alias limit"))
    ));
    for generation in 2..=3 {
        let mut next = entity_delta("aliases", &format!("p{generation}"));
        next.publication.scope.generation = generation;
        next.entities.clear();
        next.entity_revisions.clear();
        next.aliases = (0..1024)
            .map(|n| polaris_memory::structured::StructuredAlias {
                entity_id: id.clone(),
                display_alias: format!("unused{generation}_{n}"),
                match_alias: format!("unused{generation}_{n}"),
                relation: "alias".into(),
                source_id: format!("p{generation}"),
            })
            .collect();
        seal(&mut next);
        db.insert_structured_delta(&next, &view).unwrap();
        view.scope.generation = generation;
        view.visible_publication_ids
            .push(next.publication.publication_id);
    }
    assert!(
        db.structured_entity_hints(&view, &["unknown"])
            .unwrap()
            .is_empty()
    );
    assert!(matches!(
        db.structured_entity_hints(&view, &[&"x".repeat(48 * 1024)]),
        Err(Error::InvalidInput("structured hint comparison limit"))
    ));
}

#[test]
fn existing_identity_publication_requires_exact_grants_in_the_transaction() {
    use polaris_memory::structured::StructuredAncestorPublicationRange;
    let mut db = MemoryStore::open(":memory:").unwrap();
    db.enable_structured_memory().unwrap();
    let parent = entity_delta("parent", "p1");
    db.insert_structured_delta(&parent, &empty_view(&parent.publication.scope))
        .unwrap();
    let mut child = parent.clone();
    child.publication.scope.session_id = "child".into();
    child.publication.publication_id = "c1".into();
    child.entities.clear();
    child.entity_revisions[0].source_id = "child-source".into();
    child.entity_revisions[0].canonical_name = "Aster".into();
    let entity = &child.entity_revisions[0].entity_id;
    child.facts.push(StructuredFactRevision {
        fact_id: stable_fact_id(entity, "endpoint", "", "child-source").unwrap(),
        entity_id: entity.clone(),
        fact_kind: "endpoint".into(),
        value_json: "\"proposed\"".into(),
        unit: "none".into(),
        condition: "".into(),
        status: FactStatus::Proposed,
        reason: "propose".into(),
        successor_fact_id: None,
        source_id: "child-source".into(),
    });
    seal(&mut child);
    let view = StructuredPublicationView {
        ancestors: vec![StructuredAncestorPublicationRange {
            session_id: "parent".into(),
            epoch: 0,
            generation: 1,
            visible_publication_ids: vec!["p1".into()],
        }],
        ..empty_view(&child.publication.scope)
    };
    assert!(
        db.insert_structured_delta(&child, &empty_view(&child.publication.scope))
            .is_err()
    );
    let mut foreign = view.clone();
    foreign.scope.project_id = "foreign".into();
    assert!(db.insert_structured_delta(&child, &foreign).is_err());
    let mut hidden = view.clone();
    hidden.ancestors[0].visible_publication_ids = vec!["not-granted".into()];
    assert!(db.insert_structured_delta(&child, &hidden).is_err());
    db.insert_structured_delta(&child, &view).unwrap();
    db.verify_structured_delta(&child).unwrap();
    let current_view = StructuredPublicationView {
        visible_publication_ids: vec!["c1".into()],
        ..view.clone()
    };
    for field in ["entity", "kind", "condition"] {
        let mut changed = child.clone();
        changed.publication.publication_id = format!("changed-{field}");
        changed.publication.scope.generation = 2;
        changed.facts[0].source_id = "correction-source".into();
        match field {
            "entity" => {
                let extra = entity_delta("child", "extra");
                changed.facts[0].entity_id = extra.entities[0].entity_id.clone();
                changed.entities.extend(extra.entities);
                changed.entity_revisions.extend(extra.entity_revisions);
            }
            "kind" => changed.facts[0].fact_kind = "different".into(),
            _ => changed.facts[0].condition = "different".into(),
        }
        seal(&mut changed);
        assert!(
            db.insert_structured_delta(&changed, &current_view).is_err(),
            "{field} cannot repurpose a visible fact ID"
        );
    }
    // A later, distinct parent identity is not authorized by the earlier grant.
    let mut later = entity_delta("parent", "p2");
    later.publication.scope.generation = 2;
    seal(&mut later);
    db.insert_structured_delta(&later, &empty_view(&later.publication.scope))
        .unwrap();
    let mut denied = child.clone();
    denied.publication.publication_id = "denied".into();
    denied.facts.clear();
    denied.entity_revisions[0].entity_id = later.entities[0].entity_id.clone();
    seal(&mut denied);
    assert!(db.insert_structured_delta(&denied, &view).is_err());
    denied.entity_revisions[0].entity_id = "missing".into();
    seal(&mut denied);
    assert!(db.insert_structured_delta(&denied, &view).is_err());
    db.delete_session("p", "parent").unwrap();
    assert!(db.insert_structured_delta(&child, &view).is_err());
}

#[test]
fn canonical_hash_and_row_readback_reject_replay_tampering() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.sqlite");
    let mut db = MemoryStore::open(&path).unwrap();
    db.enable_structured_memory().unwrap();
    let original = entity_delta("session", "publication");
    let view = empty_view(&original.publication.scope);
    db.insert_structured_delta(&original, &view).unwrap();
    db.insert_structured_delta(&original, &view).unwrap();
    let mut changed = original.clone();
    changed.entity_revisions[0].canonical_name = "Forged".into();
    assert!(db.insert_structured_delta(&changed, &view).is_err());
    seal(&mut changed);
    assert!(db.insert_structured_delta(&changed, &view).is_err());
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE conversation_entity_revisions_v3 SET canonical_name='Forged'",
        [],
    )
    .unwrap();
    assert!(db.verify_structured_delta(&original).is_err());
    assert!(db.insert_structured_delta(&original, &view).is_err());
    conn.execute("DELETE FROM conversation_entity_revisions_v3", [])
        .unwrap();
    assert!(db.insert_structured_delta(&original, &view).is_err());
    let rows: i64 = conn
        .query_row(
            "SELECT count(*) FROM conversation_entity_revisions_v3",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(rows, 0, "replay must not silently repair missing rows");
}

#[test]
fn sqlite_commit_precedes_marker_and_commit_failure_never_calls_marker() {
    use polaris_memory::conversation::{
        EmbeddingMetadata, PendingSummary, PublishedView, Scope, SourceMetadata,
    };
    use sha2::{Digest, Sha256};
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("memory.sqlite");
    let mut db = MemoryStore::open(&path).unwrap();
    db.enable_structured_memory().unwrap();
    let delta = entity_delta("s", "p1");
    let scope = Scope {
        project_id: "p".into(),
        session_id: "s".into(),
        epoch: 0,
        generation: 1,
    };
    let summary = r#"{"facts":["fixture"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[1]}"#.to_string();
    let pending = PendingSummary {
        id: "summary".into(),
        scope: scope.clone(),
        source: SourceMetadata {
            id: "source".into(),
            start_turn: 1,
            end_turn: 1,
            raw_hash: HASH.into(),
        },
        summary_hash: format!("{:x}", Sha256::digest(summary.as_bytes())),
        summary,
        model: "gpt-6-astra".into(),
        effort: "medium".into(),
        prompt_version: "v1".into(),
        embedding: EmbeddingMetadata {
            model: "e5".into(),
            revision: "r1".into(),
            dimension: 2,
            input_hash: HASH.into(),
            values: vec![1., 0.],
        },
    };
    db.insert_pending_summary(&pending).unwrap();
    let view = PublishedView {
        scope,
        visible_ids: vec![pending.id.clone()],
        ancestors: vec![],
    };
    let grants = empty_view(&delta.publication.scope);
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute_batch("CREATE TABLE failure_parent(id INTEGER PRIMARY KEY); CREATE TABLE failure_child(id INTEGER REFERENCES failure_parent(id) DEFERRABLE INITIALLY DEFERRED); CREATE TRIGGER inject_commit_failure AFTER INSERT ON conversation_publications_v3 BEGIN INSERT INTO failure_child VALUES(7); END;").unwrap();
    let mut called = false;
    let result = db.publish_pending_with_structured(
        &view,
        std::slice::from_ref(&pending.id),
        &delta,
        &grants,
        &[],
        || {
            called = true;
            Ok(())
        },
    );
    assert!(result.is_err());
    assert!(!called, "marker cannot precede a failed SQLite commit");
    assert!(
        db.structured_publication(&delta.publication.scope, "p1")
            .unwrap()
            .is_none()
    );
    conn.execute_batch("DROP TRIGGER inject_commit_failure;")
        .unwrap();
    let result = db.publish_pending_with_structured(
        &view,
        std::slice::from_ref(&pending.id),
        &delta,
        &grants,
        &[],
        || {
            let rows: i64 = conn
                .query_row(
                    "SELECT count(*) FROM conversation_entity_revisions_v3",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(
                rows, 1,
                "a separate connection sees committed rows before marker"
            );
            Err(Error::InvalidInput("marker fixture failure"))
        },
    );
    assert!(result.is_err());
    db.verify_structured_delta(&delta).unwrap();
    db.publish_pending_with_structured(
        &view,
        std::slice::from_ref(&pending.id),
        &delta,
        &grants,
        &[],
        || Ok(()),
    )
    .unwrap();
    let mut next_delta = entity_delta("s", "p2");
    next_delta.publication.scope.generation = 2;
    seal(&mut next_delta);
    let mut next_pending = pending.clone();
    next_pending.id = "summary2".into();
    next_pending.scope.generation = 2;
    next_pending.source.id = "source2".into();
    db.insert_pending_summary(&next_pending).unwrap();
    let mut next_view = view.clone();
    next_view.scope.generation = 2;
    next_view.visible_ids.push(next_pending.id.clone());
    let prior_grants = StructuredPublicationView {
        visible_publication_ids: vec!["p1".into()],
        ..grants.clone()
    };
    conn.execute("UPDATE conversation_entity_revisions_v3 SET description='tampered' WHERE publication_id='p1'",[]).unwrap();
    let mut published = false;
    let result = db.publish_pending_with_structured(
        &next_view,
        std::slice::from_ref(&next_pending.id),
        &next_delta,
        &prior_grants,
        std::slice::from_ref(&delta),
        || {
            published = true;
            Ok(())
        },
    );
    assert!(result.is_err());
    assert!(!published);
    assert!(
        db.structured_publication(&next_delta.publication.scope, "p2")
            .unwrap()
            .is_none()
    );
}

fn record() -> Record {
    Record {
        project_id: "project".into(),
        session_id: "session".into(),
        id: "v2".into(),
        role: "user".into(),
        timestamp: "2026-09-13T00:00:00Z".into(),
        source: "synthetic".into(),
        text: "legacy record".into(),
    }
}

#[test]
fn publication_rows_resolve_current_overlay_and_frozen_dependents() {
    let mut memory = MemoryStore::open(":memory:").unwrap();
    memory.enable_structured_memory().unwrap();
    let origin = OriginScope {
        project_id: "p".into(),
        session_id: "child".into(),
        epoch: 0,
    };
    let entity = stable_entity_id(&origin, "source", 0, EntityKind::Product).unwrap();
    let scope = PublicationScope {
        project_id: "p".into(),
        session_id: "child".into(),
        epoch: 0,
        generation: 1,
    };
    let fact = stable_fact_id(&entity, "endpoint", "", "source").unwrap();
    let mut delta = StructuredDelta {
        publication: StructuredPublication {
            scope: scope.clone(),
            publication_id: "child-pub".into(),
            raw_hash: HASH.into(),
            delta_hash: HASH.into(),
            prompt_version: "v3".into(),
        },
        entities: vec![StructuredEntity {
            entity_id: entity.clone(),
            origin: origin.clone(),
            origin_source_id: "source".into(),
            origin_ordinal: 0,
            kind: EntityKind::Product,
        }],
        entity_revisions: vec![StructuredEntityRevision {
            entity_id: entity.clone(),
            source_id: "source".into(),
            canonical_name: "Aster".into(),
            description: "d".into(),
            subject_project: "p".into(),
        }],
        aliases: vec![],
        facts: vec![StructuredFactRevision {
            fact_id: fact.clone(),
            entity_id: entity.clone(),
            fact_kind: "endpoint".into(),
            value_json: "\"x\"".into(),
            unit: "none".into(),
            condition: "".into(),
            status: FactStatus::Active,
            reason: "assert".into(),
            successor_fact_id: None,
            source_id: "source".into(),
        }],
        dependencies: vec![StructuredFactDependency {
            dependent_fact_id: stable_fact_id(&entity, "plan", "", "plan-source").unwrap(),
            dependency_fact_id: fact.clone(),
            source_id: "source".into(),
        }],
        relations: vec![],
    };
    let dependent = delta.dependencies[0].dependent_fact_id.clone();
    let mut plan = delta.facts[0].clone();
    plan.fact_id = dependent.clone();
    plan.fact_kind = "plan".into();
    plan.source_id = "plan-source".into();
    delta.facts.push(plan);
    delta.publication.delta_hash = polaris_memory::structured::delta_hash(&delta).unwrap();
    memory
        .insert_structured_delta(&delta, &empty_view(&scope))
        .unwrap();
    let view = StructuredPublicationView {
        scope: scope.clone(),
        visible_publication_ids: vec!["child-pub".into()],
        ancestors: vec![],
    };
    assert_eq!(
        memory
            .resolve_structured_fact(&view, &entity, "endpoint", "")
            .unwrap()
            .unwrap()
            .fact_id,
        fact
    );
    assert_eq!(
        memory.structured_dependents(&view, &fact).unwrap(),
        vec![dependent]
    );
}

#[test]
fn entity_lookup_honors_alias_grants_ambiguity_and_tombstones() {
    let mut memory = MemoryStore::open(":memory:").unwrap();
    memory.enable_structured_memory().unwrap();
    let origin = OriginScope {
        project_id: "p".into(),
        session_id: "parent".into(),
        epoch: 0,
    };
    let put = |memory: &mut MemoryStore,
               session: &str,
               pubid: &str,
               name: &str,
               alias: &str,
               ordinal: u32| {
        let id = stable_entity_id(
            &OriginScope {
                session_id: session.into(),
                ..origin.clone()
            },
            "s",
            ordinal,
            EntityKind::Product,
        )
        .unwrap();
        let scope = PublicationScope {
            project_id: "p".into(),
            session_id: session.into(),
            epoch: 0,
            generation: 1,
        };
        let mut delta = StructuredDelta {
            publication: StructuredPublication {
                scope: scope.clone(),
                publication_id: pubid.into(),
                raw_hash: HASH.into(),
                delta_hash: format!("{:064x}", ordinal + 1),
                prompt_version: "v".into(),
            },
            entities: vec![StructuredEntity {
                entity_id: id.clone(),
                origin: OriginScope {
                    session_id: session.into(),
                    ..origin.clone()
                },
                origin_source_id: "s".into(),
                origin_ordinal: ordinal,
                kind: EntityKind::Product,
            }],
            entity_revisions: vec![StructuredEntityRevision {
                entity_id: id.clone(),
                source_id: "s".into(),
                canonical_name: name.into(),
                description: "d".into(),
                subject_project: "p".into(),
            }],
            aliases: vec![polaris_memory::structured::StructuredAlias {
                entity_id: id.clone(),
                display_alias: alias.into(),
                match_alias: alias.into(),
                relation: "alias".into(),
                source_id: "s".into(),
            }],
            facts: vec![],
            dependencies: vec![],
            relations: vec![],
        };
        delta.publication.delta_hash = polaris_memory::structured::delta_hash(&delta).unwrap();
        memory
            .insert_structured_delta(&delta, &empty_view(&scope))
            .unwrap();
        id
    };
    let current = put(&mut memory, "child", "c", "Aster", "Rigel", 1);
    let parent = put(&mut memory, "parent", "p1", "Nova", "Old", 2);
    let other = put(&mut memory, "other", "o", "Other", "Rigel", 3);
    let view = StructuredPublicationView {
        scope: PublicationScope {
            project_id: "p".into(),
            session_id: "child".into(),
            epoch: 0,
            generation: 1,
        },
        visible_publication_ids: vec!["c".into()],
        ancestors: vec![
            polaris_memory::structured::StructuredAncestorPublicationRange {
                session_id: "parent".into(),
                epoch: 0,
                generation: 1,
                visible_publication_ids: vec!["p1".into()],
            },
        ],
    };
    assert_eq!(
        memory
            .resolve_structured_entity(&view, EntityKind::Product, "Rigel", "p")
            .unwrap(),
        Some(current)
    );
    assert_eq!(
        memory
            .resolve_structured_entity(&view, EntityKind::Product, "Old", "p")
            .unwrap(),
        Some(parent)
    );
    let ambiguous = StructuredPublicationView {
        ancestors: vec![
            polaris_memory::structured::StructuredAncestorPublicationRange {
                session_id: "other".into(),
                epoch: 0,
                generation: 1,
                visible_publication_ids: vec!["o".into()],
            },
        ],
        ..view.clone()
    };
    assert!(
        memory
            .resolve_structured_entity(&ambiguous, EntityKind::Product, "Rigel", "p")
            .is_err()
    );
    memory.delete_session("p", "parent").unwrap();
    assert_eq!(
        memory
            .resolve_structured_entity(&view, EntityKind::Product, "Old", "p")
            .unwrap(),
        None
    );
    let hidden = StructuredPublicationView {
        visible_publication_ids: vec![],
        ancestors: vec![],
        ..view
    };
    assert_eq!(
        memory
            .resolve_structured_entity(&hidden, EntityKind::Product, "Other", "p")
            .unwrap(),
        None
    );
    let _ = other;
}

#[test]
fn legacy_open_leaves_v3_absent_and_opt_in_migration_keeps_v2_rows() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.sqlite3");
    let mut memory = MemoryStore::open(&database).unwrap();
    memory.upsert(&record(), None).unwrap();
    assert_eq!(memory.structured_memory_schema_version().unwrap(), None);
    let connection = rusqlite::Connection::open(&database).unwrap();
    let before: i64 = connection
        .query_row("SELECT count(*) FROM memory_records", [], |row| row.get(0))
        .unwrap();
    let v3_before: i64 = connection.query_row("SELECT count(*) FROM sqlite_master WHERE type='table' AND name='conversation_publications_v3'", [], |row| row.get(0)).unwrap();
    connection.execute(
        "INSERT INTO conversation_sources_v2 VALUES ('project','session',0,1,'source-v2',1,1,?1)",
        [HASH],
    ).unwrap();
    connection.execute(
        "INSERT INTO conversation_summaries_v2 VALUES ('project','session',0,1,'summary-v2','source-v2','{}',?1,'model','effort','v2')",
        [HASH],
    ).unwrap();
    connection.execute(
        "INSERT INTO conversation_vectors_v2 VALUES ('project','session',0,1,'summary-v2','model','r1',1,?1,?2)",
        rusqlite::params![HASH, vec![0_u8; 4]],
    ).unwrap();
    let v2_before: Vec<_> = [
        "conversation_sources_v2",
        "conversation_summaries_v2",
        "conversation_vectors_v2",
    ]
    .into_iter()
    .map(|table| {
        connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    })
    .collect();
    assert_eq!((before, v3_before), (1, 0));
    drop(connection);
    memory.enable_structured_memory().unwrap();
    assert_eq!(memory.structured_memory_schema_version().unwrap(), Some(1));
    assert_eq!(
        memory.get("project", "session", "v2").unwrap(),
        Some(record())
    );
    let connection = rusqlite::Connection::open(&database).unwrap();
    let v2_after: Vec<_> = [
        "conversation_sources_v2",
        "conversation_summaries_v2",
        "conversation_vectors_v2",
    ]
    .into_iter()
    .map(|table| {
        connection
            .query_row(
                "SELECT sql FROM sqlite_master WHERE type='table' AND name=?1",
                [table],
                |row| row.get::<_, String>(0),
            )
            .unwrap()
    })
    .collect();
    assert_eq!(v2_after, v2_before);
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM conversation_sources_v2", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        1
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM conversation_summaries_v2",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        1
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM conversation_vectors_v2", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap(),
        1
    );
}

#[test]
fn schema_readback_rejects_missing_index_and_wrong_constraints_without_v2_loss() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.sqlite3");
    let mut memory = MemoryStore::open(&database).unwrap();
    memory.upsert(&record(), None).unwrap();
    memory.enable_structured_memory().unwrap();
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection.execute_batch("DROP INDEX conversation_entities_v3_origin; CREATE UNIQUE INDEX conversation_entities_v3_origin ON conversation_entities_v3(origin_project_id, origin_session_id, origin_epoch);").unwrap();
    drop(connection);
    assert!(matches!(
        memory.enable_structured_memory(),
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(
        memory.get("project", "session", "v2").unwrap(),
        Some(record())
    );

    let other = tempfile::tempdir().unwrap();
    let other_database = other.path().join("memory.sqlite3");
    let mut malformed = MemoryStore::open(&other_database).unwrap();
    malformed.upsert(&record(), None).unwrap();
    malformed.enable_structured_memory().unwrap();
    let connection = rusqlite::Connection::open(&other_database).unwrap();
    connection
        .execute_batch(
            "PRAGMA foreign_keys=OFF;
         DROP TABLE conversation_entities_v3;
         CREATE TABLE conversation_entities_v3 (
           entity_id TEXT PRIMARY KEY NOT NULL, origin_project_id TEXT NOT NULL,
           origin_session_id TEXT NOT NULL, origin_epoch INTEGER NOT NULL,
           origin_source_id TEXT NOT NULL, origin_ordinal TEXT NOT NULL, kind TEXT NOT NULL
         );",
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        malformed.enable_structured_memory(),
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(
        malformed.get("project", "session", "v2").unwrap(),
        Some(record())
    );
}

#[test]
fn conflicting_or_version_mismatched_migration_rolls_back_without_touching_v2() {
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.sqlite3");
    let mut memory = MemoryStore::open(&database).unwrap();
    memory.upsert(&record(), None).unwrap();
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch("CREATE TABLE conversation_entities_v3 (bad INTEGER);")
        .unwrap();
    drop(connection);
    assert!(matches!(
        memory.enable_structured_memory(),
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(
        memory.get("project", "session", "v2").unwrap(),
        Some(record())
    );
    assert_eq!(memory.structured_memory_schema_version().unwrap(), None);

    let second = tempfile::tempdir().unwrap();
    let second_database = second.path().join("memory.sqlite3");
    let mut mismatched = MemoryStore::open(&second_database).unwrap();
    mismatched.enable_structured_memory().unwrap();
    let connection = rusqlite::Connection::open(&second_database).unwrap();
    connection
        .execute(
            "UPDATE memory_schema_meta SET schema_version=99 WHERE component='structured_memory'",
            [],
        )
        .unwrap();
    drop(connection);
    assert!(matches!(
        mismatched.enable_structured_memory(),
        Err(Error::InvalidInput(_))
    ));
    assert_eq!(
        mismatched.structured_memory_schema_version().unwrap(),
        Some(99)
    );
}

#[test]
fn stable_identity_and_fact_identity_do_not_depend_on_names_or_generation() {
    let origin = OriginScope {
        project_id: "p".into(),
        session_id: "s".into(),
        epoch: 2,
    };
    let aster = stable_entity_id(&origin, "source-7", 1, EntityKind::Product).unwrap();
    let after_rename = stable_entity_id(&origin, "source-7", 1, EntityKind::Product).unwrap();
    let distinct = stable_entity_id(&origin, "source-7", 1, EntityKind::ConfigurationKey).unwrap();
    let other_project = stable_entity_id(
        &OriginScope {
            project_id: "other".into(),
            ..origin.clone()
        },
        "source-7",
        1,
        EntityKind::Product,
    )
    .unwrap();
    assert_eq!(aster, after_rename);
    assert_ne!(aster, distinct);
    assert_ne!(aster, other_project);
    let unconditional = stable_fact_id(&aster, "endpoint", "", "source-7").unwrap();
    assert_eq!(
        unconditional,
        stable_fact_id(&aster, "endpoint", "", "source-7").unwrap()
    );
    assert_ne!(
        unconditional,
        stable_fact_id(&aster, "endpoint", "production", "source-7").unwrap()
    );
    assert!(stable_fact_id(&aster, "endpoint", &"x".repeat(1025), "source-7").is_err());
}

#[test]
fn only_forward_state_transitions_and_complete_host_provenance_are_accepted() {
    assert!(validate_fact_transition(None, FactStatus::Active, FactRelation::Assert).is_ok());
    assert!(validate_fact_transition(None, FactStatus::Proposed, FactRelation::Propose).is_ok());
    assert!(
        validate_fact_transition(
            Some(FactStatus::Active),
            FactStatus::Superseded,
            FactRelation::Withdraw
        )
        .is_ok()
    );
    assert!(
        validate_fact_transition(
            Some(FactStatus::Active),
            FactStatus::NeedsReconfirmation,
            FactRelation::DependsOn
        )
        .is_ok()
    );
    assert!(
        validate_fact_transition(
            Some(FactStatus::Superseded),
            FactStatus::Active,
            FactRelation::Assert
        )
        .is_err()
    );
    assert!(
        validate_fact_transition(
            Some(FactStatus::Proposed),
            FactStatus::Active,
            FactRelation::Assert
        )
        .is_err()
    );
    let publication = StructuredPublication {
        scope: PublicationScope {
            project_id: "p".into(),
            session_id: "s".into(),
            epoch: 0,
            generation: 1,
        },
        publication_id: "publication".into(),
        raw_hash: HASH.into(),
        delta_hash: HASH.into(),
        prompt_version: "v3".into(),
    };
    assert!(validate_publication(&publication).is_ok());
    let mut forged = publication;
    forged.raw_hash = "not-a-host-hash".into();
    assert!(validate_publication(&forged).is_err());
}
