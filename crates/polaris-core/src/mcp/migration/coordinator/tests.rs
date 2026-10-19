//! Migration coordinator storage ownership and fresh-boundary regression tests.
use super::super::{
    account_home::with_fixture_home,
    entry_gate::{
        GATE_FILE, ProductionBinaryIdentityV2, ProductionEntryIdentityV2, ProductionEntryKindV2,
    },
    test_fixture::GuardFixture,
};
use super::*;
use crate::conversation_state::{ConversationStateV2, ConversationStore};
use std::{fs, os::unix::fs::PermissionsExt, path::PathBuf};
const ID: &str = "00000000-0000-4000-8000-000000000001";
fn directory(path: &std::path::Path) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
}
fn fixture() -> (GuardFixture, PathBuf, ConversationStore) {
    fixture_at_default(false)
}
fn fixture_at_default(default: bool) -> (GuardFixture, PathBuf, ConversationStore) {
    let fixture = GuardFixture::new(&"a".repeat(64));
    let home = fixture.guard.verified_binding().unwrap().0;
    let store = home.join(".polaris/mcp");
    directory(&store);
    directory(&store.join("servers"));
    let data = if default {
        home.join(".polaris")
    } else {
        home.join("data")
    };
    if !default {
        directory(&data);
    }
    let session = ConversationStore::create(
        &data,
        ConversationStateV2::new("project".into(), ID.into()).unwrap(),
    )
    .unwrap();
    (fixture, data, session)
}
#[test]
fn coordinator_begin_rejects_missing_source_before_entry_mutation() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    directory(&home);
    let parent = home.join(".polaris");
    directory(&parent);
    let binary = root.path().join("polaris");
    fs::write(&binary, b"synthetic executable identity\n").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o500)).unwrap();
    let manifest = ProductionEntryManifestV2 {
        protocol_version: 2,
        entries: vec![ProductionEntryIdentityV2 {
            kind: ProductionEntryKindV2::Cli,
            binary: ProductionBinaryIdentityV2::inspect(&binary).unwrap(),
        }],
        runtime_copy_source_hashes: vec![],
    };
    manifest.canonical_hash().unwrap();
    with_fixture_home(&home, || {
        let trusted = Arc::new(TrustedUserHomeV2::inspect().unwrap());
        assert!(matches!(
            MigrationAdmissionV2::begin(trusted, &"a".repeat(64), manifest, &[], &[]),
            Err(Error::Protection)
        ));
    });
    // A missing source must fail before creating the migrating gate or intent.
    assert_eq!(fs::read_dir(&parent).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&home).unwrap().count(), 1);
}
#[test]
#[ignore = "requires a quiescent disposable OS account; v1.x MCP M1 acceptance"]
fn coordinator_default_data_root_allows_sibling_writes_but_closes_local_membership() {
    let (fixture, data, session) = fixture_at_default(true);
    let home = fixture.guard.verified_binding().unwrap().0.to_owned();
    let source = SourcePreflightV2::inspect(&home).unwrap();
    let owner = fixture._root;
    let admission = MigrationAdmissionV2::assemble(
        source,
        fixture.guard,
        TransportRootDiscoveryV2::inspect(std::slice::from_ref(&data), &[]).unwrap(),
    )
    .unwrap();
    // Only this test's disposable siblings; durability/intent decoding is not
    // asserted here. Physical writers will pin their own named objects.
    fs::write(data.join("fixture-intent"), b"owned fixture").unwrap();
    directory(&data.join("fixture-staging"));
    admission.verify_before_retire().unwrap();
    assert!(session.lock().is_err());
    directory(&data.join("sessions-v2/00000000-0000-4000-8000-000000000002"));
    assert!(admission.verify_before_retire().is_err());
    assert!(home.join(".polaris/mcp").is_dir());
    assert!(!home.join(".polaris/mcp-v2").exists());
    drop(admission);
    session.lock().unwrap();
    drop(owner);
}
#[test]
#[ignore = "requires a quiescent disposable OS account; v1.x MCP M1 acceptance"]
fn coordinator_retains_storage_and_entry_exclusion_through_fresh_checks() {
    let (fixture, data, session) = fixture();
    let home = fixture.guard.verified_binding().unwrap().0.to_owned();
    let source = SourcePreflightV2::inspect(&home).unwrap();
    // Keep the fixture's directory owner live while transferring only its guard.
    let owner = fixture._root;
    let admission = MigrationAdmissionV2::assemble(
        source,
        fixture.guard,
        TransportRootDiscoveryV2::inspect(&[data], &[]).unwrap(),
    )
    .unwrap();
    admission.verify_before_retire().unwrap();
    assert!(session.lock().is_err());
    assert!(
        polaris_mcp::entry_gate::ProductionEntryLeaseV2::enter(
            &home,
            &std::env::current_exe().unwrap()
        )
        .is_err()
    );
    assert!(home.join(".polaris/mcp").is_dir());
    assert!(!home.join(".polaris/mcp-v2").exists());
    drop(admission);
    session.lock().unwrap();
    // Failure/drop keeps Migrating; it does not restore Active or remove evidence.
    assert!(
        polaris_mcp::entry_gate::ProductionEntryLeaseV2::enter(
            &home,
            &std::env::current_exe().unwrap()
        )
        .is_err()
    );
    drop(owner);
}

#[test]
#[ignore = "requires a quiescent disposable OS account; v1.x MCP M1 acceptance"]
fn coordinator_closes_storage_change_during_native_stop_recheck() {
    let (fixture, data, session) = fixture();
    let home = fixture.guard.verified_binding().unwrap().0.to_owned();
    let source = SourcePreflightV2::inspect(&home).unwrap();
    let owner = fixture._root;
    let admission = MigrationAdmissionV2::assemble(
        source,
        fixture.guard,
        TransportRootDiscoveryV2::inspect(std::slice::from_ref(&data), &[]).unwrap(),
    )
    .unwrap();
    let reached = std::rc::Rc::new(std::cell::Cell::new(false));
    let hit = reached.clone();
    AFTER_STOP_CHECK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            directory(&data.join("sessions-v2/00000000-0000-4000-8000-000000000002"));
            hit.set(true);
        }))
    });
    assert!(admission.verify_before_retire().is_err());
    assert!(reached.get());
    assert!(session.lock().is_err());
    assert!(home.join(".polaris/mcp").is_dir());
    assert!(!home.join(".polaris/mcp-v2").exists());
    drop(admission);
    session.lock().unwrap();
    drop(owner);
}
#[test]
#[ignore = "requires a quiescent disposable OS account; v1.x MCP M1 acceptance"]
fn coordinator_closes_project_discovery_change_during_native_stop_recheck() {
    let (fixture, data, session) = fixture();
    directory(&data.join("state"));
    directory(&data.join("state/0000000000000001"));
    let home = fixture.guard.verified_binding().unwrap().0.to_owned();
    let source = SourcePreflightV2::inspect(&home).unwrap();
    let owner = fixture._root;
    let discovery = TransportRootDiscoveryV2::inspect(std::slice::from_ref(&data), &[]).unwrap();
    let admission = MigrationAdmissionV2::assemble(source, fixture.guard, discovery).unwrap();
    let reached = std::rc::Rc::new(std::cell::Cell::new(false));
    let hit = reached.clone();
    AFTER_STOP_CHECK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            directory(&data.join("state/0000000000000002"));
            hit.set(true);
        }))
    });
    assert!(admission.verify_before_retire().is_err());
    assert!(reached.get());
    assert!(session.lock().is_err());
    assert!(home.join(".polaris/mcp").is_dir());
    assert!(!home.join(".polaris/mcp-v2").exists());
    drop(admission);
    session.lock().unwrap();
    drop(owner);
}

#[test]
#[ignore = "requires a quiescent disposable OS account; v1.x MCP M1 acceptance"]
fn coordinator_closes_source_and_gate_change_after_native_stop_recheck() {
    for source_changed in [true, false] {
        let (fixture, data, session) = fixture();
        let home = fixture.guard.verified_binding().unwrap().0.to_owned();
        let source = SourcePreflightV2::inspect(&home).unwrap();
        let owner = fixture._root;
        let admission = MigrationAdmissionV2::assemble(
            source,
            fixture.guard,
            TransportRootDiscoveryV2::inspect(&[data], &[]).unwrap(),
        )
        .unwrap();
        let target = if source_changed {
            home.join(".polaris/mcp/unknown")
        } else {
            home.join(".polaris").join(GATE_FILE)
        };
        let reached = std::rc::Rc::new(std::cell::Cell::new(false));
        let hit = reached.clone();
        AFTER_STOP_CHECK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                fs::write(target, b"unexpected").unwrap();
                hit.set(true);
            }))
        });
        assert!(admission.verify_before_retire().is_err());
        assert!(reached.get());
        assert!(session.lock().is_err());
        assert!(home.join(".polaris/mcp").is_dir());
        assert!(!home.join(".polaris/mcp-v2").exists());
        drop(admission);
        session.lock().unwrap();
        drop(owner);
    }
}

#[test]
#[ignore = "requires a quiescent disposable OS account; v1.x MCP M1 acceptance"]
fn coordinator_closes_legacy_body_change_after_fresh_native_stop() {
    let (fixture, data, session) = fixture();
    let home = fixture.guard.verified_binding().unwrap().0.to_owned();
    directory(&data.join("sessions"));
    let legacy = data.join("sessions/legacy.jsonl");
    fs::write(&legacy, b"{\"role\":\"user\",\"content\":\"saved\"}\n").unwrap();
    let source = SourcePreflightV2::inspect(&home).unwrap();
    let owner = fixture._root;
    let admission = MigrationAdmissionV2::assemble(
        source,
        fixture.guard,
        TransportRootDiscoveryV2::inspect(&[data], &[]).unwrap(),
    )
    .unwrap();
    let reached = std::rc::Rc::new(std::cell::Cell::new(false));
    let hit = reached.clone();
    AFTER_STOP_CHECK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move || {
            // Body writes do not change the containing directory's membership.
            fs::write(&legacy, b"{\"role\":\"user\",\"content\":\"drift\"}\n").unwrap();
            hit.set(true);
        }));
    });
    assert!(admission.verify_before_retire().is_err());
    assert!(reached.get());
    assert!(session.lock().is_err());
    assert!(home.join(".polaris/mcp").is_dir());
    assert!(!home.join(".polaris/mcp-v2").exists());
    drop(admission);
    session.lock().unwrap();
    drop(owner);
}

#[test]
#[ignore = "requires a quiescent disposable OS account; v1.x MCP M1 acceptance"]
fn coordinator_closes_management_body_change_after_fresh_native_stop() {
    use crate::{management_effect::*, project_mutation::ProjectMutationOwnerV1};
    use std::{os::unix::fs::MetadataExt, str::FromStr};
    for namespace_changed in [true, false] {
        let (fixture, data, session) = fixture();
        let home = fixture.guard.verified_binding().unwrap().0.to_owned();
        let project = home.join("project");
        directory(&project);
        directory(&project.join(".polaris"));
        directory(&data.join("state"));
        let state = data
            .join("state")
            .join(crate::project::project_id(&project));
        directory(&state);
        let project_owner =
            ProjectMutationOwnerV1::at_store_root(&state.join("rewind-workspace")).unwrap();
        let lease = project_owner.try_mutation().unwrap();
        let store = ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
        let meta = project.metadata().unwrap();
        let namespace = state.join("rewind-workspace/namespace.json");
        fs::write(
            &namespace,
            serde_json::to_vec(&serde_json::json!({
                "version": 1, "project_root": project,
                "root_device": meta.dev(), "root_inode": meta.ino()
            }))
            .unwrap(),
        )
        .unwrap();
        fs::set_permissions(&namespace, fs::Permissions::from_mode(0o600)).unwrap();
        let request = PrepareManagementEffectV1 {
            operation_id: ManagementOperationIdV1::from_str(&"1".repeat(32)).unwrap(),
            inventory: ManagementInventoryIdV1::parse("T10").unwrap(),
            scope: ManagementEffectScopeV1 {
                project: store.project_scope_hash(),
                session: Some(ManagementEffectHashV1::digest(b"session")),
                generation: 1,
                revision: 1,
                permission: ManagementPermissionScopeV1::Project,
                account_binding: None,
            },
            request_hash: ManagementEffectHashV1::digest(b"request"),
            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
        };
        let PrepareManagementEffectOutcomeV1::Prepared(prepared) = store
            .prepare(&lease, store.cursor().unwrap(), request)
            .unwrap()
        else {
            panic!("new")
        };
        let started = store.start(&lease, &prepared).unwrap();
        store
            .complete_started(&lease, &started, ManagementEffectHashV1::digest(b"receipt"))
            .unwrap();
        drop(lease);
        let source = SourcePreflightV2::inspect(&home).unwrap();
        let owner = fixture._root;
        let admission = MigrationAdmissionV2::assemble(
            source,
            fixture.guard,
            TransportRootDiscoveryV2::inspect(&[data], &[]).unwrap(),
        )
        .unwrap();
        let target = if namespace_changed {
            namespace
        } else {
            state.join("management-effects-v1/journal.v1.jsonl")
        };
        let before = fs::read(&target).unwrap();
        let reached = std::rc::Rc::new(std::cell::Cell::new(false));
        let hit = reached.clone();
        AFTER_STOP_CHECK.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                // Identical bytes leave all semantic hashes and names unchanged.
                fs::write(&target, before).unwrap();
                hit.set(true);
            }));
        });
        assert!(admission.verify_before_retire().is_err());
        assert!(reached.get());
        assert!(project_owner.try_mutation().is_err());
        assert!(session.lock().is_err());
        let lock = fs::File::open(state.join("management-effects-v1/owner.lock")).unwrap();
        assert!(matches!(
            lock.try_lock(),
            Err(std::fs::TryLockError::WouldBlock)
        ));
        assert!(!state.join("management-effects-v2").exists());
        assert!(!home.join(".polaris/mcp-v2").exists());
        drop(admission);
        project_owner.try_mutation().unwrap();
        lock.try_lock().unwrap();
        session.lock().unwrap();
        drop(owner);
    }
}
