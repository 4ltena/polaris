//! Frozen source authority for the core owner's raw snapshot reader.

use super::*;

impl MemoryStore {
    /// The callback runs inside the same read transaction as the publication
    /// and tombstone checks. It receives metadata, never a caller-selected path.
    /// The owner must still verify raw bytes against this publication's hash.
    pub fn with_structured_source<T>(
        &self,
        view: &StructuredPublicationView,
        publication_id: &str,
        source_id: &str,
        read: impl FnOnce(StructuredPublication) -> Result<T>,
    ) -> Result<T> {
        validate_structured_view(view)?;
        validate_id(publication_id)?;
        validate_id(source_id)?;
        let transaction = self.structured_read_transaction()?;
        let tx = &self.connection;
        let mut found = None;
        for (scope, ids) in publication_scopes(view) {
            if structured_scope_forgotten(tx, &scope)? {
                return Err(Error::InvalidInput("structured source scope forgotten"));
            }
            if !ids.iter().any(|id| id == publication_id) {
                continue;
            }
            let prefix = format!(
                "source-v3:{}:{}:{}:",
                scope.project_id, scope.session_id, scope.epoch
            );
            let Some(sequence) = source_id
                .strip_prefix(&prefix)
                .and_then(|s| s.parse::<u64>().ok())
            else {
                continue;
            };
            if sequence == 0 || source_id != format!("{prefix}{sequence}") {
                continue;
            }
            let publication = tx.query_row(
                "SELECT generation,raw_hash,delta_hash,prompt_version FROM conversation_publications_v3 WHERE project_id=?1 AND session_id=?2 AND epoch=?3 AND generation<=?4 AND publication_id=?5",
                params![scope.project_id,scope.session_id,scope.epoch,scope.generation,publication_id],
                |r| Ok(StructuredPublication { scope: PublicationScope { generation: r.get(0)?, ..scope.clone() }, publication_id: publication_id.into(), raw_hash: r.get(1)?, delta_hash: r.get(2)?, prompt_version: r.get(3)? }),
            ).optional()?.ok_or(Error::InvalidInput("structured source publication absent"))?;
            validate_publication(&publication)?;
            let mut source_present = false;
            // Only a source actually referenced by rows in this exact
            // publication may be read; a valid raw sequence alone is no grant.
            for table in [
                "conversation_entity_revisions_v3",
                "conversation_aliases_v3",
                "conversation_fact_revisions_v3",
                "conversation_fact_dependencies_v3",
                "conversation_entity_relations_v3",
            ] {
                source_present |= tx.query_row(
                    &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE project_id=?1 AND session_id=?2 AND epoch=?3 AND generation=?4 AND publication_id=?5 AND source_id=?6)"),
                    params![scope.project_id,scope.session_id,scope.epoch,publication.scope.generation,publication_id,source_id],
                    |r| r.get::<_,bool>(0),
                )?;
            }
            if !source_present {
                return Err(Error::InvalidInput("structured source not referenced"));
            }
            if found.replace(publication).is_some() {
                return Err(Error::InvalidInput("structured source ambiguous"));
            }
        }
        let value = read(found.ok_or(Error::InvalidInput("structured source outside grants"))?)?;
        if let Some(transaction) = transaction {
            transaction.commit()?;
        }
        Ok(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    fn empty_receipt(scope: &PublicationScope, id: &str) -> StructuredDelta {
        let mut delta = StructuredDelta {
            publication: StructuredPublication {
                scope: scope.clone(),
                publication_id: id.into(),
                raw_hash: "a".repeat(64),
                delta_hash: "b".repeat(64),
                prompt_version: "fixture".into(),
            },
            entities: vec![],
            entity_revisions: vec![],
            aliases: vec![],
            facts: vec![],
            dependencies: vec![],
            relations: vec![],
        };
        delta.publication.delta_hash = delta_hash(&delta).unwrap();
        delta
    }

    #[test]
    fn source_reader_shares_receipt_grant_capacity() {
        let root = tempfile::tempdir().unwrap();
        let mut db = MemoryStore::open(root.path().join("memory.sqlite3")).unwrap();
        db.enable_structured_memory().unwrap();
        let scope = PublicationScope {
            project_id: "p".into(),
            session_id: "s".into(),
            epoch: 0,
            generation: 1,
        };
        let origin = OriginScope {
            project_id: "p".into(),
            session_id: "s".into(),
            epoch: 0,
        };
        let source = "source-v3:p:s:0:1";
        let entity = stable_entity_id(&origin, source, 0, EntityKind::Product).unwrap();
        let mut delta = empty_receipt(&scope, "target");
        delta.entities.push(StructuredEntity {
            entity_id: entity.clone(),
            origin,
            origin_source_id: source.into(),
            origin_ordinal: 0,
            kind: EntityKind::Product,
        });
        delta.entity_revisions.push(StructuredEntityRevision {
            entity_id: entity,
            source_id: source.into(),
            canonical_name: "Aster".into(),
            description: "product".into(),
            subject_project: "p".into(),
        });
        delta.publication.delta_hash = delta_hash(&delta).unwrap();
        let mut view = StructuredPublicationView {
            scope,
            visible_publication_ids: vec![],
            ancestors: vec![],
        };
        db.insert_structured_delta(&delta, &view).unwrap();
        // Only the selected publication needs index rows for this lower-level
        // source-capacity fixture. Full receipt verification is tested separately.
        view.visible_publication_ids = (0..65_535).map(|n| format!("unused-{n}")).collect();
        view.visible_publication_ids.push("target".into());
        db.with_structured_source(&view, "target", source, |publication| {
            assert_eq!(publication, delta.publication);
            Ok(())
        })
        .unwrap();
        view.visible_publication_ids.push("overflow".into());
        let called = Cell::new(false);
        let error = db
            .with_structured_source(&view, "target", source, |_| {
                called.set(true);
                Ok(())
            })
            .unwrap_err();
        assert!(error.to_string().contains("receipt grant limit"), "{error}");
        assert!(!called.get());
    }

    #[test]
    fn receipt_grants_bound_aggregate_count_bytes_and_ancestors_before_callbacks() {
        let root = tempfile::tempdir().unwrap();
        let mut db = MemoryStore::open(root.path().join("memory.sqlite3")).unwrap();
        db.enable_structured_memory().unwrap();
        let scope = PublicationScope {
            project_id: "p".into(),
            session_id: "s".into(),
            epoch: 0,
            generation: 1,
        };
        let mut view = StructuredPublicationView {
            scope: scope.clone(),
            visible_publication_ids: vec![],
            ancestors: vec![StructuredAncestorPublicationRange {
                session_id: "parent".into(),
                epoch: 0,
                generation: 1,
                visible_publication_ids: (0..65_535).map(|n| format!("{n:064}")).collect(),
            }],
        };
        // These are metadata-capacity fixtures, not seeded index publications.
        let last = "z".repeat(64);
        view.check_receipt_capacity(Some(&last)).unwrap();
        view.visible_publication_ids.push(last);
        view.check_receipt_capacity(None).unwrap();
        let assert_refused = |view: &StructuredPublicationView, reason: &str| {
            let reads = Cell::new(0);
            let finished = Cell::new(false);
            let error = db
                .with_verified_structured_receipts(
                    view,
                    &mut |scope, id| {
                        reads.set(reads.get() + 1);
                        Ok(empty_receipt(scope, id))
                    },
                    || {
                        finished.set(true);
                        Ok(())
                    },
                )
                .unwrap_err();
            assert!(error.to_string().contains(reason), "{error}");
            assert_eq!(reads.get(), 0);
            assert!(!finished.get());
        };
        let mut too_many = view.clone();
        too_many.visible_publication_ids.push("extra".into());
        assert_refused(&too_many, "receipt grant limit");
        let mut too_large = view.clone();
        too_large.visible_publication_ids[0].push('z');
        assert_refused(&too_large, "receipt grant byte limit");
        let mut too_deep = StructuredPublicationView {
            scope: scope.clone(),
            visible_publication_ids: vec![],
            ancestors: vec![],
        };
        too_deep.ancestors = (0..1_024)
            .map(|n| StructuredAncestorPublicationRange {
                session_id: format!("parent-{n}"),
                epoch: 0,
                generation: 1,
                visible_publication_ids: vec![],
            })
            .collect();
        too_deep.check_receipt_capacity(None).unwrap();
        too_deep.ancestors.push(too_deep.ancestors[0].clone());
        assert_refused(&too_deep, "receipt ancestor limit");

        let published = crate::conversation::PublishedView {
            scope: crate::conversation::Scope {
                project_id: "p".into(),
                session_id: "s".into(),
                epoch: 0,
                generation: 2,
            },
            visible_ids: vec![],
            ancestors: vec![],
        };
        let reads = Cell::new(0);
        let marker = Cell::new(false);
        let delta = empty_receipt(
            &PublicationScope {
                generation: 2,
                ..scope
            },
            "next",
        );
        let error = db
            .publish_pending_with_structured_reader(
                &published,
                &[],
                &delta,
                &view,
                &mut |scope, id| {
                    reads.set(reads.get() + 1);
                    Ok(empty_receipt(scope, id))
                },
                || {
                    marker.set(true);
                    Ok(())
                },
            )
            .unwrap_err();
        assert!(error.to_string().contains("receipt grant limit"));
        assert_eq!(reads.get(), 0);
        assert!(!marker.get());
        assert_eq!(
            db.connection
                .query_row(
                    "SELECT count(*) FROM conversation_publications_v3",
                    [],
                    |r| r.get::<_, i64>(0)
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn receipt_reader_crosses_16384_without_collecting_deltas_and_keeps_one_snapshot() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("memory.sqlite3");
        let mut db = MemoryStore::open(&path).unwrap();
        db.enable_structured_memory().unwrap();
        db.connection
            .pragma_update(None, "journal_mode", "WAL")
            .unwrap();
        let scope = PublicationScope {
            project_id: "p".into(),
            session_id: "s".into(),
            epoch: 0,
            generation: 1,
        };
        let ids: Vec<_> = (0..16_385).map(|n| format!("receipt-{n:05}")).collect();
        // Seed empty index publications in one fixture transaction. The reader
        // generates one matching receipt per call; no Vec<StructuredDelta> exists.
        let tx = db.connection.transaction().unwrap();
        {
            let mut insert = tx.prepare("INSERT INTO conversation_publications_v3(project_id,session_id,epoch,generation,publication_id,raw_hash,delta_hash,prompt_version) VALUES ('p','s',0,1,?1,?2,?3,?4)").unwrap();
            for id in &ids {
                let delta = empty_receipt(&scope, id);
                let p = &delta.publication;
                insert
                    .execute(rusqlite::params![
                        id,
                        p.raw_hash,
                        p.delta_hash,
                        p.prompt_version
                    ])
                    .unwrap();
            }
        }
        tx.commit().unwrap();
        let mut view = StructuredPublicationView {
            scope,
            visible_publication_ids: ids[..16_384].to_vec(),
            ancestors: vec![],
        };
        let mut count = 0;
        db.with_verified_structured_receipts(
            &view,
            &mut |scope, id| {
                assert_eq!(id, ids[count]);
                count += 1;
                Ok(empty_receipt(scope, id))
            },
            || Ok(()),
        )
        .unwrap();
        assert_eq!(count, 16_384);
        view.visible_publication_ids = ids.clone();
        count = 0;
        db.with_verified_structured_receipts(&view, &mut |scope, id| {
            assert_eq!(id, ids[count]); count += 1;
            if count == 1 {
                // The final row changes after the reader's DB snapshot is fixed.
                rusqlite::Connection::open(&path)?.execute("UPDATE conversation_publications_v3 SET raw_hash=?1 WHERE publication_id=?2", rusqlite::params!["c".repeat(64), ids.last().unwrap()])?;
            }
            Ok(empty_receipt(scope, id))
        }, || Ok(())).unwrap();
        assert_eq!(count, 16_385);
        let called = Cell::new(false);
        assert!(
            db.with_verified_structured_receipts(
                &view,
                &mut |scope, id| Ok(empty_receipt(scope, id)),
                || {
                    called.set(true);
                    Ok(())
                }
            )
            .is_err()
        );
        assert!(!called.get());
        db.delete_session("p", "s").unwrap();
        let read_count = Cell::new(0);
        assert!(matches!(
            db.with_verified_structured_receipts(
                &view,
                &mut |scope, id| {
                    read_count.set(read_count.get() + 1);
                    Ok(empty_receipt(scope, id))
                },
                || Ok(())
            ),
            Err(Error::SessionForgotten)
        ));
        assert_eq!(read_count.get(), 0);
    }

    #[test]
    fn verified_read_snapshot_rejects_modified_rows_and_keeps_nested_reads_consistent() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("memory.sqlite3");
        let mut db = MemoryStore::open(&path).unwrap();
        db.enable_structured_memory().unwrap();
        // Exercise concurrent committed writes without the rollback-journal
        // writer waiting on our reader. This setting is fixture-local only.
        db.connection
            .pragma_update(None, "journal_mode", "WAL")
            .unwrap();
        let source = "source-v3:p:s:0:1";
        let origin = OriginScope {
            project_id: "p".into(),
            session_id: "s".into(),
            epoch: 0,
        };
        let entity = stable_entity_id(&origin, source, 0, EntityKind::Product).unwrap();
        let fact = stable_fact_id(&entity, "retry_limit", "production", source).unwrap();
        let mut delta = StructuredDelta {
            publication: StructuredPublication {
                scope: PublicationScope {
                    project_id: "p".into(),
                    session_id: "s".into(),
                    epoch: 0,
                    generation: 1,
                },
                publication_id: "pub".into(),
                raw_hash: "a".repeat(64),
                delta_hash: "b".repeat(64),
                prompt_version: "fixture".into(),
            },
            entities: vec![StructuredEntity {
                entity_id: entity.clone(),
                origin,
                origin_source_id: source.into(),
                origin_ordinal: 0,
                kind: EntityKind::Product,
            }],
            entity_revisions: vec![StructuredEntityRevision {
                entity_id: entity.clone(),
                canonical_name: "Rigel".into(),
                description: "fixture".into(),
                subject_project: "p".into(),
                source_id: source.into(),
            }],
            aliases: vec![],
            dependencies: vec![],
            relations: vec![],
            facts: vec![StructuredFactRevision {
                fact_id: fact.clone(),
                entity_id: entity.clone(),
                fact_kind: "retry_limit".into(),
                value_json: "3".into(),
                unit: "retries".into(),
                condition: "production".into(),
                status: FactStatus::Active,
                reason: "assert".into(),
                successor_fact_id: None,
                source_id: source.into(),
            }],
        };
        delta.publication.delta_hash = delta_hash(&delta).unwrap();
        let mut view = StructuredPublicationView {
            scope: delta.publication.scope.clone(),
            visible_publication_ids: vec![],
            ancestors: vec![],
        };
        db.insert_structured_delta(&delta, &view).unwrap();
        view.visible_publication_ids.push("pub".into());
        db.with_verified_structured_deltas(&[], || {
            db.with_verified_structured_deltas(&[&delta], || {
                assert_eq!(db.structured_entity_hints(&view, &["Rigel"])?.len(), 1);
                // A concurrent connection changes a row, retaining the authentic
                // publication header, source ID and all grants.
                let other = rusqlite::Connection::open(&path)?;
                other.execute(
                    "UPDATE conversation_fact_revisions_v3 SET value_json='99'",
                    [],
                )?;
                let retrieved =
                    db.structured_retrieval_evidence(&view, std::slice::from_ref(&entity))?;
                assert_eq!(retrieved.facts[0].row.value_json, "3");
                assert_eq!(
                    db.structured_fact_evidence(&view, &fact)?
                        .unwrap()
                        .fact
                        .value_json,
                    "3"
                );
                db.with_structured_source(&view, "pub", source, |_| {
                    db.with_verified_structured_deltas(&[&delta], || {
                        db.verify_structured_delta(&delta)
                    })
                })?;
                Ok(())
            })?;
            assert!(
                !db.connection.is_autocommit(),
                "inner verification must not close the outer snapshot"
            );
            db.verify_structured_delta(&delta)
        })
        .unwrap();
        assert_eq!(
            db.structured_publication(&view.scope, "pub").unwrap(),
            Some(delta.publication.clone())
        );
        assert_eq!(
            db.structured_retrieval_evidence(&view, &[entity])
                .unwrap()
                .facts[0]
                .row
                .value_json,
            "99"
        );
        let called = Cell::new(false);
        assert!(
            db.with_verified_structured_deltas(&[&delta], || {
                called.set(true);
                Ok(())
            })
            .is_err()
        );
        assert!(!called.get());
        assert!(
            db.with_structured_source(&view, "pub", source, |_| db.verify_structured_delta(&delta))
                .is_err()
        );
    }
}
