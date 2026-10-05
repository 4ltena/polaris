//! Management-effect ownership, journal and recovery regression tests.

use super::*;
use crate::project_mutation::{ProjectMutationLeaseV1, ProjectMutationOwnerV1};
use std::{
    collections::HashSet,
    fs,
    io::Write,
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    str::FromStr,
    sync::{Arc, Barrier},
};

struct Fixture {
    _root: tempfile::TempDir,
    _owner: ProjectMutationOwnerV1,
    lease: ProjectMutationLeaseV1,
    store: ManagementEffectStoreV1,
}

#[test]
fn m3e_runtime_activity_effects_preserve_project_and_inventory_boundaries() {
    let Fixture {
        _root,
        _owner: owner,
        lease,
        store,
    } = Fixture::new();
    drop(lease);
    let activity = owner.try_activity().unwrap();
    let sibling = owner.try_activity().unwrap();
    assert!(matches!(
        owner.try_mutation(),
        Err(crate::project_mutation::ProjectMutationError::Busy)
    ));
    let runtime_request = |number, inventory: &str| {
        let mut request = request(store.project_scope_hash(), number);
        request.inventory = ManagementInventoryIdV1::parse(inventory).unwrap();
        request.scope.permission = ManagementPermissionScopeV1::LocalSession;
        request
    };
    for (number, inventory) in [(1, "D10"), (2, "D12"), (3, "D21"), (4, "D23"), (5, "D24")] {
        let PrepareManagementEffectOutcomeV1::Prepared(prepared) = store
            .prepare_runtime(
                &activity,
                store.cursor().unwrap(),
                runtime_request(number, inventory),
            )
            .unwrap()
        else {
            panic!("fresh runtime effect");
        };
        let started = store.start_runtime(&activity, &prepared).unwrap();
        store
            .complete_started_runtime(&activity, &started, hash("runtime receipt"))
            .unwrap();
    }
    let before = fs::read(store.test_journal_path()).unwrap();
    let other_root = tempfile::tempdir().unwrap();
    let other = ProjectMutationOwnerV1::at_store_root(other_root.path()).unwrap();
    let other_activity = other.try_activity().unwrap();
    assert!(matches!(
        store.prepare_runtime(
            &other_activity,
            store.cursor().unwrap(),
            runtime_request(6, "D12")
        ),
        Err(ManagementEffectStoreError::WrongProjectLease)
    ));
    let mut system = runtime_request(6, "D12");
    system.scope.permission = ManagementPermissionScopeV1::System;
    assert!(matches!(
        store.prepare_runtime(&activity, store.cursor().unwrap(), system),
        Err(ManagementEffectStoreError::InvalidInventoryContract)
    ));
    let project = request(store.project_scope_hash(), 6);
    assert!(matches!(
        store.prepare_runtime(&activity, store.cursor().unwrap(), project),
        Err(ManagementEffectStoreError::InvalidInventoryContract)
    ));
    let mut source_apply = request(store.project_scope_hash(), 6);
    source_apply.inventory = ManagementInventoryIdV1::parse("C12").unwrap();
    assert!(matches!(
        store.prepare_runtime(&activity, store.cursor().unwrap(), source_apply),
        Err(ManagementEffectStoreError::InvalidInventoryContract)
    ));
    assert_eq!(fs::read(store.test_journal_path()).unwrap(), before);

    let PrepareManagementEffectOutcomeV1::Prepared(declined) = store
        .prepare_runtime(
            &activity,
            store.cursor().unwrap(),
            runtime_request(6, "D21"),
        )
        .unwrap()
    else {
        panic!("fresh declined effect");
    };
    store
        .deny_runtime(&activity, &declined, ManagementDenialReasonV1::UserDeclined)
        .unwrap();
    let PrepareManagementEffectOutcomeV1::Prepared(pending) = store
        .prepare_runtime(
            &activity,
            store.cursor().unwrap(),
            runtime_request(7, "D12"),
        )
        .unwrap()
    else {
        panic!("fresh pending effect");
    };
    let started = store.start_runtime(&activity, &pending).unwrap();
    store
        .mark_outcome_unknown_runtime(
            &activity,
            &started,
            ManagementUnknownReasonV1::DetachedEffect,
        )
        .unwrap();
    assert!(store.blocker().unwrap().is_some());
    assert!(matches!(
        store.prepare_runtime(
            &activity,
            store.cursor().unwrap(),
            runtime_request(8, "D21")
        ),
        Err(ManagementEffectStoreError::UnresolvedEffect)
    ));
    assert!(matches!(
        owner.try_mutation(),
        Err(crate::project_mutation::ProjectMutationError::Busy)
    ));
    drop(activity);
    assert!(matches!(
        owner.try_mutation(),
        Err(crate::project_mutation::ProjectMutationError::Busy)
    ));
    drop(sibling);
    let lease = owner.try_mutation().unwrap();
    let recovered = ManagementEffectStoreV1::test_open(
        &_root.path().join("state"),
        lease.store_root().to_path_buf(),
        store.project_scope_hash(),
        &lease,
        ManagementEffectLimitsV1::default(),
    );
    // Recovery is still EX-only; opening must retain the unresolved outcome.
    assert!(recovered.unwrap().blocker().unwrap().is_some());
}

#[test]
fn m3e_runtime_activity_capacity_never_seals_or_accepts_replaced_binding() {
    let Fixture {
        _root,
        _owner: owner,
        lease,
        store,
    } = Fixture::with_limits(ManagementEffectLimitsV1 {
        max_effects: 1,
        ..ManagementEffectLimitsV1::default()
    });
    let root = lease.store_root().to_path_buf();
    drop(lease);
    let activity = owner.try_activity().unwrap();
    let runtime_request = |number| {
        let mut request = request(store.project_scope_hash(), number);
        request.inventory = ManagementInventoryIdV1::parse("D12").unwrap();
        request.scope.permission = ManagementPermissionScopeV1::LocalSession;
        request
    };
    let PrepareManagementEffectOutcomeV1::Prepared(prepared) = store
        .prepare_runtime(&activity, store.cursor().unwrap(), runtime_request(1))
        .unwrap()
    else {
        panic!("fresh effect");
    };
    store
        .deny_runtime(&activity, &prepared, ManagementDenialReasonV1::UserDeclined)
        .unwrap();
    let cursor = store.cursor().unwrap();
    let before = fs::read(store.test_journal_path()).unwrap();
    assert!(matches!(
        store.prepare_runtime(&activity, cursor, runtime_request(2)),
        Err(ManagementEffectStoreError::StoreFull)
    ));
    assert_eq!(store.cursor().unwrap(), cursor);
    assert_eq!(fs::read(store.test_journal_path()).unwrap(), before);
    let held = root.with_file_name("held-runtime-lease");
    fs::rename(&root, &held).unwrap();
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        store.prepare_runtime(&activity, cursor, runtime_request(2)),
        Err(ManagementEffectStoreError::WrongProjectLease)
    ));
    assert_eq!(fs::read(store.test_journal_path()).unwrap(), before);
}

#[test]
fn m2a_deterministic_seal_restarts_namespace_stops_without_retry_growth() {
    for point in 1..=9 {
        let fixture = Fixture::new();
        let (_, prepared) = prepared(&fixture, 1);
        let started = fixture.store.start(&fixture.lease, &prepared).unwrap();
        let original = started.verified_event().clone();
        fixture.store.test_fail_seal_once_at(point);
        assert!(
            fixture.store.test_joined_seal(&fixture.lease).is_err(),
            "stop {point}"
        );
        let state = fixture._root.path().join("state");
        let before = m2a_file_inventory(&state);
        let ManagementEffectInspectOpenV1::Present(inspected) = fixture
            .store
            .test_inspect(&fixture.lease, ManagementEffectLimitsV1::default())
            .unwrap()
        else {
            panic!("missing store");
        };
        assert_eq!(before, m2a_file_inventory(&state));
        let generation = inspected.cursor().generation;
        assert_eq!(generation, if point >= 8 { 1 } else { 0 });
        let snapshot = inspected
            .effects()
            .find(|e| e.request.operation_id == operation_id(1))
            .unwrap();
        assert_eq!(
            inspected
                .latest_event_evidence(operation_id(1), snapshot)
                .unwrap(),
            Some(original.clone())
        );
        drop(inspected);
        if generation == 0 {
            fixture.store.test_fail_seal_once_at(point);
            assert!(fixture.store.test_joined_seal(&fixture.lease).is_err());
            assert_eq!(
                before,
                m2a_file_inventory(&state),
                "retry changed bytes at {point}"
            );
            fixture.store.test_joined_seal(&fixture.lease).unwrap();
        }
        assert_eq!(fixture.store.cursor().unwrap().generation, 1);
        let generation_path = state.join("management-effects-v1/generations");
        let names: Vec<_> = fs::read_dir(&generation_path)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, [std::ffi::OsString::from("1")]);
        assert!(
            !state
                .join("management-effects-v1/.CURRENT-1.v1.tmp")
                .exists()
        );
        let effect = fixture.store.effect(operation_id(1)).unwrap().unwrap();
        assert_eq!(
            fixture
                .store
                .latest_event_evidence(operation_id(1), &effect)
                .unwrap(),
            Some(original)
        );
    }
}

#[test]
fn m2a_deterministic_seal_accepts_exact_partial_prefix_and_blocks_foreign_prefix() {
    for index in 0..3 {
        for foreign in [false, true] {
            let fixture = Fixture::new();
            let (_, prepared) = prepared(&fixture, 1);
            fixture.store.start(&fixture.lease, &prepared).unwrap();
            fixture
                .store
                .test_joined_seal_prefix(&fixture.lease, index, foreign)
                .unwrap();
            let state = fixture._root.path().join("state");
            let before = m2a_file_inventory(&state);
            let inspected = fixture
                .store
                .test_inspect(&fixture.lease, ManagementEffectLimitsV1::default());
            if foreign {
                assert!(
                    matches!(inspected, Err(ManagementEffectStoreError::ForeignPrefix)),
                    "artifact {index}"
                );
                assert!(matches!(
                    fixture.store.test_joined_seal(&fixture.lease),
                    Err(ManagementEffectStoreError::ForeignPrefix)
                ));
                assert_eq!(before, m2a_file_inventory(&state));
            } else {
                drop(inspected.unwrap());
                assert_eq!(before, m2a_file_inventory(&state));
                fixture.store.test_joined_seal(&fixture.lease).unwrap();
                assert_eq!(fixture.store.cursor().unwrap().generation, 1);
            }
        }
    }
}

#[test]
fn m2a_deterministic_seal_blocks_partial_final_and_conflicting_temporary_names() {
    for case in 0..4 {
        let fixture = Fixture::new();
        let (_, prepared) = prepared(&fixture, 1);
        fixture.store.start(&fixture.lease, &prepared).unwrap();
        fixture
            .store
            .test_joined_seal_prefix(&fixture.lease, 0, false)
            .unwrap();
        let state = fixture._root.path().join("state");
        let root = state.join("management-effects-v1");
        match case {
            0 => fs::rename(
                root.join("generations/.seal-1.v1.tmp"),
                root.join("generations/1"),
            )
            .unwrap(),
            1 => {
                fs::create_dir(root.join("generations/1")).unwrap();
                fs::set_permissions(
                    root.join("generations/1"),
                    fs::Permissions::from_mode(0o700),
                )
                .unwrap();
            }
            2 => {
                fs::write(root.join(".CURRENT-2.v1.tmp"), b"{}").unwrap();
                fs::set_permissions(
                    root.join(".CURRENT-2.v1.tmp"),
                    fs::Permissions::from_mode(0o600),
                )
                .unwrap();
            }
            3 => {
                fs::write(root.join(".CURRENT.12345.tmp"), b"{}").unwrap();
                fs::set_permissions(
                    root.join(".CURRENT.12345.tmp"),
                    fs::Permissions::from_mode(0o600),
                )
                .unwrap();
            }
            _ => unreachable!(),
        }
        let before = m2a_file_inventory(&state);
        assert!(
            fixture
                .store
                .test_inspect(&fixture.lease, ManagementEffectLimitsV1::default())
                .is_err()
        );
        assert!(fixture.store.test_joined_seal(&fixture.lease).is_err());
        assert_eq!(before, m2a_file_inventory(&state), "case {case}");
    }
}

#[test]
fn operation_id_has_one_stable_child_session_identity() {
    let first = operation_id(1);
    let second = operation_id(2);
    let child = first.child_session_id();
    assert_eq!(child, first.child_session_id());
    assert_ne!(child, second.child_session_id());
    assert_eq!(child.len(), 36);
    assert_eq!(&child[14..15], "4");
    assert!(matches!(&child[19..20], "8" | "9" | "a" | "b"));
}

#[test]
fn m2a_event_evidence_compares_generation_hash_and_entire_typed_body() {
    let fixture = Fixture::new();
    let (_, prepared) = prepared(&fixture, 1);
    let proof = prepared.verified_event().clone();
    assert_eq!(
        proof.event,
        ManagementEffectEventV1::Prepared {
            request: prepared.request().clone()
        }
    );
    assert_eq!(
        fixture
            .store
            .event_evidence(prepared.operation_id(), proof.evidence, &proof.event)
            .unwrap(),
        Some(proof.clone())
    );
    let before = m2a_file_inventory(&fixture._root.path().join("state"));
    for field in 0..10 {
        let mut other = prepared.request().clone();
        match field {
            0 => other.operation_id = operation_id(2),
            1 => other.inventory = ManagementInventoryIdV1::parse("T09").unwrap(),
            2 => other.scope.project = hash("other project"),
            3 => other.scope.session = Some(hash("other session")),
            4 => other.scope.generation += 1,
            5 => other.scope.revision += 1,
            6 => other.scope.permission = ManagementPermissionScopeV1::System,
            7 => other.scope.account_binding = Some(hash("other account")),
            8 => other.request_hash = hash("other request"),
            9 => other.rewind = ManagementRewindDispositionV1::CapturedRestorable,
            _ => unreachable!(),
        }
        assert!(
            matches!(
                fixture.store.event_evidence(
                    prepared.operation_id(),
                    proof.evidence,
                    &ManagementEffectEventV1::Prepared { request: other }
                ),
                Err(ManagementEffectStoreError::CorruptJournal)
            ),
            "field {field}"
        );
    }
    assert_eq!(
        before,
        m2a_file_inventory(&fixture._root.path().join("state"))
    );
    let mut wrong_hash = proof.evidence;
    wrong_hash.event_hash = hash("foreign envelope");
    assert!(matches!(
        fixture
            .store
            .event_evidence(prepared.operation_id(), wrong_hash, &proof.event),
        Err(ManagementEffectStoreError::CorruptJournal)
    ));
    assert!(matches!(
        fixture
            .store
            .event_evidence(operation_id(2), proof.evidence, &proof.event),
        Err(ManagementEffectStoreError::CorruptJournal)
    ));
    let denied = fixture
        .store
        .deny(
            &fixture.lease,
            &prepared,
            ManagementDenialReasonV1::UserDeclined,
        )
        .unwrap();
    let terminal = denied.verified_event();
    let alternate = ManagementEffectEventV1::Denied {
        operation_id: denied.operation_id(),
        reason: ManagementDenialReasonV1::StaleRequest,
    };
    assert!(matches!(
        fixture
            .store
            .event_evidence(denied.operation_id(), terminal.evidence, &alternate),
        Err(ManagementEffectStoreError::CorruptJournal)
    ));
    let snapshot = fixture
        .store
        .effect(denied.operation_id())
        .unwrap()
        .unwrap();
    assert_eq!(
        fixture
            .store
            .latest_event_evidence(denied.operation_id(), &snapshot)
            .unwrap()
            .as_ref(),
        Some(terminal)
    );
    let mut changed = snapshot.clone();
    changed.request.scope.revision += 1;
    assert!(matches!(
        fixture
            .store
            .latest_event_evidence(denied.operation_id(), &changed),
        Err(ManagementEffectStoreError::CorruptJournal)
    ));
    assert_eq!(
        fixture
            .store
            .deny(
                &fixture.lease,
                &prepared,
                ManagementDenialReasonV1::UserDeclined
            )
            .unwrap()
            .verified_event(),
        terminal
    );
}

#[test]
fn m2a_event_evidence_preserves_original_generation_through_pending_rescue() {
    let mut fixture = Fixture::new();
    let (_, first) = prepared(&fixture, 1);
    fixture
        .store
        .deny(
            &fixture.lease,
            &first,
            ManagementDenialReasonV1::UserDeclined,
        )
        .unwrap();
    let (_, second) = prepared(&fixture, 2);
    let original = second.verified_event().clone();
    assert_eq!(
        (original.evidence.generation, original.evidence.sequence),
        (0, 3)
    );
    let state_dir = fixture._root.path().join("state");
    fixture.store = ManagementEffectStoreV1::test_open(
        &state_dir,
        fs::canonicalize(state_dir.join("rewind-workspace")).unwrap(),
        hash("project-a"),
        &fixture.lease,
        ManagementEffectLimitsV1 {
            max_records: 3,
            ..ManagementEffectLimitsV1::default()
        },
    )
    .unwrap();
    let rescued = fixture
        .store
        .prepared_effect(operation_id(2))
        .unwrap()
        .unwrap();
    assert_eq!(rescued.generation(), 1);
    assert_eq!(rescued.verified_event(), &original);
    let started = fixture.store.start(&fixture.lease, &rescued).unwrap();
    let unknown = fixture
        .store
        .mark_outcome_unknown(
            &fixture.lease,
            &started,
            ManagementUnknownReasonV1::DetachedEffect,
        )
        .unwrap();
    let completed = fixture
        .store
        .complete_unknown(&fixture.lease, &unknown, hash("final readback"))
        .unwrap();
    let terminal = completed.verified_event();
    assert_eq!(
        (terminal.evidence.generation, terminal.evidence.sequence),
        (1, 3)
    );
    assert_eq!(
        fixture
            .store
            .event_evidence(operation_id(2), original.evidence, &original.event)
            .unwrap(),
        Some(original)
    );
    let snapshot = fixture.store.effect(operation_id(2)).unwrap().unwrap();
    assert_eq!(
        fixture
            .store
            .latest_event_evidence(operation_id(2), &snapshot)
            .unwrap()
            .as_ref(),
        Some(terminal)
    );
}

fn m2a_file_inventory(root: &std::path::Path) -> std::collections::BTreeMap<PathBuf, Vec<u8>> {
    fn walk(
        root: &std::path::Path,
        directory: &std::path::Path,
        out: &mut std::collections::BTreeMap<PathBuf, Vec<u8>>,
    ) {
        for entry in fs::read_dir(directory).unwrap() {
            let path = entry.unwrap().path();
            let meta = fs::symlink_metadata(&path).unwrap();
            assert!(!meta.file_type().is_symlink());
            if meta.is_dir() {
                out.insert(path.strip_prefix(root).unwrap().to_owned(), Vec::new());
                walk(root, &path, out);
            } else {
                out.insert(
                    path.strip_prefix(root).unwrap().to_owned(),
                    fs::read(path).unwrap(),
                );
            }
        }
    }
    let mut out = std::collections::BTreeMap::new();
    walk(root, root, &mut out);
    out
}

#[test]
fn m2a_inspect_open_preserves_absent_store_and_every_complete_or_torn_lifecycle_state() {
    let root = tempfile::tempdir().unwrap();
    fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
    let owner =
        ProjectMutationOwnerV1::at_store_root(&root.path().join("rewind-workspace")).unwrap();
    let lease = owner.try_mutation().unwrap();
    let before = m2a_file_inventory(root.path());
    assert!(matches!(
        ManagementEffectStoreV1::inspect_open_at_state_dir(root.path(), root.path(), &lease)
            .unwrap(),
        ManagementEffectInspectOpenV1::Absent
    ));
    assert_eq!(before, m2a_file_inventory(root.path()));

    for case in 0..=5 {
        for torn in [false, true] {
            let fixture = Fixture::new();
            let mut expected = None;
            if case > 0 {
                let (_, prepared) = prepared(&fixture, 1);
                expected = Some(prepared.verified_event().clone());
                if case == 5 {
                    expected = Some(
                        fixture
                            .store
                            .deny(
                                &fixture.lease,
                                &prepared,
                                ManagementDenialReasonV1::UserDeclined,
                            )
                            .unwrap()
                            .verified_event()
                            .clone(),
                    );
                } else if case > 1 {
                    let started = fixture.store.start(&fixture.lease, &prepared).unwrap();
                    expected = Some(started.verified_event().clone());
                    if case > 2 {
                        let unknown = fixture
                            .store
                            .mark_outcome_unknown(
                                &fixture.lease,
                                &started,
                                ManagementUnknownReasonV1::InterruptedAfterStart,
                            )
                            .unwrap();
                        expected = Some(unknown.verified_event().clone());
                        if case == 4 {
                            expected = Some(
                                fixture
                                    .store
                                    .complete_unknown(
                                        &fixture.lease,
                                        &unknown,
                                        hash("complete readback"),
                                    )
                                    .unwrap()
                                    .verified_event()
                                    .clone(),
                            );
                        }
                    }
                }
            }
            if torn {
                fs::OpenOptions::new()
                    .append(true)
                    .open(fixture.store.test_journal_path())
                    .unwrap()
                    .write_all(b"{\"incomplete\":")
                    .unwrap();
            }
            let state = fixture._root.path().join("state");
            let before = m2a_file_inventory(&state);
            // An insufficient next-event reserve must remain an inspection,
            // without truncation, generation sealing or restart append.
            let result = fixture
                .store
                .test_inspect(
                    &fixture.lease,
                    ManagementEffectLimitsV1 {
                        max_records: 4,
                        ..ManagementEffectLimitsV1::default()
                    },
                )
                .unwrap();
            let ManagementEffectInspectOpenV1::Present(inspected) = result else {
                panic!("existing store disappeared");
            };
            assert_eq!(
                matches!(inspected.tail(), ManagementEffectTailV1::NeedsRepair { .. }),
                torn
            );
            if let Some(expected) = expected {
                let snapshot = inspected
                    .effects()
                    .find(|snapshot| snapshot.request.operation_id == operation_id(1))
                    .unwrap();
                assert_eq!(
                    inspected
                        .latest_event_evidence(operation_id(1), snapshot)
                        .unwrap(),
                    Some(expected)
                );
            } else {
                assert_eq!(inspected.effects().count(), 0);
            }
            assert_eq!(before, m2a_file_inventory(&state));
            drop(inspected);
            assert_eq!(before, m2a_file_inventory(&state));
        }
    }
}

#[test]
fn m2a_inspect_pins_refuse_changed_file_and_added_namespace_entries() {
    for replace_file in [false, true] {
        let fixture = Fixture::new();
        let (_, prepared) = prepared(&fixture, 1);
        let expected = prepared.verified_event().clone();
        let ManagementEffectInspectOpenV1::Present(inspected) = fixture
            .store
            .test_inspect(&fixture.lease, ManagementEffectLimitsV1::default())
            .unwrap()
        else {
            panic!("missing store");
        };
        let snapshot = inspected.effects().next().unwrap().clone();
        if replace_file {
            let path = fixture.store.test_journal_path();
            let bytes = fs::read(path).unwrap();
            let replacement = path.with_extension("replacement");
            fs::write(&replacement, &bytes).unwrap();
            fs::set_permissions(&replacement, fs::Permissions::from_mode(0o600)).unwrap();
            fs::rename(replacement, path).unwrap();
        } else {
            fs::write(
                fixture
                    .store
                    .test_journal_path()
                    .parent()
                    .unwrap()
                    .join("foreign"),
                b"unchanged",
            )
            .unwrap();
        }
        assert!(
            inspected
                .latest_event_evidence(operation_id(1), &snapshot)
                .is_err()
        );
        assert!(
            inspected
                .event_evidence(operation_id(1), expected.evidence, &expected.event)
                .is_err()
        );
    }
}

impl Fixture {
    fn new() -> Self {
        Self::with_limits(ManagementEffectLimitsV1::default())
    }

    fn with_limits(limits: ManagementEffectLimitsV1) -> Self {
        let root = tempfile::tempdir().unwrap();
        let state = root.path().join("state");
        fs::create_dir(&state).unwrap();
        fs::set_permissions(&state, fs::Permissions::from_mode(0o700)).unwrap();
        let lease_root = state.join("rewind-workspace");
        let owner = ProjectMutationOwnerV1::at_store_root(&lease_root).unwrap();
        let lease = owner.try_mutation().unwrap();
        let expected_lease_root = fs::canonicalize(&lease_root).unwrap();
        let project_scope = hash("project-a");
        let store = ManagementEffectStoreV1::test_open(
            &state,
            expected_lease_root,
            project_scope,
            &lease,
            limits,
        )
        .unwrap();
        Self {
            _root: root,
            _owner: owner,
            lease,
            store,
        }
    }

    fn request(&self, id: u8) -> PrepareManagementEffectV1 {
        request(self.store.project_scope_hash(), id)
    }

    fn reopen(&mut self) {
        let state = self._root.path().join("state");
        let expected_lease_root = fs::canonicalize(state.join("rewind-workspace")).unwrap();
        self.store = ManagementEffectStoreV1::test_open(
            &state,
            expected_lease_root,
            hash("project-a"),
            &self.lease,
            ManagementEffectLimitsV1::default(),
        )
        .unwrap();
    }
}

fn hash(value: &str) -> ManagementEffectHashV1 {
    ManagementEffectHashV1::digest(value.as_bytes())
}

fn operation_id(value: u8) -> ManagementOperationIdV1 {
    ManagementOperationIdV1::from_str(&format!("{value:032x}")).unwrap()
}

fn request(project: ManagementEffectHashV1, value: u8) -> PrepareManagementEffectV1 {
    PrepareManagementEffectV1 {
        operation_id: operation_id(value),
        inventory: ManagementInventoryIdV1::parse("T10").unwrap(),
        scope: ManagementEffectScopeV1 {
            project,
            session: Some(hash("session-a")),
            generation: 7,
            revision: 11,
            permission: ManagementPermissionScopeV1::Project,
            account_binding: None,
        },
        request_hash: hash(&format!("request-{value}")),
        rewind: ManagementRewindDispositionV1::IrreversibleLocal,
    }
}

fn prepared(
    fixture: &Fixture,
    value: u8,
) -> (PrepareManagementEffectV1, PreparedManagementEffectV1) {
    let request = fixture.request(value);
    let cursor = fixture.store.cursor().unwrap();
    let PrepareManagementEffectOutcomeV1::Prepared(effect) = fixture
        .store
        .prepare(&fixture.lease, cursor, request.clone())
        .unwrap()
    else {
        panic!("new request must be prepared");
    };
    (request, effect)
}

#[test]
fn all_legal_branches_are_durable_and_terminal_states_are_immutable() {
    let fixture = Fixture::new();

    let (_, denied_source) = prepared(&fixture, 1);
    fixture
        .store
        .deny(
            &fixture.lease,
            &denied_source,
            ManagementDenialReasonV1::UserDeclined,
        )
        .unwrap();
    assert!(matches!(
        fixture.store.start(&fixture.lease, &denied_source),
        Err(ManagementEffectStoreError::ImmutableTerminal)
    ));

    let (_, completed_source) = prepared(&fixture, 2);
    let started = fixture
        .store
        .start(&fixture.lease, &completed_source)
        .unwrap();
    fixture
        .store
        .complete_started(&fixture.lease, &started, hash("receipt-2"))
        .unwrap();
    assert!(matches!(
        fixture.store.deny(
            &fixture.lease,
            &completed_source,
            ManagementDenialReasonV1::PolicyDenied
        ),
        Err(ManagementEffectStoreError::ImmutableTerminal)
    ));

    let (_, unknown_source) = prepared(&fixture, 3);
    let started = fixture
        .store
        .start(&fixture.lease, &unknown_source)
        .unwrap();
    let unknown = fixture
        .store
        .mark_outcome_unknown(
            &fixture.lease,
            &started,
            ManagementUnknownReasonV1::ReceiptPersistenceFailed,
        )
        .unwrap();
    fixture
        .store
        .complete_unknown(&fixture.lease, &unknown, hash("receipt-3"))
        .unwrap();

    let (_, abandoned_source) = prepared(&fixture, 4);
    let started = fixture
        .store
        .start(&fixture.lease, &abandoned_source)
        .unwrap();
    let unknown = fixture
        .store
        .mark_outcome_unknown(
            &fixture.lease,
            &started,
            ManagementUnknownReasonV1::DetachedEffect,
        )
        .unwrap();
    fixture
        .store
        .reconcile_abandon(&fixture.lease, &unknown, hash("reconciliation-4"))
        .unwrap();

    let effects = fixture.store.effects().unwrap();
    assert_eq!(effects.len(), 4);
    assert!(effects.iter().all(|effect| effect.state.is_terminal()));
    assert!(fixture.store.blocker().unwrap().is_none());
}

#[test]
fn exact_replays_return_the_same_transition_without_appending() {
    let fixture = Fixture::new();
    let request = fixture.request(1);
    let cursor = fixture.store.cursor().unwrap();
    let PrepareManagementEffectOutcomeV1::Prepared(first) = fixture
        .store
        .prepare(&fixture.lease, cursor, request.clone())
        .unwrap()
    else {
        panic!();
    };
    let PrepareManagementEffectOutcomeV1::Prepared(replayed) = fixture
        .store
        .prepare(&fixture.lease, cursor, request.clone())
        .unwrap()
    else {
        panic!();
    };
    assert_eq!(first.last_sequence(), replayed.last_sequence());

    let started = fixture.store.start(&fixture.lease, &first).unwrap();
    let started_replay = fixture.store.start(&fixture.lease, &first).unwrap();
    assert_eq!(started.last_sequence(), started_replay.last_sequence());

    let completed = fixture
        .store
        .complete_started(&fixture.lease, &started, hash("receipt"))
        .unwrap();
    let completed_replay = fixture
        .store
        .complete_started(&fixture.lease, &started, hash("receipt"))
        .unwrap();
    assert_eq!(completed.last_sequence(), completed_replay.last_sequence());
    assert_eq!(fixture.store.cursor().unwrap().sequence, 3);

    assert!(matches!(
        fixture
            .store
            .prepare(&fixture.lease, cursor, request)
            .unwrap(),
        PrepareManagementEffectOutcomeV1::Existing(ManagementEffectSnapshotV1 {
            state: ManagementEffectStateV1::Completed { .. },
            ..
        })
    ));
}

#[test]
fn restart_turns_started_effect_into_blocking_unknown_state() {
    let mut fixture = Fixture::new();
    let (_, prepared) = prepared(&fixture, 1);
    fixture.store.start(&fixture.lease, &prepared).unwrap();

    fixture.reopen();

    let blocker = fixture.store.blocker().unwrap().unwrap();
    assert!(matches!(
        blocker.state,
        ManagementEffectStateV1::OutcomeUnknown {
            reason: ManagementUnknownReasonV1::ProcessRestart
        }
    ));
    assert!(matches!(
        fixture.store.start(&fixture.lease, &prepared),
        Err(ManagementEffectStoreError::UnresolvedEffect)
    ));
    assert!(matches!(
        fixture.store.prepare(
            &fixture.lease,
            fixture.store.cursor().unwrap(),
            fixture.request(2)
        ),
        Err(ManagementEffectStoreError::UnresolvedEffect)
    ));

    let unknown = fixture
        .store
        .unknown_effect(prepared.operation_id())
        .unwrap()
        .unwrap();
    fixture
        .store
        .complete_unknown(&fixture.lease, &unknown, hash("reconciled-receipt"))
        .unwrap();
    assert!(fixture.store.blocker().unwrap().is_none());
}

#[test]
fn prepared_effect_survives_restart_and_can_only_be_denied_or_started() {
    let mut fixture = Fixture::new();
    let (_, prepared) = prepared(&fixture, 1);
    fixture.reopen();

    let recovered = fixture
        .store
        .prepared_effect(prepared.operation_id())
        .unwrap()
        .unwrap();
    fixture
        .store
        .deny(
            &fixture.lease,
            &recovered,
            ManagementDenialReasonV1::StaleRequest,
        )
        .unwrap();
    assert!(fixture.store.blocker().unwrap().is_none());
}

#[test]
fn incomplete_tail_is_repaired_but_complete_corruption_is_rejected() {
    let mut fixture = Fixture::new();
    let (_, effect) = prepared(&fixture, 1);
    let journal = fixture.store.test_journal_path().to_path_buf();
    OpenAppend::write(&journal, br#"{"incomplete":"#);

    fixture.reopen();
    assert!(matches!(
        fixture
            .store
            .effect(effect.operation_id())
            .unwrap()
            .unwrap()
            .state,
        ManagementEffectStateV1::Prepared
    ));
    assert_eq!(fs::read(&journal).unwrap().last(), Some(&b'\n'));

    let mut bytes = fs::read(&journal).unwrap();
    let pattern = br#""version":1"#;
    let offset = bytes
        .windows(pattern.len())
        .position(|window| window == pattern)
        .unwrap()
        + pattern.len()
        - 1;
    bytes[offset] = b'2';
    fs::write(&journal, bytes).unwrap();
    let state = fixture._root.path().join("state");
    let expected = fs::canonicalize(state.join("rewind-workspace")).unwrap();
    assert!(matches!(
        ManagementEffectStoreV1::test_open(
            &state,
            expected,
            hash("project-a"),
            &fixture.lease,
            ManagementEffectLimitsV1::default()
        ),
        Err(ManagementEffectStoreError::CorruptJournal)
    ));
}

struct OpenAppend;

impl OpenAppend {
    fn write(path: &PathBuf, bytes: &[u8]) {
        let mut file = fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(bytes).unwrap();
        file.sync_all().unwrap();
    }
}

#[test]
fn full_terminal_generation_rolls_over_and_old_ids_stay_unique() {
    let limits = ManagementEffectLimitsV1 {
        max_journal_bytes: 64 * 1024,
        max_records: 4,
        max_effects: 1,
        max_record_bytes: 16 * 1024,
    };
    let fixture = Fixture::with_limits(limits);
    let stale = fixture.store.cursor().unwrap();
    let (first_request, first) = prepared(&fixture, 1);
    fixture
        .store
        .deny(
            &fixture.lease,
            &first,
            ManagementDenialReasonV1::UserDeclined,
        )
        .unwrap();
    assert!(matches!(
        fixture
            .store
            .prepare(&fixture.lease, stale, fixture.request(2)),
        Err(ManagementEffectStoreError::StaleCursor)
    ));
    assert!(matches!(
        fixture
            .store
            .prepare(&fixture.lease, stale, first_request)
            .unwrap(),
        PrepareManagementEffectOutcomeV1::Existing(_)
    ));

    let current = fixture.store.cursor().unwrap();
    let second = fixture.request(2);
    assert!(matches!(
        fixture.store.prepare(&fixture.lease, current, second),
        Ok(PrepareManagementEffectOutcomeV1::Prepared(_))
    ));
    assert_eq!(fixture.store.cursor().unwrap().generation, 1);
    assert!(matches!(
        fixture.store.start(&fixture.lease, &first),
        Err(ManagementEffectStoreError::StaleEffect)
    ));
    assert!(matches!(
        fixture
            .store
            .prepare(&fixture.lease, current, fixture.request(3)),
        Err(ManagementEffectStoreError::UnresolvedEffect)
    ));
    assert!(fixture.store.effect(operation_id(1)).unwrap().is_some());
    let mut conflicting = fixture.request(1);
    conflicting.request_hash = hash("different");
    assert!(matches!(
        fixture.store.prepare(&fixture.lease, current, conflicting),
        Err(ManagementEffectStoreError::RequestConflict)
    ));
    let legacy = fixture.store.test_journal_path();
    assert!(
        fs::read_to_string(legacy)
            .unwrap()
            .contains("user_declined")
    );
    assert!(
        fixture
            ._root
            .path()
            .join("state/management-effects-v1/CURRENT")
            .exists()
    );
}

#[test]
fn record_and_byte_reservations_seal_before_the_next_prepare() {
    let cases = [
        ManagementEffectLimitsV1 {
            max_journal_bytes: 256 * 1024,
            max_records: 4,
            max_effects: 8,
            max_record_bytes: 16 * 1024,
        },
        ManagementEffectLimitsV1 {
            max_journal_bytes: 64 * 1024,
            max_records: 100,
            max_effects: 8,
            max_record_bytes: 16 * 1024,
        },
    ];
    for limits in cases {
        let fixture = Fixture::with_limits(limits);
        let (first_request, first) = prepared(&fixture, 1);
        fixture
            .store
            .deny(
                &fixture.lease,
                &first,
                ManagementDenialReasonV1::UserDeclined,
            )
            .unwrap();
        let old_cursor = fixture.store.cursor().unwrap();
        let (_, second) = prepared(&fixture, 2);
        assert_eq!(second.generation(), 1);
        fixture
            .store
            .deny(
                &fixture.lease,
                &second,
                ManagementDenialReasonV1::UserDeclined,
            )
            .unwrap();
        assert!(matches!(
            fixture
                .store
                .prepare(&fixture.lease, old_cursor, fixture.request(3)),
            Err(ManagementEffectStoreError::StaleCursor)
        ));
        assert!(matches!(
            fixture
                .store
                .prepare(&fixture.lease, old_cursor, first_request)
                .unwrap(),
            PrepareManagementEffectOutcomeV1::Existing(_)
        ));
    }
}

#[test]
fn full_legacy_with_started_effect_rescues_before_recovery() {
    let mut fixture = Fixture::new();
    let (first_request, first) = prepared(&fixture, 1);
    fixture
        .store
        .deny(
            &fixture.lease,
            &first,
            ManagementDenialReasonV1::UserDeclined,
        )
        .unwrap();
    let (_, second) = prepared(&fixture, 2);
    fixture.store.start(&fixture.lease, &second).unwrap();
    let legacy = fs::read(fixture.store.test_journal_path()).unwrap();
    let state_dir = fixture._root.path().join("state");
    let expected = fs::canonicalize(state_dir.join("rewind-workspace")).unwrap();
    fixture.store = ManagementEffectStoreV1::test_open(
        &state_dir,
        expected,
        hash("project-a"),
        &fixture.lease,
        ManagementEffectLimitsV1 {
            max_journal_bytes: 64 * 1024,
            max_records: 4,
            max_effects: 2,
            max_record_bytes: 16 * 1024,
        },
    )
    .unwrap();
    assert_eq!(fs::read(fixture.store.test_journal_path()).unwrap(), legacy);
    assert_eq!(fixture.store.cursor().unwrap().generation, 1);
    assert!(matches!(
        fixture.store.blocker().unwrap().unwrap().state,
        ManagementEffectStateV1::OutcomeUnknown {
            reason: ManagementUnknownReasonV1::ProcessRestart
        }
    ));
    assert!(
        fixture
            .store
            .effect(first_request.operation_id)
            .unwrap()
            .is_some()
    );
    let unknown = fixture
        .store
        .unknown_effect(operation_id(2))
        .unwrap()
        .unwrap();
    fixture
        .store
        .complete_unknown(&fixture.lease, &unknown, hash("reconciled"))
        .unwrap();
    assert!(fixture.store.blocker().unwrap().is_none());
}

#[test]
fn full_legacy_rescues_prepared_and_unknown_snapshots() {
    for unknown_before_restart in [false, true] {
        let mut fixture = Fixture::new();
        let (_, first) = prepared(&fixture, 1);
        fixture
            .store
            .deny(
                &fixture.lease,
                &first,
                ManagementDenialReasonV1::UserDeclined,
            )
            .unwrap();
        let (_, second) = prepared(&fixture, 2);
        if unknown_before_restart {
            let started = fixture.store.start(&fixture.lease, &second).unwrap();
            fixture
                .store
                .mark_outcome_unknown(
                    &fixture.lease,
                    &started,
                    ManagementUnknownReasonV1::DetachedEffect,
                )
                .unwrap();
        }
        let legacy = fs::read(fixture.store.test_journal_path()).unwrap();
        let state_dir = fixture._root.path().join("state");
        let expected = fs::canonicalize(state_dir.join("rewind-workspace")).unwrap();
        fixture.store = ManagementEffectStoreV1::test_open(
            &state_dir,
            expected,
            hash("project-a"),
            &fixture.lease,
            ManagementEffectLimitsV1 {
                max_journal_bytes: 64 * 1024,
                max_records: if unknown_before_restart { 5 } else { 3 },
                max_effects: 2,
                max_record_bytes: 16 * 1024,
            },
        )
        .unwrap();
        assert_eq!(fixture.store.cursor().unwrap().generation(), 1);
        assert_eq!(fs::read(fixture.store.test_journal_path()).unwrap(), legacy);
        let unknown = if unknown_before_restart {
            fixture
                .store
                .unknown_effect(operation_id(2))
                .unwrap()
                .unwrap()
        } else {
            let resumed = fixture
                .store
                .prepared_effect(operation_id(2))
                .unwrap()
                .unwrap();
            let started = fixture.store.start(&fixture.lease, &resumed).unwrap();
            fixture
                .store
                .mark_outcome_unknown(
                    &fixture.lease,
                    &started,
                    ManagementUnknownReasonV1::DetachedEffect,
                )
                .unwrap()
        };
        fixture
            .store
            .complete_unknown(&fixture.lease, &unknown, hash("resolution"))
            .unwrap();
        assert!(fixture.store.blocker().unwrap().is_none());
    }
}

#[test]
fn multiple_seals_page_old_effects_and_detect_archive_damage() {
    let fixture = Fixture::with_limits(ManagementEffectLimitsV1 {
        max_journal_bytes: 64 * 1024,
        max_records: 4,
        max_effects: 1,
        max_record_bytes: 16 * 1024,
    });
    for value in 1..=3 {
        let (_, effect) = prepared(&fixture, value);
        fixture
            .store
            .deny(
                &fixture.lease,
                &effect,
                ManagementDenialReasonV1::UserDeclined,
            )
            .unwrap();
    }
    let cursor = fixture.store.cursor().unwrap();
    assert_eq!(cursor.generation, 2);
    let capacity = fixture.store.capacity().unwrap();
    assert_eq!(capacity.archived_effects, 2);
    assert_eq!(capacity.prior_records, 4);
    assert!(capacity.retained_archive_bytes > 0);
    for (page, id) in [(1, 1), (2, 2), (3, 3)] {
        let rows = fixture.store.effects_page(cursor, page, None, 1).unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].request.operation_id, operation_id(id));
        assert!(
            fixture
                .store
                .effects_page(cursor, page, Some(operation_id(id)), 1)
                .unwrap()
                .is_empty()
        );
        assert!(fixture.store.effect(operation_id(id)).unwrap().is_some());
    }
    let archive = fixture
        ._root
        .path()
        .join("state/management-effects-v1/generations/1/archive.v1.json");
    let mut bytes = fs::read(&archive).unwrap();
    bytes[0] = b' ';
    fs::write(&archive, bytes).unwrap();
    assert!(matches!(
        fixture.store.cursor(),
        Err(ManagementEffectStoreError::CorruptJournal)
    ));
}

#[test]
fn missing_sealed_legacy_journal_is_not_recreated() {
    let fixture = Fixture::with_limits(ManagementEffectLimitsV1 {
        max_journal_bytes: 64 * 1024,
        max_records: 4,
        max_effects: 1,
        max_record_bytes: 16 * 1024,
    });
    let (_, first) = prepared(&fixture, 1);
    fixture
        .store
        .deny(
            &fixture.lease,
            &first,
            ManagementDenialReasonV1::UserDeclined,
        )
        .unwrap();
    let (_, second) = prepared(&fixture, 2);
    fixture
        .store
        .deny(
            &fixture.lease,
            &second,
            ManagementDenialReasonV1::UserDeclined,
        )
        .unwrap();
    let legacy = fixture.store.test_journal_path();
    fs::remove_file(legacy).unwrap();
    let state_dir = fixture._root.path().join("state");
    let expected = fs::canonicalize(state_dir.join("rewind-workspace")).unwrap();
    assert!(
        ManagementEffectStoreV1::test_open(
            &state_dir,
            expected,
            hash("project-a"),
            &fixture.lease,
            ManagementEffectLimitsV1::default()
        )
        .is_err()
    );
    assert!(!legacy.exists());
}

#[test]
fn interrupted_seal_adopts_only_published_generation() {
    let limits = ManagementEffectLimitsV1 {
        max_journal_bytes: 64 * 1024,
        max_records: 4,
        max_effects: 1,
        max_record_bytes: 16 * 1024,
    };
    for point in 1..=6 {
        let mut fixture = Fixture::with_limits(limits);
        let (_, first) = prepared(&fixture, 1);
        fixture
            .store
            .deny(
                &fixture.lease,
                &first,
                ManagementDenialReasonV1::UserDeclined,
            )
            .unwrap();
        let legacy = fs::read(fixture.store.test_journal_path()).unwrap();
        fixture.store.test_fail_seal_once_at(point);
        assert!(
            matches!(
                fixture.store.prepare(
                    &fixture.lease,
                    fixture.store.cursor().unwrap(),
                    fixture.request(2)
                ),
                Err(ManagementEffectStoreError::Io(_))
            ),
            "point {point}"
        );
        assert_eq!(fs::read(fixture.store.test_journal_path()).unwrap(), legacy);
        let state_dir = fixture._root.path().join("state");
        let expected = fs::canonicalize(state_dir.join("rewind-workspace")).unwrap();
        fixture.store = ManagementEffectStoreV1::test_open(
            &state_dir,
            expected,
            hash("project-a"),
            &fixture.lease,
            limits,
        )
        .unwrap();
        assert_eq!(
            fixture.store.cursor().unwrap().generation(),
            u64::from(point == 6),
            "point {point}"
        );
        assert!(
            matches!(
                fixture.store.prepare(
                    &fixture.lease,
                    fixture.store.cursor().unwrap(),
                    fixture.request(2)
                ),
                Ok(PrepareManagementEffectOutcomeV1::Prepared(_))
            ),
            "point {point}"
        );
        assert!(fixture.store.effect(operation_id(1)).unwrap().is_some());
    }
}

#[test]
fn two_restarts_before_pointer_publication_keep_every_orphan_and_retry() {
    let limits = ManagementEffectLimitsV1 {
        max_journal_bytes: 64 * 1024,
        max_records: 4,
        max_effects: 1,
        max_record_bytes: 16 * 1024,
    };
    let mut fixture = Fixture::with_limits(limits);
    let (_, first) = prepared(&fixture, 1);
    fixture
        .store
        .deny(
            &fixture.lease,
            &first,
            ManagementDenialReasonV1::UserDeclined,
        )
        .unwrap();
    let legacy = fs::read(fixture.store.test_journal_path()).unwrap();
    let state_dir = fixture._root.path().join("state");
    let store_root = state_dir.join("management-effects-v1");

    for _ in 0..2 {
        fixture.store.test_reset_file_sequence_for_restart();
        fixture.store.test_fail_seal_once_at(5);
        assert!(matches!(
            fixture.store.prepare(
                &fixture.lease,
                fixture.store.cursor().unwrap(),
                fixture.request(2)
            ),
            Err(ManagementEffectStoreError::Io(_))
        ));
        let expected = fs::canonicalize(state_dir.join("rewind-workspace")).unwrap();
        fixture.store = ManagementEffectStoreV1::test_open(
            &state_dir,
            expected,
            hash("project-a"),
            &fixture.lease,
            limits,
        )
        .unwrap();
        assert_eq!(fixture.store.cursor().unwrap().generation(), 0);
        assert_eq!(fs::read(fixture.store.test_journal_path()).unwrap(), legacy);
    }

    fixture.store.test_reset_file_sequence_for_restart();
    assert!(matches!(
        fixture.store.prepare(
            &fixture.lease,
            fixture.store.cursor().unwrap(),
            fixture.request(2)
        ),
        Ok(PrepareManagementEffectOutcomeV1::Prepared(_))
    ));
    assert_eq!(fixture.store.cursor().unwrap().generation(), 1);
    assert_eq!(fs::read(fixture.store.test_journal_path()).unwrap(), legacy);
    assert!(fixture.store.effect(operation_id(1)).unwrap().is_some());
    let orphans = fs::read_dir(store_root.join("generations"))
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".orphan-1-")
        })
        .count();
    let temporary_pointers = fs::read_dir(store_root)
        .unwrap()
        .filter(|entry| {
            entry
                .as_ref()
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".CURRENT-")
        })
        .count();
    assert_eq!(orphans, 2);
    assert_eq!(temporary_pointers, 2);
}

#[test]
fn read_only_store_does_not_publish_a_generation() {
    let fixture = Fixture::with_limits(ManagementEffectLimitsV1 {
        max_journal_bytes: 64 * 1024,
        max_records: 4,
        max_effects: 1,
        max_record_bytes: 16 * 1024,
    });
    let (_, first) = prepared(&fixture, 1);
    fixture
        .store
        .deny(
            &fixture.lease,
            &first,
            ManagementDenialReasonV1::UserDeclined,
        )
        .unwrap();
    let root = fixture._root.path().join("state/management-effects-v1");
    let legacy = fs::read(fixture.store.test_journal_path()).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o500)).unwrap();
    let result = fixture.store.prepare(
        &fixture.lease,
        fixture.store.cursor().unwrap(),
        fixture.request(2),
    );
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(result, Err(ManagementEffectStoreError::Io(_))));
    assert_eq!(fs::read(fixture.store.test_journal_path()).unwrap(), legacy);
    assert!(!root.join("CURRENT").exists());
    assert!(matches!(
        fixture.store.prepare(
            &fixture.lease,
            fixture.store.cursor().unwrap(),
            fixture.request(2)
        ),
        Ok(PrepareManagementEffectOutcomeV1::Prepared(_))
    ));
}

#[test]
fn concurrent_prepare_with_one_cursor_accepts_only_one_request() {
    let mut fixture = Fixture::new();
    let cursor = fixture.store.cursor().unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let results = std::thread::scope(|scope| {
        let store = &fixture.store;
        let lease = &fixture.lease;
        let first_request = fixture.request(1);
        let second_request = fixture.request(2);
        let first_barrier = Arc::clone(&barrier);
        let first = scope.spawn(move || {
            first_barrier.wait();
            store.prepare(lease, cursor, first_request)
        });
        let second_barrier = Arc::clone(&barrier);
        let second = scope.spawn(move || {
            second_barrier.wait();
            store.prepare(lease, cursor, second_request)
        });
        barrier.wait();
        [first.join().unwrap(), second.join().unwrap()]
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(ManagementEffectStoreError::UnresolvedEffect)))
            .count(),
        1
    );
    fixture.reopen();
    assert_eq!(fixture.store.effects().unwrap().len(), 1);
    assert!(fixture.store.blocker().unwrap().is_some());
}

#[test]
fn invalid_pointer_does_not_repair_an_unverified_tail() {
    let limits = ManagementEffectLimitsV1 {
        max_journal_bytes: 64 * 1024,
        max_records: 4,
        max_effects: 1,
        max_record_bytes: 16 * 1024,
    };
    let fixture = Fixture::with_limits(limits);
    let (_, first) = prepared(&fixture, 1);
    fixture
        .store
        .deny(
            &fixture.lease,
            &first,
            ManagementDenialReasonV1::UserDeclined,
        )
        .unwrap();
    let (_, _) = prepared(&fixture, 2);
    let root = fixture._root.path().join("state/management-effects-v1");
    let active = root.join("generations/1/journal.v1.jsonl");
    OpenAppend::write(&active, b"{\"incomplete");
    let original = fs::read(&active).unwrap();
    let pointer = root.join("CURRENT");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&pointer).unwrap()).unwrap();
    value["manifest_hash"] = serde_json::json!(ManagementEffectHashV1::zero().as_hex());
    fs::write(pointer, serde_json::to_vec(&value).unwrap()).unwrap();
    let state_dir = fixture._root.path().join("state");
    let expected = fs::canonicalize(state_dir.join("rewind-workspace")).unwrap();
    assert!(matches!(
        ManagementEffectStoreV1::test_open(
            &state_dir,
            expected,
            hash("project-a"),
            &fixture.lease,
            limits
        ),
        Err(ManagementEffectStoreError::CorruptJournal)
    ));
    assert_eq!(fs::read(active).unwrap(), original);
}

#[test]
fn replay_rejects_zero_terminal_evidence_with_valid_record_hash() {
    for abandon in [false, true] {
        let fixture = Fixture::new();
        let (_, prepared) = prepared(&fixture, 1);
        let started = fixture.store.start(&fixture.lease, &prepared).unwrap();
        if abandon {
            fixture
                .store
                .mark_outcome_unknown(
                    &fixture.lease,
                    &started,
                    ManagementUnknownReasonV1::DetachedEffect,
                )
                .unwrap();
        }
        fixture
            .store
            .test_append_zero_evidence(&fixture.lease, operation_id(1), abandon)
            .unwrap();
        let state_dir = fixture._root.path().join("state");
        let expected = fs::canonicalize(state_dir.join("rewind-workspace")).unwrap();
        assert!(matches!(
            ManagementEffectStoreV1::test_open(
                &state_dir,
                expected,
                hash("project-a"),
                &fixture.lease,
                ManagementEffectLimitsV1::default()
            ),
            Err(ManagementEffectStoreError::CorruptJournal)
        ));
    }
}

#[test]
fn an_individual_record_cannot_exceed_the_configured_byte_bound() {
    let fixture = Fixture::with_limits(ManagementEffectLimitsV1 {
        max_journal_bytes: 4096,
        max_records: 16,
        max_effects: 8,
        max_record_bytes: 128,
    });
    assert!(matches!(
        fixture.store.prepare(
            &fixture.lease,
            fixture.store.cursor().unwrap(),
            fixture.request(1)
        ),
        Err(ManagementEffectStoreError::StoreFull)
    ));
    assert!(fixture.store.effects().unwrap().is_empty());
}

#[test]
fn scope_id_hash_and_lease_validation_reject_ambiguous_requests() {
    let fixture = Fixture::new();
    let cursor = fixture.store.cursor().unwrap();

    let mut wrong_scope = fixture.request(1);
    wrong_scope.scope.project = hash("another-project");
    assert!(matches!(
        fixture.store.prepare(&fixture.lease, cursor, wrong_scope),
        Err(ManagementEffectStoreError::WrongProjectScope)
    ));

    let mut zero_id = fixture.request(1);
    zero_id.operation_id =
        ManagementOperationIdV1::from_str("00000000000000000000000000000000").unwrap();
    assert!(matches!(
        fixture.store.prepare(&fixture.lease, cursor, zero_id),
        Err(ManagementEffectStoreError::InvalidValue)
    ));

    assert!(ManagementEffectHashV1::from_hex("AA").is_err());
    assert!(ManagementInventoryIdV1::parse("../../secret").is_err());
    assert!(ManagementInventoryIdV1::parse("é1").is_err());
    assert!(ManagementInventoryIdV1::parse("T22").is_err());
    assert!(ManagementInventoryIdV1::parse("C42").is_ok());
    assert!(ManagementInventoryIdV1::parse("D41").is_ok());
    assert!(ManagementInventoryIdV1::parse("F25").is_ok());

    let other_root = tempfile::tempdir().unwrap();
    let other_owner = ProjectMutationOwnerV1::at_store_root(other_root.path()).unwrap();
    let other_lease = other_owner.try_mutation().unwrap();
    assert!(matches!(
        fixture
            .store
            .prepare(&other_lease, cursor, fixture.request(2)),
        Err(ManagementEffectStoreError::WrongProjectLease)
    ));
}

#[test]
fn conflicting_same_id_is_not_treated_as_an_exact_replay() {
    let fixture = Fixture::new();
    let (mut request, _) = prepared(&fixture, 1);
    request.scope.revision += 1;
    assert!(matches!(
        fixture
            .store
            .prepare(&fixture.lease, fixture.store.cursor().unwrap(), request),
        Err(ManagementEffectStoreError::RequestConflict)
    ));
}

#[test]
fn journal_contains_only_typed_metadata_and_hashes() {
    let fixture = Fixture::new();
    let _ = prepared(&fixture, 1);
    let journal = fs::read_to_string(fixture.store.test_journal_path()).unwrap();
    assert!(!journal.contains("super-secret"));
    assert!(!journal.contains("/Users/example/project"));
    assert!(!journal.contains("prompt body"));
    assert!(journal.contains("request_hash"));
    assert!(journal.contains("permission"));
}

#[derive(Clone, Copy)]
enum ExpectedPresence {
    Forbidden,
    Optional,
    Required,
}

impl ExpectedPresence {
    fn accepts(self, present: bool) -> bool {
        match self {
            Self::Forbidden => !present,
            Self::Optional => true,
            Self::Required => present,
        }
    }
}

type ExpectedInventory = (
    &'static [ManagementPermissionScopeV1],
    ManagementRewindDispositionV1,
    ExpectedPresence,
    ExpectedPresence,
);

fn expected_inventory(id: &str) -> Option<ExpectedInventory> {
    use ExpectedPresence::{Forbidden, Optional, Required};
    use ManagementPermissionScopeV1::{Clipboard, LocalSession, Project, Remote, System};
    use ManagementRewindDispositionV1::{
        CapturedRestorable, IrreversibleLocal, IrreversibleRemote, RuntimeUnknown,
    };

    // Independent transcription of the approved 70-row inventory, grouped by contract.
    let groups: &[(&str, ExpectedInventory)] = &[
        (
            "T03 T04 D17 D19 D20 F12 F15",
            (&[LocalSession], CapturedRestorable, Required, Forbidden),
        ),
        (
            "T06",
            (&[LocalSession], IrreversibleLocal, Optional, Forbidden),
        ),
        (
            "T07 T08 T16 D07 D10 D12 D18 D21 D23 D24 F06 F10 F16 F17 F18 C20",
            (&[LocalSession], IrreversibleLocal, Required, Forbidden),
        ),
        (
            "T09 D13",
            (&[LocalSession], IrreversibleLocal, Required, Optional),
        ),
        (
            "C08 C11 C12 C13 C14 C18 C19 C30 C31 C35 D27 T10 F07 F08 F09",
            (&[Project], IrreversibleLocal, Optional, Forbidden),
        ),
        (
            "C22 C23 C24 C25 C26 C27 C28 D31 D32 D33 D34 D35 D37 C02 C06 D41",
            (&[System], IrreversibleLocal, Optional, Forbidden),
        ),
        (
            "C03 C04 C05 C07 T20",
            (&[System], IrreversibleLocal, Optional, Required),
        ),
        (
            "T19",
            (&[Clipboard], IrreversibleLocal, Required, Forbidden),
        ),
        ("C10", (&[Remote], IrreversibleRemote, Optional, Required)),
        (
            "T17",
            (&[Project, System], IrreversibleLocal, Required, Forbidden),
        ),
        (
            "C37 D08 D22 T14",
            (
                &[LocalSession, Project, System, Remote],
                RuntimeUnknown,
                Required,
                Optional,
            ),
        ),
        (
            "C38",
            (
                &[Project, System, Remote],
                RuntimeUnknown,
                Optional,
                Forbidden,
            ),
        ),
    ];
    groups.iter().find_map(|(ids, expected)| {
        ids.split_ascii_whitespace()
            .any(|candidate| candidate == id)
            .then_some(*expected)
    })
}

#[test]
fn all_parseable_inventory_ids_have_the_approved_field_contract() {
    use ManagementPermissionScopeV1::{Clipboard, LocalSession, Project, Remote, System};
    use ManagementRewindDispositionV1::{
        CapturedRestorable, IrreversibleLocal, IrreversibleRemote, RuntimeUnknown,
    };

    let mut accepted_ids = HashSet::new();
    for (family, maximum) in [('C', 42), ('D', 41), ('T', 21), ('F', 25)] {
        for number in 1..=maximum {
            let fixture = Fixture::new();
            let mut cursor = fixture.store.cursor().unwrap();
            let mut journal = fs::read(fixture.store.test_journal_path()).unwrap();
            let mut serial = 1_u32;
            let id = format!("{family}{number:02}");
            let inventory = ManagementInventoryIdV1::parse(&id).unwrap();
            let expected = expected_inventory(&id);
            if expected.is_some() {
                assert!(accepted_ids.insert(id.clone()), "duplicate {id}");
            }
            for permission in [LocalSession, Project, Clipboard, System, Remote] {
                for rewind in [
                    CapturedRestorable,
                    IrreversibleLocal,
                    IrreversibleRemote,
                    RuntimeUnknown,
                ] {
                    for session_present in [false, true] {
                        for account_present in [false, true] {
                            let mut probe = request(hash("project-a"), 1);
                            probe.operation_id =
                                ManagementOperationIdV1::from_str(&format!("{serial:032x}"))
                                    .unwrap();
                            serial += 1;
                            probe.inventory = inventory;
                            probe.scope.permission = permission;
                            probe.rewind = rewind;
                            probe.scope.session = session_present.then(|| hash("session-a"));
                            probe.scope.account_binding =
                                account_present.then(|| hash("account-a"));
                            let accepted =
                                expected.is_some_and(|(allowed, disposition, session, account)| {
                                    allowed.contains(&permission)
                                        && disposition == rewind
                                        && session.accepts(session_present)
                                        && account.accepts(account_present)
                                });
                            if accepted {
                                let PrepareManagementEffectOutcomeV1::Prepared(effect) = fixture
                                    .store
                                    .prepare(&fixture.lease, cursor, probe)
                                    .unwrap()
                                else {
                                    panic!("{id} did not prepare a new effect");
                                };
                                fixture
                                    .store
                                    .deny(
                                        &fixture.lease,
                                        &effect,
                                        ManagementDenialReasonV1::UserDeclined,
                                    )
                                    .unwrap();
                                cursor = fixture.store.cursor().unwrap();
                                journal = fs::read(fixture.store.test_journal_path()).unwrap();
                            } else {
                                assert!(
                                    matches!(
                                        fixture.store.prepare(&fixture.lease, cursor, probe),
                                        Err(ManagementEffectStoreError::InvalidInventoryContract)
                                    ),
                                    "{id} {permission:?} {rewind:?} session={session_present} account={account_present}"
                                );
                                assert_eq!(fixture.store.cursor().unwrap(), cursor);
                                assert_eq!(
                                    fs::read(fixture.store.test_journal_path()).unwrap(),
                                    journal
                                );
                            }
                        }
                    }
                }
            }
        }
    }
    assert_eq!(accepted_ids.len(), 70);
}

fn assert_contract_rejected_without_write(fixture: &Fixture, probe: PrepareManagementEffectV1) {
    let cursor = fixture.store.cursor().unwrap();
    let before = fs::read(fixture.store.test_journal_path()).unwrap();
    assert!(matches!(
        fixture.store.prepare(&fixture.lease, cursor, probe),
        Err(ManagementEffectStoreError::InvalidInventoryContract)
    ));
    assert_eq!(fixture.store.cursor().unwrap(), cursor);
    assert_eq!(fs::read(fixture.store.test_journal_path()).unwrap(), before);
}

#[test]
fn source_and_remote_contract_mismatches_cannot_change_the_journal() {
    let fixture = Fixture::new();
    for id in ["T16", "F18"] {
        let mut probe = fixture.request(1);
        probe.inventory = ManagementInventoryIdV1::parse(id).unwrap();
        probe.scope.permission = ManagementPermissionScopeV1::LocalSession;
        probe.scope.session = None;
        assert_contract_rejected_without_write(&fixture, probe);
    }
    let mut remote = fixture.request(1);
    remote.inventory = ManagementInventoryIdV1::parse("C10").unwrap();
    remote.scope.permission = ManagementPermissionScopeV1::Remote;
    remote.scope.account_binding = Some(hash("account-a"));
    assert_contract_rejected_without_write(&fixture, remote.clone());
    remote.rewind = ManagementRewindDispositionV1::IrreversibleRemote;
    remote.scope.permission = ManagementPermissionScopeV1::Project;
    assert_contract_rejected_without_write(&fixture, remote);
}

#[test]
fn account_binding_hash_is_stable_and_unambiguous() {
    assert_eq!(
        ManagementEffectHashV1::account_binding("openai", "registration-42")
            .unwrap()
            .as_hex(),
        "fc791fdd44ddeb732e88ddb613e67ed7295bf64b593872ba406bf3752544bed7"
    );
    assert_ne!(
        ManagementEffectHashV1::account_binding("ab", "c").unwrap(),
        ManagementEffectHashV1::account_binding("a", "bc").unwrap()
    );
    assert!(ManagementEffectHashV1::account_binding("OpenAI", "registration-42").is_err());
    assert!(ManagementEffectHashV1::account_binding("openai", "").is_err());
}

#[test]
fn legacy_contract_mismatch_remains_readable_and_reconcilable() {
    let mut fixture = Fixture::new();
    let mut legacy = fixture.request(1);
    legacy.inventory = ManagementInventoryIdV1::parse("C01").unwrap();
    legacy.scope.permission = ManagementPermissionScopeV1::Remote;
    legacy.rewind = ManagementRewindDispositionV1::IrreversibleRemote;
    legacy.scope.account_binding = Some(hash("legacy-account"));
    fixture
        .store
        .test_append_legacy_prepared(&fixture.lease, legacy.clone())
        .unwrap();
    let original = fs::read(fixture.store.test_journal_path()).unwrap();
    fixture.reopen();
    assert_eq!(
        fs::read(fixture.store.test_journal_path()).unwrap(),
        original
    );
    assert!(fixture.store.blocker().unwrap().is_some());
    assert_contract_rejected_without_write(&fixture, legacy.clone());
    let prepared = fixture
        .store
        .prepared_effect(legacy.operation_id)
        .unwrap()
        .unwrap();
    fixture
        .store
        .deny(
            &fixture.lease,
            &prepared,
            ManagementDenialReasonV1::PolicyDenied,
        )
        .unwrap();
    fixture.reopen();
    assert!(fixture.store.blocker().unwrap().is_none());
    assert!(matches!(
        fixture
            .store
            .effect(legacy.operation_id)
            .unwrap()
            .unwrap()
            .state,
        ManagementEffectStateV1::Denied { .. }
    ));
}

#[test]
fn sealed_legacy_contract_mismatch_stays_queryable() {
    let mut fixture = Fixture::with_limits(ManagementEffectLimitsV1 {
        max_journal_bytes: 64 * 1024,
        max_records: 4,
        max_effects: 1,
        max_record_bytes: 16 * 1024,
    });
    let mut legacy = fixture.request(1);
    legacy.inventory = ManagementInventoryIdV1::parse("D01").unwrap();
    fixture
        .store
        .test_append_legacy_prepared(&fixture.lease, legacy.clone())
        .unwrap();
    let legacy_effect = fixture
        .store
        .prepared_effect(legacy.operation_id)
        .unwrap()
        .unwrap();
    fixture
        .store
        .deny(
            &fixture.lease,
            &legacy_effect,
            ManagementDenialReasonV1::PolicyDenied,
        )
        .unwrap();
    let (_, second) = prepared(&fixture, 2);
    fixture
        .store
        .deny(
            &fixture.lease,
            &second,
            ManagementDenialReasonV1::UserDeclined,
        )
        .unwrap();
    fixture.reopen();
    assert!(matches!(
        fixture
            .store
            .effect(legacy.operation_id)
            .unwrap()
            .unwrap()
            .state,
        ManagementEffectStateV1::Denied { .. }
    ));
    assert_contract_rejected_without_write(&fixture, legacy);
}
