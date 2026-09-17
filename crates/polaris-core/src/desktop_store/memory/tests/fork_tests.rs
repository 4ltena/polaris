//! Fork publication, run ordinal ownership and bounded frozen grant regressions.
use super::*;
use crate::desktop_store::disk;
use crate::workflow::{ExternalGrant, SessionWorkflow, VerificationEvidence, WorkflowConfig};
use polaris_provider::Message;
use std::fs;

fn workflow() -> SavedWorkflow {
    let mut saved = SavedWorkflow::capture(&SessionWorkflow::new(WorkflowConfig {
        enabled: true,
        always: vec!["builtin:workflow-core".into()],
        ..Default::default()
    }));
    saved.gates.external_grants.push(ExternalGrant {
        operation: "push".into(),
        target: "main".into(),
        artifact_hash: "artifact-a".into(),
    });
    saved.gates.verification = Some(VerificationEvidence {
        artifact_hash: "artifact-a".into(),
        passed: true,
        stale: false,
    });
    saved
}
fn with_workflow(parent: &mut Writer) {
    parent
        .configure_workflow(
            parent.snapshot().unwrap().marker.session_revision,
            Some(workflow()),
        )
        .unwrap();
}
fn child_id() -> SessionId {
    SessionId::new("fork-child").unwrap()
}
fn owner_bytes(writer: &Writer) -> Vec<Vec<u8>> {
    [disk::RAW, disk::SIDECAR, disk::MARKER]
        .into_iter()
        .map(|name| fs::read(writer.test_directory().join(name)).unwrap())
        .collect()
}
fn child_path(parent: &Writer, id: &SessionId) -> std::path::PathBuf {
    parent
        .test_directory()
        .parent()
        .unwrap()
        .join(content_hash(id.as_str().as_bytes()))
}
fn frozen_child(parent: &Writer) -> Published {
    let p = parent.snapshot().unwrap();
    let (state, raw) = fork::fork_seed(&p, &child_id(), "artifact-b").unwrap();
    let mut marker = p.marker;
    marker.session_id = child_id();
    marker.epoch = DecimalU64::new(1);
    Published { marker, state, raw }
}

#[tokio::test]
async fn fork_freezes_exact_parent_ids_without_local_memory_or_execution_capabilities() {
    let (root, mut parent, target) =
        setup_mode("memory-project", "fork-parent", HistoryMode::Structured10);
    let db = tempfile::tempdir().unwrap();
    let (work, pending, response) = structured_pending(&mut parent, &target).await;
    parent
        .publish_structured_memory(
            &target,
            &work,
            &pending,
            &response,
            &db.path().join("memory.sqlite3"),
        )
        .unwrap();
    finish(&mut parent, &target, 11);
    with_workflow(&mut parent);
    let before = parent.snapshot().unwrap();
    let bytes = owner_bytes(&parent);
    let child = parent.fork_memory(child_id(), "artifact-b").unwrap();
    let forked = child.snapshot().unwrap();
    assert_eq!(owner_bytes(&parent), bytes);
    assert_eq!(parent.snapshot().unwrap().marker, before.marker);
    assert_eq!(forked.state.inherited_turn_count.get(), 10);
    assert_eq!(forked.marker.session_revision.get(), 0);
    assert_eq!(forked.marker.content_revision.get(), 0);
    assert_eq!(forked.raw.len(), 20);
    for (index, event) in forked.raw.iter().enumerate() {
        assert_eq!(event.sequence, index as u64 + 1);
        assert_eq!(event.epoch, 1);
        assert_eq!(event.turn_id, index as u64 / 2 + 1);
        assert_eq!(
            serde_json::to_value(&event.message).unwrap(),
            serde_json::to_value(&before.raw[index + 2].message).unwrap()
        );
    }
    let memory = memory(&forked).unwrap();
    assert_eq!(memory.epoch.get(), 1);
    assert_eq!(memory.generation.get(), 0);
    assert!(memory.sources.is_empty());
    assert!(memory.attempts.is_empty());
    assert_eq!(memory.resources, resources());
    let projected = project(
        &forked,
        forked.raw.clone(),
        forked.marker.raw_offset,
        forked.marker.raw_hash.clone(),
    )
    .unwrap();
    assert!(projected.state.structured_coverage.is_none());
    assert!(projected.state.visible_summary_ids.is_empty());
    assert!(
        projected
            .state
            .visible_structured_publication_ids
            .is_empty()
    );
    assert_eq!(memory.ancestors.len(), 1);
    let ancestor = &memory.ancestors[0];
    assert_eq!(ancestor.marker, before.marker);
    assert_eq!(ancestor.generation.get(), 1);
    assert_eq!(ancestor.summary_ids, vec![pending.id]);
    assert_eq!(
        ancestor.structured_ids,
        vec![response.delta.publication.publication_id]
    );
    assert!(forked.state.runs.is_empty());
    assert!(forked.state.requests.is_empty());
    assert!(forked.state.unresolved_approvals.is_empty());
    assert!(forked.state.approval_records.is_empty());
    assert!(forked.state.source_applies.is_empty());
    assert!(forked.state.children.is_empty());
    assert!(forked.state.tasks.is_empty());
    assert_eq!(forked.state.configuration, before.state.configuration);
    assert!(forked.state.draft.text.is_empty());
    let saved = forked.state.workflow.as_ref().unwrap();
    assert!(saved.gates.external_grants.is_empty());
    assert!(saved.gates.verification.as_ref().unwrap().stale);
    assert!(
        !before
            .state
            .workflow
            .as_ref()
            .unwrap()
            .gates
            .verification
            .as_ref()
            .unwrap()
            .stale
    );
    drop(child);
    let reopened = root.open(&before.marker.project_id, &child_id()).unwrap();
    assert_eq!(reopened.snapshot().unwrap().state, forked.state);
    assert_eq!(reopened.snapshot().unwrap().marker, forked.marker);
}

#[test]
fn fork_first_child_checkpoint_finish_and_replay_preserve_inherited_prefix() {
    let (root, mut parent, target) = setup();
    finish(&mut parent, &target, 11);
    with_workflow(&mut parent);
    let mut child = parent.fork_memory(child_id(), "artifact-b").unwrap();
    let inherited = child.snapshot().unwrap();
    let target = accept_text(&mut child, 1, "first child turn");
    let previous = inherited.state.workflow.clone().unwrap();
    let mut candidate = previous.clone();
    candidate.state.phase = crate::workflow::Phase::Review;
    candidate.state.revision += 1;
    let suffix = vec![Message::assistant("child checkpoint")];
    child
        .checkpoint_workflow(
            &target,
            suffix.clone(),
            previous.clone(),
            candidate.clone(),
            None,
        )
        .unwrap();
    child
        .checkpoint_workflow(&target, suffix.clone(), previous, candidate.clone(), None)
        .unwrap();
    // Confirmation performs disk reconciliation; it must use the inherited offset too.
    child
        .confirm_workflow_checkpoint(&target, &suffix, &candidate)
        .unwrap();
    let suffix = vec![
        Message::assistant("child checkpoint"),
        Message::assistant("child result"),
    ];
    child
        .finish_with_messages_and_workflow(
            &target,
            &OperationId::new("main-1").unwrap(),
            Observation::Succeeded,
            ResultId::new("result-1").unwrap(),
            suffix.clone(),
            Some(candidate.clone()),
        )
        .unwrap();
    let completed = child.snapshot().unwrap();
    assert_eq!(completed.state.runs.len(), 1);
    assert_eq!(
        serde_json::to_value(&completed.raw[..20]).unwrap(),
        serde_json::to_value(&inherited.raw).unwrap()
    );
    assert_eq!(completed.raw.len(), 23);
    assert!(completed.raw[20..].iter().all(|event| event.turn_id == 11));
    assert_eq!(completed.raw[20].message.content, "first child turn");
    assert_eq!(completed.raw[22].message.content, "child result");
    drop(child);
    let mut child = root
        .open(&completed.marker.project_id, &child_id())
        .unwrap();
    child
        .finish_with_messages_and_workflow(
            &target,
            &OperationId::new("main-1").unwrap(),
            Observation::Succeeded,
            ResultId::new("result-1").unwrap(),
            suffix,
            Some(candidate),
        )
        .unwrap();
    assert_eq!(child.snapshot().unwrap().marker, completed.marker);
    let second = accept_text(&mut child, 2, "second child turn");
    finish(&mut child, &second, 2);
    assert_eq!(child.snapshot().unwrap().raw.last().unwrap().turn_id, 12);
}

#[test]
fn fork_keeps_ancestor_order_and_refuses_cycles_and_active_or_incomplete_parent() {
    let (_root, mut parent, target) = setup();
    let destination = child_path(&parent, &child_id());
    assert!(matches!(
        parent.fork_memory(child_id(), "artifact-a"),
        Err(StoreError::RunConflict)
    ));
    assert!(!destination.exists());
    finish(&mut parent, &target, 11);
    let child = parent.fork_memory(child_id(), "artifact-a").unwrap();
    let grand_id = SessionId::new("grandchild").unwrap();
    let grandchild = child.fork_memory(grand_id.clone(), "artifact-a").unwrap();
    let snapshot = grandchild.snapshot().unwrap();
    assert_eq!(
        memory(&snapshot)
            .unwrap()
            .ancestors
            .iter()
            .map(|a| a.marker.session_id.as_str())
            .collect::<Vec<_>>(),
        vec!["memory-session", "fork-child"]
    );
    for id in [
        parent.snapshot().unwrap().marker.session_id,
        child_id(),
        grand_id,
    ] {
        assert!(matches!(
            grandchild.fork_memory(id, "artifact-a"),
            Err(StoreError::TargetMismatch)
        ));
    }
    let mut incomplete = parent.snapshot().unwrap();
    incomplete.raw.last_mut().unwrap().message = Message::tool_result("unmatched", "broken result");
    assert!(
        fork::fork_seed(
            &incomplete,
            &SessionId::new("incomplete-child").unwrap(),
            "artifact-a"
        )
        .is_err()
    );
    incomplete.state.configuration.history_mode = HistoryMode::Legacy;
    assert!(
        fork::fork_seed(
            &incomplete,
            &SessionId::new("legacy-child").unwrap(),
            "artifact-a"
        )
        .is_err()
    );
}

#[test]
fn run_details_of_a_fork_use_its_own_turns_after_the_inherited_ones() {
    let (_root, mut parent, target) = setup();
    finish(&mut parent, &target, 11);
    let mut child = parent.fork_memory(child_id(), "artifact-a").unwrap();
    let target = accept(&mut child, 1);
    child
        .finish_with_text(
            &target,
            &OperationId::new("main-1").unwrap(),
            Observation::Succeeded,
            ResultId::new("result-1").unwrap(),
            "answer 101".into(),
        )
        .unwrap();
    let details = child.project_run_details(&target).unwrap();
    let texts: Vec<_> = details
        .entries
        .iter()
        .map(|entry| entry.text.as_str())
        .collect();
    assert_eq!(
        texts,
        [
            child.snapshot().unwrap().state.runs[0].input.text.as_str(),
            "answer 101"
        ]
    );
    assert_eq!(
        parent.project_run_details(&target).unwrap().entries[1].text,
        "answer 1"
    );
}

#[test]
fn fork_memory_snapshot_preserves_host_owned_empty_ancestor_grants() {
    let (_root, mut parent, target) = setup();
    finish(&mut parent, &target, 11);
    let mut child = parent.fork_memory(child_id(), "artifact-a").unwrap();
    let target = accept(&mut child, 1);
    child.bind_memory(&target, resources()).unwrap();
    let snapshot = child.memory_snapshot(&target).unwrap();
    assert_eq!(snapshot.view().unwrap().ancestors.len(), 1);
    assert!(snapshot.view().unwrap().ancestors[0].visible_ids.is_empty());
}

#[test]
fn fork_immutable_count_and_grants_reject_commit_and_persisted_revision_changes() {
    let (root, mut parent, target) = setup();
    finish(&mut parent, &target, 11);
    let mut child = parent.fork_memory(child_id(), "artifact-a").unwrap();
    let before = child.snapshot().unwrap();
    for alter_count in [true, false] {
        let mut altered = before.clone();
        if alter_count {
            altered.state.inherited_turn_count = DecimalU64::new(9);
        } else {
            altered
                .state
                .conversation_memory
                .as_mut()
                .unwrap()
                .ancestors[0]
                .marker
                .raw_hash = content_hash(b"changed");
        }
        assert!(child.commit(altered, false).is_err());
        assert_eq!(child.snapshot().unwrap().marker, before.marker);
    }
    // Persist an otherwise valid adjacent revision with a changed frozen marker.
    let mut altered = before.state.clone();
    altered.session_revision = DecimalU64::new(1);
    altered.conversation_memory.as_mut().unwrap().ancestors[0]
        .marker
        .raw_hash = content_hash(b"changed");
    let path = child.test_directory();
    let mut bytes = fs::read(path.join(disk::SIDECAR)).unwrap();
    bytes.extend(disk::line(&altered).unwrap());
    let mut marker = before.marker.clone();
    marker.session_revision = DecimalU64::new(1);
    marker.sidecar_offset = DecimalU64::new(bytes.len() as u64);
    marker.sidecar_hash = content_hash(&bytes);
    fs::write(path.join(disk::SIDECAR), bytes).unwrap();
    fs::write(
        path.join(disk::MARKER),
        serde_json::to_vec(&marker).unwrap(),
    )
    .unwrap();
    drop(child);
    assert!(matches!(
        root.open(&before.marker.project_id, &child_id()),
        Err(StoreError::Corrupt("継承原文・固定祖先の変更"))
    ));
}

#[test]
fn fork_bounds_counts_ids_and_scope_before_any_child_directory_exists() {
    let (_root, mut parent, target) = setup();
    finish(&mut parent, &target, 11);
    let valid = frozen_child(&parent);
    fork::validate(&valid).unwrap();
    for kind in 0..5 {
        let mut invalid = valid.clone();
        let m = invalid.state.conversation_memory.as_mut().unwrap();
        match kind {
            0 => m.ancestors[0].marker.project_id = ProjectId::new("foreign").unwrap(),
            1 => m.ancestors[0].marker.session_id = child_id(),
            2 => m.ancestors[0].marker.deleted = true,
            3 => m.ancestors.push(m.ancestors[0].clone()),
            4 => {
                m.ancestors[0].generation = DecimalU64::new(1);
                m.ancestors[0].summary_ids.push("invalid".into());
            }
            _ => unreachable!(),
        }
        assert!(fork::validate(&invalid).is_err(), "case {kind}");
    }
    let mut oversized = parent.snapshot().unwrap();
    oversized.state.inherited_turn_count = DecimalU64::new(11);
    assert!(parent.commit(oversized, false).is_err());
    assert!(!child_path(&parent, &child_id()).exists());
    let mut mismatch = parent.snapshot().unwrap();
    mismatch.state.runs.pop();
    assert!(parent.commit(mismatch, false).is_err());
}

#[test]
fn fork_summary_and_structured_capacities_are_independent_and_bounded() {
    let (_root, mut parent, target) = setup();
    finish(&mut parent, &target, 11);
    let mut p = frozen_child(&parent);
    let a = &mut p.state.conversation_memory.as_mut().unwrap().ancestors[0];
    a.generation = DecimalU64::new(65_536);
    a.summary_ids = (1..=65_536)
        .map(|i| format!("strict10-{i}-0000000000000000"))
        .collect();
    a.structured_ids = (1..=65_536)
        .map(|i| format!("structured10-{i}-0000000000000000"))
        .collect();
    fork::validate(&p).unwrap();
    assert!(matches!(
        fork::reserve(&p, 1, &p.marker.raw_hash),
        Err(StoreError::Overflow)
    ));
    let mut one_slot = p.clone();
    let a = &mut one_slot
        .state
        .conversation_memory
        .as_mut()
        .unwrap()
        .ancestors[0];
    a.generation = DecimalU64::new(65_535);
    a.summary_ids.pop();
    a.structured_ids.pop();
    fork::reserve(&one_slot, 1, &one_slot.marker.raw_hash).unwrap();
    let mut too_many = p.clone();
    let a = &mut too_many
        .state
        .conversation_memory
        .as_mut()
        .unwrap()
        .ancestors[0];
    a.generation = DecimalU64::new(65_537);
    a.summary_ids.push("strict10-65537-0000000000000000".into());
    assert!(matches!(
        fork::validate(&too_many),
        Err(StoreError::Overflow)
    ));
    let mut too_deep = frozen_child(&parent);
    let m = too_deep.state.conversation_memory.as_mut().unwrap();
    let original = m.ancestors[0].clone();
    m.ancestors = (0..1_024)
        .map(|i| {
            let mut a = original.clone();
            a.marker.session_id = SessionId::new(format!("ancestor-{i}")).unwrap();
            a
        })
        .collect();
    fork::validate(&too_deep).unwrap();
    m_push_ancestor(&mut too_deep, original);
    assert!(fork::validate(&too_deep).is_err());
}
fn m_push_ancestor(p: &mut Published, a: fork::FrozenAncestor) {
    p.state
        .conversation_memory
        .as_mut()
        .unwrap()
        .ancestors
        .push(a);
}

#[test]
fn fork_initial_publication_failure_never_exposes_an_empty_child() {
    for point in [
        "marker_partial",
        "marker_sync",
        "marker_rename",
        "directory_sync",
    ] {
        let (root, mut parent, target) = setup();
        finish(&mut parent, &target, 11);
        let bytes = owner_bytes(&parent);
        disk::FAIL.with(|fail| fail.set(Some(point)));
        let result = parent.fork_memory(child_id(), "artifact-a");
        disk::FAIL.with(|fail| fail.set(None));
        assert!(result.is_err());
        assert_eq!(owner_bytes(&parent), bytes);
        let result = root.open(&parent.snapshot().unwrap().marker.project_id, &child_id());
        if point == "directory_sync" {
            let snapshot = result.unwrap().snapshot().unwrap();
            assert_eq!(snapshot.raw.len(), 20);
            assert_eq!(snapshot.state.inherited_turn_count.get(), 10);
            assert_eq!(memory(&snapshot).unwrap().ancestors.len(), 1);
            assert!(snapshot.state.runs.is_empty());
        } else {
            assert!(result.is_err());
        }
        assert!(parent.fork_memory(child_id(), "artifact-a").is_err());
    }
}

#[test]
fn legacy_sidecar_without_inherited_metadata_reopens_and_new_run_completes() {
    let (root, mut parent, target) = setup();
    finish(&mut parent, &target, 11);
    let path = parent.test_directory();
    let before = parent.snapshot().unwrap();
    let bytes = fs::read(path.join(disk::SIDECAR)).unwrap();
    let mut old_bytes = Vec::new();
    for line in bytes.split_inclusive(|b| *b == b'\n') {
        let mut state: serde_json::Value = serde_json::from_slice(line).unwrap();
        state
            .as_object_mut()
            .unwrap()
            .remove("inherited_turn_count");
        old_bytes.extend(disk::line(&state).unwrap());
    }
    let mut marker = before.marker;
    marker.sidecar_offset = DecimalU64::new(old_bytes.len() as u64);
    marker.sidecar_hash = content_hash(&old_bytes);
    fs::write(path.join(disk::SIDECAR), old_bytes).unwrap();
    fs::write(
        path.join(disk::MARKER),
        serde_json::to_vec(&marker).unwrap(),
    )
    .unwrap();
    drop(parent);
    let mut parent = root.open(&marker.project_id, &marker.session_id).unwrap();
    assert_eq!(
        parent.snapshot().unwrap().state.inherited_turn_count.get(),
        0
    );
    let details = parent.project_run_details(&target).unwrap();
    assert_eq!(details.entries[1].text, "answer 11");
    let target = accept(&mut parent, 12);
    finish(&mut parent, &target, 12);
    assert_eq!(parent.snapshot().unwrap().raw.last().unwrap().turn_id, 12);
    let details = parent.project_run_details(&target).unwrap();
    assert_eq!(details.entries[1].text, "answer 12");
}

#[tokio::test]
async fn fork_strict_summary_publishes_with_frozen_parent_and_reopens_sources() {
    let (root, mut parent, target) = setup();
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("memory.sqlite3");
    let (work, first) = pending(&mut parent, &target).await;
    parent
        .publish_memory(&target, &work, &first, &database)
        .unwrap();
    finish(&mut parent, &target, 11);
    let parent_marker = parent.snapshot().unwrap().marker;
    let parent_path = parent.test_directory();
    let parent_bytes = owner_bytes(&parent);
    let mut child = parent.fork_memory(child_id(), "artifact-a").unwrap();
    drop(parent);
    let target = accept_text(&mut child, 1, "Child branch keeps Rigel");
    child.bind_memory(&target, resources()).unwrap();
    let snapshot = child.memory_snapshot(&target).unwrap();
    assert_eq!(snapshot.view().unwrap().ancestors.len(), 1);
    let parent_uri = format!("conversation://{}", first.source.id);
    assert!(
        child
            .read_memory_source(&target, &database, &parent_uri)
            .unwrap()
            .contains("Rigel")
    );
    let (work, local) = pending(&mut child, &target).await;
    child
        .publish_memory(&target, &work, &local, &database)
        .unwrap();
    let published = child.memory_snapshot(&target).unwrap();
    assert_eq!(
        published.view().unwrap().visible_ids,
        vec![local.id.clone()]
    );
    assert_eq!(
        published.view().unwrap().ancestors[0].visible_ids,
        vec![first.id]
    );
    let local_uri = format!("conversation://{}", local.source.id);
    let local_page = child
        .read_memory_source(&target, &database, &local_uri)
        .unwrap();
    assert!(local_page.contains("Rigel"));
    for (name, bytes) in [disk::RAW, disk::SIDECAR, disk::MARKER]
        .into_iter()
        .zip(parent_bytes)
    {
        assert_eq!(fs::read(parent_path.join(name)).unwrap(), bytes);
    }
    drop(child);
    let mut child = root.open(&parent_marker.project_id, &child_id()).unwrap();
    // Recovering an active run stops it; a new accepted run reads the same grants.
    let next = accept_text(&mut child, 2, "Read preserved originals");
    child.bind_memory(&next, resources()).unwrap();
    assert_eq!(
        child
            .read_memory_source(&next, &database, &local_uri)
            .unwrap(),
        local_page
    );
    assert!(
        child
            .read_memory_source(&next, &database, &parent_uri)
            .unwrap()
            .contains("Rigel")
    );
}

#[tokio::test]
async fn fork_strict_publication_rejects_busy_and_modified_v2_parent_before_marker() {
    let (_root, mut parent, target) = setup();
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.sqlite3");
    let (work, first) = pending(&mut parent, &target).await;
    parent
        .publish_memory(&target, &work, &first, &database)
        .unwrap();
    finish(&mut parent, &target, 11);
    let mut child = parent.fork_memory(child_id(), "artifact-a").unwrap();
    let target = accept_text(&mut child, 1, "Rigel child");
    child.bind_memory(&target, resources()).unwrap();
    let (work, local) = pending(&mut child, &target).await;
    let before = child.snapshot().unwrap().marker;
    assert!(matches!(
        child.publish_memory(&target, &work, &local, &database),
        Err(StoreError::Busy)
    ));
    assert_eq!(child.snapshot().unwrap().marker, before);
    drop(parent);
    let db = rusqlite::Connection::open(&database).unwrap();
    db.execute(
        "UPDATE conversation_sources_v2 SET raw_hash=?1 WHERE session_id='memory-session'",
        ["f".repeat(64)],
    )
    .unwrap();
    assert!(
        child
            .publish_memory(&target, &work, &local, &database)
            .is_err()
    );
    assert_eq!(child.snapshot().unwrap().marker, before);
    db.execute(
        "UPDATE conversation_sources_v2 SET raw_hash=?1 WHERE session_id='memory-session'",
        [&first.source.raw_hash],
    )
    .unwrap();
    db.execute(
        "UPDATE conversation_vectors_v2 SET revision='other' WHERE session_id='memory-session'",
        [],
    )
    .unwrap();
    assert!(
        child
            .publish_memory(&target, &work, &local, &database)
            .is_err()
    );
    assert_eq!(child.snapshot().unwrap().marker, before);
    db.execute(
        "UPDATE conversation_vectors_v2 SET revision='fixed' WHERE session_id='memory-session'",
        [],
    )
    .unwrap();
    child
        .publish_memory(&target, &work, &local, &database)
        .unwrap();
    assert_eq!(
        child
            .memory_snapshot(&target)
            .unwrap()
            .view()
            .unwrap()
            .visible_ids,
        vec![local.id]
    );
}

#[tokio::test]
async fn fork_structured_child_overlays_parent_and_reuses_response_after_marker_failure() {
    use crate::conversation_memory::structured::{
        PublishedResolver, validate_response_with_resolver,
    };
    use polaris_memory::structured::{EntityKind, FactStatus};
    let (root, mut parent, target) = setup_mode(
        "memory-project",
        "memory-session",
        HistoryMode::Structured10,
    );
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.sqlite3");
    let (work, first, original) = structured_pending(&mut parent, &target).await;
    parent
        .publish_structured_memory(&target, &work, &first, &original, &database)
        .unwrap();
    finish(&mut parent, &target, 11);
    let parent_bytes = owner_bytes(&parent);
    let parent_path = parent.test_directory();
    let project = parent.coordinates().0.clone();
    let mut child = parent.fork_memory(child_id(), "artifact-a").unwrap();
    drop(parent);
    let text = "Rename Rigel to Aster. Withdraw the fixture decision; no replacement is decided.";
    let target = accept_text(&mut child, 1, text);
    child.bind_memory(&target, resources()).unwrap();
    let snapshot = child.memory_snapshot(&target).unwrap();
    let work = child.begin_summary(&target, &snapshot, 1).unwrap();
    let intent = work.structured_intent.as_ref().unwrap();
    let source = intent.sources.iter().find(|s| s.content == text).unwrap();
    let body = serde_json::json!({
        "summary":{"facts":[],"decisions":[],"constraints":[],"corrections":[text],"open_items":[],"source_turn_ids":[1]},
        "processed_source_ids":intent.sources.iter().map(|s| &s.source_id).collect::<Vec<_>>(),
        "entity_candidates":[{"source_id":source.source_id,"evidence_span":{"start":0,"length":text.len()},"evidence_text":text,"ordinal":0,"speech_act":"correct","kind":"product","name":"Aster","subject_project":"memory-project","renamed_from":"Rigel"}],
        "fact_deltas":[{"source_id":source.source_id,"evidence_span":{"start":0,"length":text.len()},"evidence_text":text,"ordinal":0,"speech_act":"withdraw","entity_kind":"product","entity_name":"Aster","subject_project":"memory-project","fact_kind":"fixture","value":null,"unit":"none","condition":"","relation":"withdraw"}]
    }).to_string();
    let db = MemoryStore::open(&database).unwrap();
    let prior = snapshot.structured_view().unwrap();
    let response = frozen::with_published_receipts(&child, &db, |_| {
        validate_response_with_resolver(
            &work.source,
            1,
            intent,
            &body,
            &PublishedResolver {
                memory: &db,
                view: &prior,
            },
        )
        .map_err(Into::into)
    })
    .unwrap();
    assert!(response.delta.entities.is_empty());
    assert_eq!(
        response.delta.entity_revisions[0].entity_id,
        original.delta.entities[0].entity_id
    );
    child
        .save_structured_response(&target, &work, &response)
        .unwrap();
    let saved = SavedStructuredResponse {
        summary: response.summary.clone(),
        delta: response.delta,
        coverage: response.coverage,
    };
    let pending = history()
        .embed_summary(&work.source, 1, response.summary)
        .await
        .unwrap();
    disk::FAIL.with(|fail| fail.set(Some("marker_rename")));
    assert!(
        child
            .publish_structured_memory(&target, &work, &pending, &saved, &database)
            .is_err()
    );
    disk::FAIL.with(|fail| fail.set(None));
    db.verify_structured_delta(&saved.delta).unwrap();
    child.recover().unwrap();
    let snapshot = child.memory_snapshot(&target).unwrap();
    assert!(
        snapshot
            .structured_view()
            .unwrap()
            .visible_publication_ids
            .is_empty()
    );
    let recovered = child.begin_summary(&target, &snapshot, 1).unwrap();
    assert!(!recovered.newly_started);
    assert_eq!(recovered.structured_response.as_ref(), Some(&saved));
    let checked = std::rc::Rc::new(std::cell::Cell::new(false));
    let observed = checked.clone();
    let reader_root = root.clone();
    let reader_project = project.clone();
    frozen::before_marker(move || {
        assert!(matches!(
            reader_root.open(&reader_project, &SessionId::new("memory-session").unwrap()),
            Err(StoreError::Busy)
        ));
        observed.set(true);
    });
    child
        .publish_structured_memory(&target, &recovered, &pending, &saved, &database)
        .unwrap();
    assert!(checked.get());
    drop(
        root.open(&project, &SessionId::new("memory-session").unwrap())
            .unwrap(),
    );
    let view = child
        .memory_snapshot(&target)
        .unwrap()
        .structured_view()
        .unwrap();
    let entity = &original.delta.entities[0].entity_id;
    assert_eq!(
        db.resolve_structured_entity(&view, EntityKind::Product, "Aster", "memory-project")
            .unwrap(),
        Some(entity.clone())
    );
    assert_eq!(
        db.structured_fact_revision(&view, &original.delta.facts[0].fact_id)
            .unwrap()
            .unwrap()
            .status,
        FactStatus::Superseded
    );
    let uri = crate::conversation_memory::structured::sources::source_uri(
        &saved.delta.publication.publication_id,
        &source.source_id,
    )
    .unwrap();
    let page = child.read_memory_source(&target, &database, &uri).unwrap();
    assert!(page.contains(text));
    for (name, bytes) in [disk::RAW, disk::SIDECAR, disk::MARKER]
        .into_iter()
        .zip(parent_bytes)
    {
        assert_eq!(fs::read(parent_path.join(name)).unwrap(), bytes);
    }
    let snapshot = child.memory_snapshot(&target).unwrap();
    let related = child
        .prepare_related_memory(&target, &snapshot, &database, related_query_embedding())
        .unwrap();
    assert!(
        related
            .semantic
            .iter()
            .any(|hit| hit.pending.id == first.id)
    );
    assert!(
        related
            .keyword
            .iter()
            .any(|hit| hit.pending.id == pending.id)
    );
    let mut parent = root
        .open(&project, &SessionId::new("memory-session").unwrap())
        .unwrap();
    let next = accept_text(&mut parent, 12, "Nebula is a later parent-only product.");
    parent.bind_memory(&next, resources()).unwrap();
    let snapshot = parent.memory_snapshot(&next).unwrap();
    let later_work = parent.begin_summary(&next, &snapshot, 2).unwrap();
    let intent = later_work.structured_intent.as_ref().unwrap();
    let source = intent
        .sources
        .iter()
        .find(|s| s.content.contains("Nebula"))
        .unwrap();
    let body = serde_json::json!({
        "summary":{"facts":["Nebula parent-only"],"decisions":[],"constraints":[],"corrections":[],"open_items":[],"source_turn_ids":[2]},
        "processed_source_ids":intent.sources.iter().map(|s| &s.source_id).collect::<Vec<_>>(),
        "entity_candidates":[{"source_id":source.source_id,"evidence_span":{"start":0,"length":6},"evidence_text":"Nebula","ordinal":0,"speech_act":"assert","kind":"product","name":"Nebula","subject_project":"memory-project"}],"fact_deltas":[]
    }).to_string();
    let response = crate::conversation_memory::structured::validate_response(
        &later_work.source,
        2,
        intent,
        &body,
    )
    .unwrap();
    parent
        .save_structured_response(&next, &later_work, &response)
        .unwrap();
    let later = SavedStructuredResponse {
        summary: response.summary.clone(),
        delta: response.delta,
        coverage: response.coverage,
    };
    let later_pending = history()
        .embed_summary(&later_work.source, 2, response.summary)
        .await
        .unwrap();
    parent
        .publish_structured_memory(&next, &later_work, &later_pending, &later, &database)
        .unwrap();
    drop(parent);
    assert!(
        db.resolve_structured_entity(&view, EntityKind::Product, "Nebula", "memory-project")
            .unwrap()
            .is_none()
    );
    let later_uri = crate::conversation_memory::structured::sources::source_uri(
        &later.delta.publication.publication_id,
        &source.source_id,
    )
    .unwrap();
    assert!(
        child
            .read_memory_source(&target, &database, &later_uri)
            .is_err()
    );
    assert_eq!(
        child.read_memory_source(&target, &database, &uri).unwrap(),
        page
    );
    drop(child);
    let mut child = root.open(&project, &child_id()).unwrap();
    let target = accept_text(&mut child, 2, "Aster fixture status");
    child.bind_memory(&target, resources()).unwrap();
    assert_eq!(
        child.read_memory_source(&target, &database, &uri).unwrap(),
        page
    );
}

#[tokio::test]
async fn fork_publication_rejects_forgotten_parent_even_without_any_visible_ids() {
    let (_root, mut parent, target) = setup();
    finish(&mut parent, &target, 11);
    let mut child = parent.fork_memory(child_id(), "artifact-a").unwrap();
    drop(parent);
    let target = accept(&mut child, 1);
    child.bind_memory(&target, resources()).unwrap();
    let (work, pending) = pending(&mut child, &target).await;
    let dir = tempfile::tempdir().unwrap();
    let database = dir.path().join("memory.sqlite3");
    let mut db = MemoryStore::open(&database).unwrap();
    db.delete_session("memory-project", "memory-session")
        .unwrap();
    let before = child.snapshot().unwrap().marker;
    assert!(
        child
            .publish_memory(&target, &work, &pending, &database)
            .is_err()
    );
    assert_eq!(child.snapshot().unwrap().marker, before);
    assert!(
        child
            .memory_snapshot(&target)
            .unwrap()
            .view()
            .unwrap()
            .visible_ids
            .is_empty()
    );
}
