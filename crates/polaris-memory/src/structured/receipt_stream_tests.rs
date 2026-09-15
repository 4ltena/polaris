//! Receipt completeness and snapshot boundaries for an owner-driven scan.

use super::*;
use std::cell::Cell;

fn receipt(scope: &PublicationScope, id: &str) -> StructuredDelta {
    let mut delta = StructuredDelta {
        publication: StructuredPublication {
            scope: PublicationScope {
                generation: 1,
                ..scope.clone()
            },
            publication_id: id.into(),
            raw_hash: "a".repeat(64),
            delta_hash: String::new(),
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

fn fixture() -> (tempfile::TempDir, MemoryStore, StructuredPublicationView) {
    let root = tempfile::tempdir().unwrap();
    let mut db = MemoryStore::open(root.path().join("memory.sqlite3")).unwrap();
    db.enable_structured_memory().unwrap();
    let view = StructuredPublicationView {
        scope: PublicationScope {
            project_id: "p".into(),
            session_id: "child".into(),
            epoch: 0,
            generation: 3,
        },
        visible_publication_ids: vec!["a".into(), "b".into()],
        ancestors: vec![StructuredAncestorPublicationRange {
            session_id: "parent".into(),
            epoch: 0,
            generation: 2,
            visible_publication_ids: vec!["a".into()],
        }],
    };
    for (scope, ids) in publication_scopes(&view) {
        for id in ids {
            let delta = receipt(&scope, id);
            db.insert_structured_delta(
                &delta,
                &StructuredPublicationView {
                    scope: delta.publication.scope.clone(),
                    visible_publication_ids: vec![],
                    ancestors: vec![],
                },
            )
            .unwrap();
        }
    }
    (root, db, view)
}

fn scan(
    scope: &PublicationScope,
    ids: &[String],
    submit: &mut dyn FnMut(&StructuredDelta) -> Result<()>,
) -> Result<()> {
    for id in ids.iter().rev() {
        submit(&receipt(scope, id))?;
    }
    Ok(())
}

#[test]
fn receipt_stream_accepts_unordered_complete_scopes_with_shared_ids_and_older_generations() {
    let (_root, db, view) = fixture();
    let mut scans = vec![];
    let value = db
        .with_verified_structured_receipt_stream(
            &view,
            &mut |scope, ids, submit| {
                scans.push(scope.session_id.clone());
                scan(scope, ids, submit)
            },
            || Ok(42),
        )
        .unwrap();
    assert_eq!(value, 42);
    assert_eq!(scans, ["child", "parent"]);
    assert!(db.connection.is_autocommit());
}

#[test]
fn receipt_stream_rejects_missing_duplicate_extra_and_foreign_before_read() {
    let (_root, db, view) = fixture();
    for case in [
        "missing",
        "duplicate",
        "extra",
        "project",
        "session",
        "epoch",
        "future",
        "negative",
        "hash",
    ] {
        let read = Cell::new(false);
        let mut submissions = 0;
        let result = db.with_verified_structured_receipt_stream(
            &view,
            &mut |scope, ids, submit| {
                assert_eq!(scope.session_id, "child");
                let mut delta = receipt(scope, &ids[0]);
                match case {
                    "missing" => return submit(&delta),
                    "duplicate" => {
                        submit(&delta)?;
                        submissions += 1;
                    }
                    "extra" => delta.publication.publication_id = "ungranted".into(),
                    "project" => delta.publication.scope.project_id = "elsewhere".into(),
                    "session" => delta.publication.scope.session_id = "parent".into(),
                    "epoch" => delta.publication.scope.epoch = 1,
                    "future" => delta.publication.scope.generation = scope.generation + 1,
                    "negative" => delta.publication.scope.generation = -1,
                    "hash" => delta.publication.raw_hash = "c".repeat(64),
                    _ => unreachable!(),
                }
                submissions += 1;
                submit(&delta)?;
                panic!("invalid receipt accepted: {case}");
            },
            || {
                read.set(true);
                Ok(())
            },
        );
        assert!(result.is_err(), "{case}");
        assert!(!read.get(), "{case}");
        assert_eq!(
            submissions,
            match case {
                "missing" => 0,
                "duplicate" => 2,
                _ => 1,
            }
        );
        assert!(db.connection.is_autocommit(), "{case}");
    }
}

#[test]
fn receipt_stream_keeps_ignored_submission_failures_and_the_original_error() {
    let (_root, db, view) = fixture();
    for case in ["duplicate", "foreign", "stored_delta"] {
        let read = Cell::new(false);
        let error = db
            .with_verified_structured_receipt_stream(
                &view,
                &mut |scope, ids, submit| {
                    let mut bad = receipt(scope, &ids[0]);
                    match case {
                        "duplicate" => submit(&bad)?,
                        "foreign" => bad.publication.scope.session_id = "parent".into(),
                        "stored_delta" => {
                            bad.publication.raw_hash = "c".repeat(64);
                            bad.publication.delta_hash = delta_hash(&bad)?;
                        }
                        _ => unreachable!(),
                    }
                    let _ignored = submit(&bad);
                    for id in ids {
                        // Even a subsequent correct submission cannot erase a
                        // failed verification that the scanner chose to ignore.
                        assert!(submit(&receipt(scope, id)).is_err());
                    }
                    Ok(())
                },
                || {
                    read.set(true);
                    Ok(())
                },
            )
            .unwrap_err();
        let expected = match case {
            "duplicate" => "structured receipt submitted twice",
            "foreign" => "structured prior publication scope mismatch",
            "stored_delta" => "structured publication readback mismatch",
            _ => unreachable!(),
        };
        assert!(error.to_string().contains(expected), "{case}: {error}");
        assert!(!read.get());
    }
}

#[test]
fn receipt_stream_scanner_errors_block_read_before_and_after_complete_submissions() {
    let (_root, db, view) = fixture();
    for complete in [false, true] {
        let read = Cell::new(false);
        let error = db
            .with_verified_structured_receipt_stream(
                &view,
                &mut |scope, ids, submit| {
                    if complete {
                        scan(scope, ids, submit)?;
                    }
                    Err(Error::InvalidInput("owner scan failed"))
                },
                || {
                    read.set(true);
                    Ok(())
                },
            )
            .unwrap_err();
        assert!(matches!(error, Error::InvalidInput("owner scan failed")));
        assert!(!read.get());
    }
}

#[test]
fn receipt_stream_checks_forgotten_scopes_before_their_scanner() {
    for forgotten in ["child", "parent"] {
        let (_root, mut db, view) = fixture();
        db.delete_session("p", forgotten).unwrap();
        let mut scans = vec![];
        let read = Cell::new(false);
        let result = db.with_verified_structured_receipt_stream(
            &view,
            &mut |scope, ids, submit| {
                scans.push(scope.session_id.clone());
                scan(scope, ids, submit)
            },
            || {
                read.set(true);
                Ok(())
            },
        );
        assert!(matches!(result, Err(Error::SessionForgotten)));
        assert_eq!(
            scans,
            if forgotten == "child" {
                vec![]
            } else {
                vec!["child"]
            }
        );
        assert!(!read.get());
    }
}

#[test]
fn receipt_stream_validates_the_entire_view_before_any_scanner() {
    let (_root, db, original) = fixture();
    for case in [
        "current_scope",
        "ancestor_scope",
        "ancestor_epoch",
        "current_duplicate",
        "ancestor_duplicate",
        "session_duplicate",
        "count",
        "bytes",
        "depth",
    ] {
        let mut view = original.clone();
        match case {
            "current_scope" => view.scope.project_id.clear(),
            "ancestor_scope" => view.ancestors[0].session_id.clear(),
            "ancestor_epoch" => view.ancestors[0].epoch = -1,
            "current_duplicate" => view.visible_publication_ids.push("a".into()),
            "ancestor_duplicate" => view.ancestors[0].visible_publication_ids.push("a".into()),
            "session_duplicate" => view.ancestors.push(view.ancestors[0].clone()),
            "count" => {
                view.ancestors[0].visible_publication_ids =
                    (0..65_537).map(|i| i.to_string()).collect()
            }
            "bytes" => {
                view.ancestors[0].visible_publication_ids =
                    (0..4_097).map(|i| format!("{i:01024}")).collect()
            }
            "depth" => {
                view.ancestors = (0..1_025)
                    .map(|i| StructuredAncestorPublicationRange {
                        session_id: format!("parent-{i}"),
                        ..view.ancestors[0].clone()
                    })
                    .collect()
            }
            _ => unreachable!(),
        }
        let scans = Cell::new(0);
        let read = Cell::new(false);
        let result = db.with_verified_structured_receipt_stream(
            &view,
            &mut |scope, ids, submit| {
                scans.set(scans.get() + 1);
                scan(scope, ids, submit)
            },
            || {
                read.set(true);
                Ok(())
            },
        );
        assert!(result.is_err(), "{case}");
        assert_eq!(scans.get(), 0, "{case}");
        assert!(!read.get(), "{case}");
    }
}

#[test]
fn receipt_stream_skips_empty_scopes_but_still_checks_current_tombstone() {
    let (_root, mut db, mut view) = fixture();
    view.visible_publication_ids.clear();
    view.ancestors[0].visible_publication_ids.clear();
    let read = Cell::new(0);
    let mut scanner = |_: &PublicationScope,
                       _: &[String],
                       _: &mut dyn FnMut(&StructuredDelta) -> Result<()>|
     -> Result<()> { panic!("empty scope scanned") };
    db.with_verified_structured_receipt_stream(&view, &mut scanner, || {
        read.set(read.get() + 1);
        Ok(())
    })
    .unwrap();
    db.delete_session("p", "child").unwrap();
    assert!(matches!(
        db.with_verified_structured_receipt_stream(&view, &mut scanner, || {
            read.set(read.get() + 1);
            Ok(())
        }),
        Err(Error::SessionForgotten)
    ));
    assert_eq!(read.get(), 1);
}

#[test]
fn receipt_stream_and_nested_reads_share_the_snapshot_during_wal_changes() {
    let (root, db, view) = fixture();
    db.connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    let mut changed = false;
    db.with_verified_structured_receipt_stream(&view, &mut |scope, ids, submit| {
        for id in ids {
            if !changed {
                let other = rusqlite::Connection::open(root.path().join("memory.sqlite3"))?;
                other.execute("UPDATE conversation_publications_v3 SET raw_hash=?1 WHERE session_id='child' AND publication_id='b'", params!["c".repeat(64)])?;
                changed = true;
            }
            submit(&receipt(scope, id))?;
        }
        Ok(())
    }, || {
        // The existing per-ID API must reuse the outer snapshot as well.
        db.with_verified_structured_receipts(&view, &mut |scope, id| Ok(receipt(scope, id)), || {
            assert_eq!(db.structured_publication(&PublicationScope { generation: 1, ..view.scope.clone() }, "b")?.unwrap().raw_hash, "a".repeat(64));
            Ok(())
        })
    }).unwrap();
    assert!(changed);
    let read = Cell::new(false);
    assert!(
        db.with_verified_structured_receipt_stream(&view, &mut scan, || {
            read.set(true);
            Ok(())
        })
        .is_err()
    );
    assert!(!read.get());
}

#[test]
fn receipt_stream_ancestor_tombstone_is_checked_in_the_same_snapshot() {
    let (root, db, view) = fixture();
    db.connection
        .pragma_update(None, "journal_mode", "WAL")
        .unwrap();
    let mut scans = vec![];
    db.with_verified_structured_receipt_stream(
        &view,
        &mut |scope, ids, submit| {
            scans.push(scope.session_id.clone());
            if scope.session_id == "child" {
                MemoryStore::open(root.path().join("memory.sqlite3"))?
                    .delete_session("p", "parent")?;
            }
            scan(scope, ids, submit)
        },
        || Ok(()),
    )
    .unwrap();
    assert_eq!(scans, ["child", "parent"]);
    let mut later_scans = vec![];
    assert!(matches!(
        db.with_verified_structured_receipt_stream(
            &view,
            &mut |scope, ids, submit| {
                later_scans.push(scope.session_id.clone());
                scan(scope, ids, submit)
            },
            || -> Result<()> { panic!("forgotten ancestor allowed") }
        ),
        Err(Error::SessionForgotten)
    ));
    assert_eq!(later_scans, ["child"]);
}

#[test]
fn per_id_reader_still_rejects_swapped_granted_receipts() {
    let (_root, db, view) = fixture();
    let error = db
        .with_verified_structured_receipts(
            &view,
            &mut |scope, id| Ok(receipt(scope, if id == "a" { "b" } else { "a" })),
            || -> Result<()> { panic!("swapped receipts allowed") },
        )
        .unwrap_err();
    assert!(matches!(
        error,
        Error::InvalidInput("structured prior publication scope mismatch")
    ));
}

struct PublicationFixture {
    _root: tempfile::TempDir,
    db: MemoryStore,
    prior: StructuredPublicationView,
    published: crate::conversation::PublishedView,
    pending_id: String,
    delta: StructuredDelta,
}
fn publication_fixture() -> PublicationFixture {
    use crate::conversation::{
        AncestorRange, EmbeddingMetadata, PendingSummary, PublishedView, Scope, SourceMetadata,
    };
    use sha2::{Digest, Sha256};
    let (root, mut db, prior) = fixture();
    let summary = r#"{"facts":["fixture"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[1]}"#.to_owned();
    let mut pending = PendingSummary {
        scope: Scope {
            project_id: "p".into(),
            session_id: "child".into(),
            epoch: 0,
            generation: 4,
        },
        id: "summary".into(),
        source: SourceMetadata {
            id: "source".into(),
            start_turn: 1,
            end_turn: 1,
            raw_hash: "a".repeat(64),
        },
        summary_hash: format!("{:x}", Sha256::digest(summary.as_bytes())),
        summary,
        model: "gpt-6-astra".into(),
        effort: "medium".into(),
        prompt_version: "fixture".into(),
        embedding: EmbeddingMetadata {
            model: "e5".into(),
            revision: "r1".into(),
            dimension: 2,
            input_hash: "a".repeat(64),
            values: vec![1., 0.],
        },
    };
    pending.embedding.input_hash = pending.summary_hash.clone();
    db.insert_pending_summary(&pending).unwrap();
    let published = PublishedView {
        scope: pending.scope.clone(),
        visible_ids: vec![pending.id.clone()],
        ancestors: vec![AncestorRange {
            session_id: "parent".into(),
            epoch: 0,
            generation: 2,
            visible_ids: vec!["parent-summary".into()],
        }],
    };
    pending.scope.session_id = "parent".into();
    pending.scope.generation = 2;
    pending.id = "parent-summary".into();
    pending.source.id = "parent-source".into();
    db.insert_pending_summary(&pending).unwrap();
    let mut delta = receipt(&prior.scope, "new");
    delta.publication.scope.generation = 4;
    delta.publication.delta_hash = delta_hash(&delta).unwrap();
    PublicationFixture {
        _root: root,
        db,
        prior,
        published,
        pending_id: "summary".into(),
        delta,
    }
}

#[test]
fn publication_stream_checks_v2_only_parent_before_owner_access_and_holds_sqlite_guard() {
    let mut f = publication_fixture();
    f.prior.visible_publication_ids.clear();
    f.prior.ancestors[0].visible_publication_ids.clear();
    let mut scans = Vec::new();
    let called = Cell::new(0);
    let other = rusqlite::Connection::open(f._root.path().join("memory.sqlite3")).unwrap();
    other.busy_timeout(std::time::Duration::ZERO).unwrap();
    f.db.publish_pending_with_receipt_stream(
        &f.published,
        std::slice::from_ref(&f.pending_id),
        &f.prior,
        &mut |scope, ids, verifier| {
            assert!(ids.is_empty());
            scans.push(scope.session_id.clone());
            verifier.verify_summaries(|_| Ok(()))
        },
        || {
            assert!(
                other
                    .execute(
                        "DELETE FROM conversation_summaries_v2 WHERE session_id='parent'",
                        []
                    )
                    .is_err()
            );
            called.set(called.get() + 1);
            Ok(())
        },
    )
    .unwrap();
    assert_eq!(scans, ["child", "parent"]);
    f.db.delete_session("p", "parent").unwrap();
    scans.clear();
    let result = f.db.publish_pending_with_receipt_stream(
        &f.published,
        std::slice::from_ref(&f.pending_id),
        &f.prior,
        &mut |scope, _, verifier| {
            scans.push(scope.session_id.clone());
            verifier.verify_summaries(|_| Ok(()))
        },
        || {
            called.set(called.get() + 1);
            Ok(())
        },
    );
    assert!(matches!(result, Err(Error::SessionForgotten)));
    assert_eq!(scans, ["child"]);
    assert_eq!(called.get(), 1);
}

#[test]
fn publication_stream_scans_once_and_reuses_committed_delta_after_marker_failure() {
    let mut f = publication_fixture();
    let scans = Cell::new(0);
    let called = Cell::new(0);
    for fail in [true, false] {
        let result = f.db.publish_pending_with_structured_receipt_stream(
            &f.published,
            std::slice::from_ref(&f.pending_id),
            &f.delta,
            &f.prior,
            &mut |scope, ids, submit| {
                scans.set(scans.get() + 1);
                scan(scope, ids, &mut |delta| submit.submit_structured(delta))?;
                submit.verify_summaries(|_| Ok(()))
            },
            || {
                called.set(called.get() + 1);
                if fail {
                    Err(Error::InvalidInput("marker failed"))
                } else {
                    Ok(())
                }
            },
        );
        assert_eq!(result.is_err(), fail);
        f.db.verify_structured_delta(&f.delta).unwrap();
    }
    assert_eq!(scans.get(), 4);
    assert_eq!(called.get(), 2);
}

#[test]
fn publication_stream_rejects_missing_v2_ancestor_before_marker() {
    let mut f = publication_fixture();
    f.db.connection
        .execute(
            "DELETE FROM conversation_summaries_v2 WHERE session_id='parent'",
            [],
        )
        .unwrap();
    assert!(
        f.db.publish_pending_with_structured_receipt_stream(
            &f.published,
            std::slice::from_ref(&f.pending_id),
            &f.delta,
            &f.prior,
            &mut |scope, ids, verifier| {
                scan(scope, ids, &mut |d| verifier.submit_structured(d))?;
                verifier.verify_summaries(|_| Ok(()))
            },
            || -> Result<()> { panic!("missing ancestor reached marker") }
        )
        .is_err()
    );
}

#[test]
fn publication_stream_rejects_intervening_commits_and_revalidates_saved_retry() {
    for journal in ["WAL", "DELETE"] {
        for case in ["v2-body", "v3-body", "tombstone", "unrelated"] {
            let mut f = publication_fixture();
            f.db.connection
                .pragma_update(None, "journal_mode", journal)
                .unwrap();
            let path = f._root.path().join("memory.sqlite3");
            let hook_path = path.clone();
            crate::conversation::after_structured_index_commit(move || {
                let mut other = rusqlite::Connection::open(hook_path).unwrap();
                other.execute_batch("PRAGMA foreign_keys=ON").unwrap();
                let tx = other.transaction().unwrap();
                // Only body rows change: publication headers/hashes stay intact.
                let changed = match case {
                    "v2-body" => tx.execute("UPDATE conversation_vectors_v2 SET revision='r2' WHERE session_id='parent'", []).unwrap(),
                    "v3-body" => tx.execute("INSERT INTO conversation_fact_dependencies_v3 VALUES('p','parent',0,1,'a','unexpected','dependency','source')", []).unwrap(),
                    "tombstone" => tx.execute("INSERT INTO memory_forgotten_sessions VALUES('p','parent')", []).unwrap(),
                    "unrelated" => tx.execute("INSERT INTO memory_forgotten_sessions VALUES('another-project','unrelated-session')", []).unwrap(),
                    _ => unreachable!(),
                };
                assert_eq!(changed, 1);
                tx.commit().unwrap();
            });
            let scans = Cell::new(0);
            let mut scanner = |scope: &PublicationScope,
                               ids: &[String],
                               verifier: &mut crate::conversation::PublicationReceiptVerifier<
                '_,
            >| {
                scans.set(scans.get() + 1);
                scan(scope, ids, &mut |d| verifier.submit_structured(d))?;
                verifier.verify_summaries(|_| Ok(()))
            };
            let error =
                f.db.publish_pending_with_structured_receipt_stream(
                    &f.published,
                    std::slice::from_ref(&f.pending_id),
                    &f.delta,
                    &f.prior,
                    &mut scanner,
                    || -> Result<()> { panic!("{journal}/{case}: changed index reached marker") },
                )
                .unwrap_err();
            assert!(
                matches!(
                    error,
                    Error::InvalidInput("structured index changed before marker publication")
                ),
                "{journal}/{case}: {error}"
            );
            assert_eq!(scans.get(), 2, "{journal}/{case}");
            assert!(f.db.connection.is_autocommit());
            // The already committed response/index can be reused without extraction.
            f.db.verify_structured_delta(&f.delta).unwrap();
            let other = rusqlite::Connection::open(path).unwrap();
            match case {
                "v2-body" => {
                    other.execute("UPDATE conversation_vectors_v2 SET revision='r1' WHERE session_id='parent'", []).unwrap();
                }
                "v3-body" => {
                    other.execute("DELETE FROM conversation_fact_dependencies_v3 WHERE session_id='parent'", []).unwrap();
                }
                _ => (),
            }
            let called = Cell::new(false);
            let retry = f.db.publish_pending_with_structured_receipt_stream(
                &f.published,
                std::slice::from_ref(&f.pending_id),
                &f.delta,
                &f.prior,
                &mut scanner,
                || {
                    called.set(true);
                    Ok(())
                },
            );
            if case == "tombstone" {
                // Forget remains authoritative; a retry must not undo it.
                assert!(matches!(retry, Err(Error::SessionForgotten)));
                assert!(!called.get());
            } else {
                retry.unwrap();
                assert!(called.get(), "{journal}/{case}");
            }
        }
    }
}

#[test]
fn publication_stream_preserves_verified_snapshot_without_an_external_commit() {
    for journal in ["WAL", "DELETE"] {
        let mut f = publication_fixture();
        f.db.connection
            .pragma_update(None, "journal_mode", journal)
            .unwrap();
        let path = f._root.path().join("memory.sqlite3");
        crate::conversation::after_structured_index_commit(move || {
            let mut other = rusqlite::Connection::open(path).unwrap();
            let tx = other.transaction().unwrap();
            assert_eq!(tx.execute("UPDATE conversation_vectors_v2 SET revision='rolled-back' WHERE session_id='parent'", []).unwrap(), 1);
            tx.rollback().unwrap();
        });
        let called = Cell::new(false);
        f.db.publish_pending_with_structured_receipt_stream(
            &f.published,
            std::slice::from_ref(&f.pending_id),
            &f.delta,
            &f.prior,
            &mut |scope, ids, verifier| {
                scan(scope, ids, &mut |d| verifier.submit_structured(d))?;
                verifier.verify_summaries(|_| Ok(()))
            },
            || {
                called.set(true);
                Ok(())
            },
        )
        .unwrap();
        assert!(called.get());
        f.db.verify_structured_delta(&f.delta).unwrap();
    }
}

#[test]
fn publication_stream_rejects_swallowed_receipt_error_without_committing_new_delta() {
    let mut f = publication_fixture();
    let called = Cell::new(false);
    let result = f.db.publish_pending_with_structured_receipt_stream(
        &f.published,
        std::slice::from_ref(&f.pending_id),
        &f.delta,
        &f.prior,
        &mut |scope, ids, submit| {
            for id in ids {
                let mut bad = receipt(scope, id);
                bad.publication.raw_hash = "b".repeat(64);
                let _ = submit.submit_structured(&bad);
            }
            Ok(())
        },
        || {
            called.set(true);
            Ok(())
        },
    );
    assert!(result.is_err());
    assert!(!called.get());
    assert!(
        f.db.structured_publication(&f.delta.publication.scope, "new")
            .unwrap()
            .is_none()
    );
}

#[test]
fn publication_stream_cannot_skip_or_swallow_summary_verification() {
    for skip in [true, false] {
        let mut f = publication_fixture();
        f.prior.visible_publication_ids.clear();
        f.prior.ancestors[0].visible_publication_ids.clear();
        let called = Cell::new(false);
        let result = f.db.publish_pending_with_receipt_stream(
            &f.published,
            std::slice::from_ref(&f.pending_id),
            &f.prior,
            &mut |scope, _, verifier| {
                if scope.session_id == "parent" {
                    if !skip {
                        let _ = verifier
                            .verify_summaries(|_| Err(Error::InvalidInput("bad owner receipt")));
                    }
                    Ok(())
                } else {
                    verifier.verify_summaries(|_| Ok(()))
                }
            },
            || {
                called.set(true);
                Ok(())
            },
        );
        assert!(result.is_err(), "skip={skip}");
        assert!(!called.get(), "skip={skip}");
    }
}
