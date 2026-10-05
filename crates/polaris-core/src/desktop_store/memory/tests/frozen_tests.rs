//! Fixed ancestor reader: locks, receipts, raw bindings and read-only failures.
use super::*;
mod initialization_tests;
mod prompt_tests;
use crate::desktop_store::disk;
use std::{fs, path::PathBuf};

fn bytes(path: &Path) -> Vec<Vec<u8>> {
    [disk::MARKER, disk::SIDECAR, disk::RAW]
        .into_iter()
        .map(|name| fs::read(path.join(name)).unwrap())
        .collect()
}

struct Fixture {
    root: PrototypeRoot,
    parent: Writer,
    child: Writer,
    pending: PendingSummary,
    response: SavedStructuredResponse,
    database: PathBuf,
    _directory: tempfile::TempDir,
}
async fn published_parent() -> Fixture {
    let (root, mut parent, target) =
        setup_mode("memory-project", "frozen-parent", HistoryMode::Structured10);
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let (work, pending, response) = structured_pending(&mut parent, &target).await;
    parent
        .publish_structured_memory(&target, &work, &pending, &response, &database)
        .unwrap();
    finish(&mut parent, &target, 11);
    let child = parent
        .fork_memory(SessionId::new("frozen-child").unwrap(), "artifact-a")
        .unwrap();
    Fixture {
        root,
        parent,
        child,
        pending,
        response,
        database,
        _directory: directory,
    }
}

#[tokio::test]
async fn frozen_reader_holds_parent_lock_and_reads_only_the_fork_grants_after_later_publication() {
    let Fixture {
        root,
        mut parent,
        child,
        pending,
        response,
        database,
        _directory,
    } = published_parent().await;
    let child_snapshot = child.snapshot().unwrap();
    let parent_path = parent.test_directory();
    let parent_before = bytes(&parent_path);
    let child_before = bytes(&child.test_directory());
    let db = MemoryStore::open_read_only(&database).unwrap();
    assert!(matches!(
        frozen::with_published_receipts(&child, &db, |_| Ok(())),
        Err(StoreError::Busy)
    ));
    assert_eq!(bytes(&parent_path), parent_before);
    assert_eq!(bytes(&child.test_directory()), child_before);

    let target = accept_text(&mut parent, 12, "Later parent publication");
    let snapshot = parent.memory_snapshot(&target).unwrap();
    let work = parent.begin_summary(&target, &snapshot, 2).unwrap();
    let intent = work.structured_intent.as_ref().unwrap();
    let next = crate::conversation_memory::structured::validate_response(
        &work.source, 2, intent,
        &serde_json::json!({
            "summary": {"facts":["Later parent note"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[2]},
            "processed_source_ids": intent.sources.iter().map(|s| s.source_id.clone()).collect::<Vec<_>>(),
            "entity_candidates":[],"fact_deltas":[]
        }).to_string(),
    ).unwrap();
    parent
        .save_structured_response(&target, &work, &next)
        .unwrap();
    let later = history()
        .embed_summary(&work.source, 2, next.summary.clone())
        .await
        .unwrap();
    parent
        .publish_structured_memory(
            &target,
            &work,
            &later,
            &SavedStructuredResponse {
                summary: next.summary,
                delta: next.delta,
                coverage: next.coverage,
            },
            &database,
        )
        .unwrap();
    finish(&mut parent, &target, 12);
    let parent_marker = parent.snapshot().unwrap().marker;
    drop(parent);
    let parent_after = bytes(&parent_path);
    for reopen in [false, true] {
        let read = |child: &Writer| {
            frozen::with_published_receipts(child, &db, |owners| {
                let owner = owners
                    .iter()
                    .find(|owner| owner.matches(&response.delta.publication.scope))
                    .unwrap();
                let proof = owner.verify_summary(&pending)?;
                let raw = owner.summary_raw(proof)?;
                assert_eq!(raw.len(), 2);
                assert!(raw[0].message.content.contains("Rigel"));
                assert!(!owner.summaries.contains_key(&later.id));
                assert!(matches!(
                    root.open(&parent_marker.project_id, &parent_marker.session_id),
                    Err(StoreError::Busy)
                ));
                Ok(())
            })
            .unwrap();
        };
        if reopen {
            let id = child_snapshot.marker.session_id.clone();
            drop(child);
            let child = root.open(&child_snapshot.marker.project_id, &id).unwrap();
            read(&child);
            assert_eq!(bytes(&parent_path), parent_after);
            assert_eq!(bytes(&child.test_directory()), child_before);
            break;
        } else {
            read(&child);
        }
    }
    assert_eq!(bytes(&parent_path), parent_after);
}

#[tokio::test]
async fn frozen_reader_rejects_forgotten_parent_before_open_and_keeps_files_unchanged() {
    let Fixture {
        parent,
        child,
        database,
        _directory,
        ..
    } = published_parent().await;
    let before = bytes(&parent.test_directory());
    let mut db = MemoryStore::open(&database).unwrap();
    db.delete_session("memory-project", "frozen-parent")
        .unwrap();
    let error = frozen::with_published_receipts(&child, &db, |_| Ok::<(), StoreError>(()))
        .err()
        .unwrap();
    assert!(
        !matches!(error, StoreError::Busy),
        "parent is locked, so Busy would expose premature owner access"
    );
    assert!(error.to_string().contains("forgotten"), "{error}");
    assert_eq!(bytes(&parent.test_directory()), before);
}

fn replace_last_state(
    path: &Path,
    original: &[u8],
    mut marker: Marker,
    edit: impl FnOnce(&mut serde_json::Value),
) -> Marker {
    let last = original[..original.len() - 1]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |at| at + 1);
    let mut value: serde_json::Value = serde_json::from_slice(&original[last..]).unwrap();
    edit(&mut value);
    let mut changed = original[..last].to_vec();
    changed.extend(disk::line(&value).unwrap());
    marker.sidecar_offset = DecimalU64::new(changed.len() as u64);
    marker.sidecar_hash = content_hash(&changed);
    fs::write(path.join(disk::SIDECAR), changed).unwrap();
    fs::write(
        path.join(disk::MARKER),
        serde_json::to_vec(&marker).unwrap(),
    )
    .unwrap();
    marker
}

#[tokio::test]
async fn frozen_host_user_content_requires_an_actual_input_boundary_for_active_facts() {
    use polaris_memory::structured::{FactStatus, stable_fact_id};
    use polaris_provider::Role;
    use serde_json::Value;
    let Fixture {
        parent,
        child,
        pending,
        response,
        ..
    } = published_parent().await;
    let path = parent.test_directory();
    let original_marker = parent.snapshot().unwrap().marker;
    let original_sidecar = fs::read(path.join(disk::SIDECAR)).unwrap();
    let original_raw = fs::read(path.join(disk::RAW)).unwrap();
    drop(parent);
    let old_events: Vec<RawEventV2> = original_raw
        .split_inclusive(|byte| *byte == b'\n')
        .map(|line| serde_json::from_slice(line).unwrap())
        .collect();
    let mut events = old_events.clone();
    assert_eq!(events[1].message.role, Role::Assistant);
    assert!(!events[1].starts_turn);
    events[1].message.role = Role::User;
    let (mut old_prefix, mut new_prefix) = (Vec::new(), Vec::new());
    let (mut offsets, mut hashes, mut turns) = (
        std::collections::BTreeMap::new(),
        std::collections::BTreeMap::new(),
        std::collections::BTreeMap::new(),
    );
    offsets.insert(0u64, 0u64);
    for (old, new) in old_events.iter().zip(&events) {
        old_prefix.extend(disk::line(old).unwrap());
        new_prefix.extend(disk::line(new).unwrap());
        offsets.insert(old_prefix.len() as u64, new_prefix.len() as u64);
        hashes.insert(content_hash(&old_prefix), content_hash(&new_prefix));
    }
    for turn in events.iter().map(|e| e.turn_id) {
        let group: Vec<_> = events.iter().filter(|e| e.turn_id == turn).collect();
        turns.insert(turn, content_hash(&serde_json::to_vec(&group).unwrap()));
    }
    fn rebind(
        value: &mut Value,
        offsets: &std::collections::BTreeMap<u64, u64>,
        hashes: &std::collections::BTreeMap<String, String>,
        turns: &std::collections::BTreeMap<u64, String>,
    ) {
        match value {
            Value::Object(object) => {
                if let Some(offset) = object.get_mut("raw_offset") {
                    let old = offset
                        .as_u64()
                        .or_else(|| offset.as_str().and_then(|s| s.parse().ok()))
                        .unwrap();
                    let new = offsets[&old];
                    *offset = if offset.is_string() {
                        Value::String(new.to_string())
                    } else {
                        Value::from(new)
                    };
                }
                if object.contains_key("turn_hash") {
                    let turn = object["turn"].as_str().unwrap().parse().unwrap();
                    object.insert("turn_hash".into(), Value::String(turns[&turn].clone()));
                }
                for value in object.values_mut() {
                    rebind(value, offsets, hashes, turns);
                }
            }
            Value::Array(values) => {
                for value in values {
                    rebind(value, offsets, hashes, turns);
                }
            }
            Value::String(value) => {
                if let Some(next) = hashes.get(value) {
                    *value = next.clone();
                }
            }
            _ => {}
        }
    }
    fs::write(path.join(disk::RAW), &new_prefix).unwrap();
    let mut raw_marker = original_marker.clone();
    raw_marker.raw_offset = DecimalU64::new(new_prefix.len() as u64);
    raw_marker.raw_hash = content_hash(&new_prefix);
    for active in [false, true] {
        let changed = replace_last_state(&path, &original_sidecar, raw_marker.clone(), |value| {
            rebind(value, &offsets, &hashes, &turns);
            let structured = &mut value["conversation_memory"]["attempts"][0]["structured"];
            let source = &mut structured["intent"]["sources"][1];
            assert_eq!(source["sequence"], 2);
            source["role"] = serde_json::to_value(Role::User).unwrap();
            let source_id = source["source_id"].as_str().unwrap().to_owned();
            let mut delta: StructuredDelta =
                serde_json::from_value(structured["response"]["delta"].clone()).unwrap();
            let fact = &mut delta.facts[0];
            fact.source_id = source_id;
            fact.fact_id = stable_fact_id(
                &fact.entity_id,
                &fact.fact_kind,
                &fact.condition,
                &fact.source_id,
            )
            .unwrap();
            fact.status = if active {
                FactStatus::Active
            } else {
                FactStatus::Proposed
            };
            fact.reason = if active { "assert" } else { "unconfirmed" }.into();
            delta.publication.delta_hash = polaris_memory::structured::delta_hash(&delta).unwrap();
            polaris_memory::structured::validate_delta(&delta).unwrap();
            structured["response"]["delta"] = serde_json::to_value(delta).unwrap();
        });
        let before = bytes(&path);
        let read = frozen::FrozenMemory::load(
            &child,
            Default::default(),
            &fork::FrozenAncestor {
                marker: changed,
                generation: DecimalU64::new(1),
                summary_ids: vec![pending.id.clone()],
                structured_ids: vec![response.delta.publication.publication_id.clone()],
            },
            &mut |_| Ok(()),
        );
        if active {
            assert!(matches!(
                read,
                Err(StoreError::Corrupt("固定原文の利用者入力根拠"))
            ));
        } else {
            read.unwrap();
        }
        assert_eq!(bytes(&path), before);
    }
    fs::write(path.join(disk::RAW), original_raw).unwrap();
    fs::write(path.join(disk::SIDECAR), original_sidecar).unwrap();
    fs::write(
        path.join(disk::MARKER),
        serde_json::to_vec(&original_marker).unwrap(),
    )
    .unwrap();
}

#[tokio::test]
async fn frozen_projection_rejects_crossed_operations_raw_bindings_and_coverage() {
    let Fixture {
        parent,
        child,
        pending,
        response,
        ..
    } = published_parent().await;
    let path = parent.test_directory();
    let marker = parent.snapshot().unwrap().marker;
    let sidecar = fs::read(path.join(disk::SIDECAR)).unwrap();
    drop(parent);
    // serde_json::Value orders all object keys differently from the saved
    // structs, including memory before runs. Equivalent field order is valid.
    let reordered = replace_last_state(&path, &sidecar, marker.clone(), |_| {});
    let valid = frozen::FrozenMemory::load(
        &child,
        Default::default(),
        &fork::FrozenAncestor {
            marker: reordered.clone(),
            generation: DecimalU64::new(1),
            summary_ids: vec![pending.id.clone()],
            structured_ids: vec![response.delta.publication.publication_id.clone()],
        },
        &mut |_| Ok(()),
    )
    .unwrap();
    assert_eq!(
        valid
            .summary_raw(valid.verify_summary(&pending).unwrap())
            .unwrap()
            .len(),
        2
    );
    drop(valid);
    for case in 0..9 {
        let changed = replace_last_state(&path, &sidecar, marker.clone(), |value| match case {
            0 => {
                value["conversation_memory"]["attempts"][0]["operation_id"] =
                    "different-operation".into()
            }
            1 => {
                value["conversation_memory"]["attempts"][0]["structured"]["intent"]["sources"][0]
                    ["content"] = "different raw content".into()
            }
            2 => {
                value["conversation_memory"]["attempts"][0]["source"]["turn_hash"] =
                    "0".repeat(64).into();
                value["conversation_memory"]["sources"][0]["source"]["turn_hash"] =
                    "0".repeat(64).into();
            }
            3 => {
                value["conversation_memory"]["attempts"][0]["structured"]["intent"]["coverage"]["prefix_hash"] =
                    "0".repeat(64).into()
            }
            4 => value["unknown_authority"] = true.into(),
            5 => {
                let run = value["runs"].as_array_mut().unwrap().last_mut().unwrap();
                let operations = run["operations"].as_array_mut().unwrap();
                let operation = operations
                    .iter_mut()
                    .find(|op| op["operation_id"].as_str().unwrap().starts_with("memory-"))
                    .unwrap();
                operation["result_id"] = serde_json::Value::Null;
            }
            6 => value["desktop_sidecar_schema_version"] = 3.into(),
            7 => {
                value.as_object_mut().unwrap().remove("tier_bindings");
            }
            8 => {
                value["goal"] = serde_json::json!({"text":"invalid", "revision":"0", "active":true})
            }
            _ => unreachable!(),
        });
        let before = bytes(&path);
        let read = frozen::FrozenMemory::load(
            &child,
            Default::default(),
            &fork::FrozenAncestor {
                marker: changed.clone(),
                generation: DecimalU64::new(1),
                summary_ids: vec![pending.id.clone()],
                structured_ids: vec![response.delta.publication.publication_id.clone()],
            },
            &mut |_| Ok(()),
        );
        assert!(read.is_err(), "case {case}");
        assert_eq!(bytes(&path), before);
    }
    fs::write(path.join(disk::SIDECAR), &sidecar).unwrap();
    fs::write(
        path.join(disk::MARKER),
        serde_json::to_vec(&marker).unwrap(),
    )
    .unwrap();
    let positive = frozen::FrozenMemory::load(
        &child,
        Default::default(),
        &fork::FrozenAncestor {
            marker: marker.clone(),
            generation: DecimalU64::new(1),
            summary_ids: vec![pending.id.clone()],
            structured_ids: vec![response.delta.publication.publication_id.clone()],
        },
        &mut |_| Ok(()),
    )
    .unwrap();
    assert_eq!(
        positive
            .summary_raw(positive.verify_summary(&pending).unwrap())
            .unwrap()
            .len(),
        2
    );
}

#[tokio::test]
async fn frozen_reader_rejects_changed_identity_truncated_prefix_and_duplicate_fields() {
    let Fixture {
        parent,
        child,
        pending,
        response,
        ..
    } = published_parent().await;
    let path = parent.test_directory();
    let marker = parent.snapshot().unwrap().marker;
    let original = bytes(&path);
    drop(parent);
    let load = |frozen: &Marker| {
        frozen::FrozenMemory::load(
            &child,
            Default::default(),
            &fork::FrozenAncestor {
                marker: (frozen).clone(),
                generation: DecimalU64::new(1),
                summary_ids: vec![pending.id.clone()],
                structured_ids: vec![response.delta.publication.publication_id.clone()],
            },
            &mut |_| Ok(()),
        )
    };
    assert!(load(&marker).is_ok());
    for case in 0..5 {
        let mut frozen = marker.clone();
        match case {
            0 => frozen.project_id = ProjectId::new("other-project").unwrap(),
            1 => {
                let mut current = marker.clone();
                current.epoch = DecimalU64::new(2);
                fs::write(
                    path.join(disk::MARKER),
                    serde_json::to_vec(&current).unwrap(),
                )
                .unwrap();
            }
            2 => {
                fs::write(path.join(disk::RAW), &original[2][..original[2].len() - 1]).unwrap();
            }
            3 => {
                fs::write(
                    path.join(disk::SIDECAR),
                    &original[1][..original[1].len() - 1],
                )
                .unwrap();
            }
            4 => {
                let mut side = original[1].clone();
                let last = side[..side.len() - 1]
                    .iter()
                    .rposition(|byte| *byte == b'\n')
                    .map_or(0, |at| at + 1);
                side.splice(last + 1..last + 1, b"\"tasks\":[],".iter().copied());
                frozen.sidecar_offset = DecimalU64::new(side.len() as u64);
                frozen.sidecar_hash = content_hash(&side);
                fs::write(path.join(disk::SIDECAR), side).unwrap();
                fs::write(
                    path.join(disk::MARKER),
                    serde_json::to_vec(&frozen).unwrap(),
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        let before = bytes(&path);
        assert!(load(&frozen).is_err(), "case {case}");
        assert_eq!(bytes(&path), before);
        for (name, contents) in [disk::MARKER, disk::SIDECAR, disk::RAW]
            .into_iter()
            .zip(&original)
        {
            fs::write(path.join(name), contents).unwrap();
        }
    }
    assert!(load(&marker).is_ok());
}
