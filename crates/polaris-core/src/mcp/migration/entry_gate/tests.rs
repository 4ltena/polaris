//! Migration entry-gate identity, binding and fail-closed regression tests.
use super::*;
use polaris_mcp::entry_gate::ProductionEntryLeaseV2;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    binary: PathBuf,
    manifest: ProductionEntryManifestV2,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let binary = root.path().join("polaris");
        std::fs::write(&binary, b"synthetic executable identity\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o500)).unwrap();
        let manifest = ProductionEntryManifestV2 {
            protocol_version: 2,
            entries: vec![ProductionEntryIdentityV2 {
                kind: ProductionEntryKindV2::Cli,
                binary: ProductionBinaryIdentityV2::inspect(&binary).unwrap(),
            }],
            runtime_copy_source_hashes: vec![],
        };
        Self {
            _root: root,
            home,
            binary,
            manifest,
        }
    }
    fn active(&self) {
        let migration =
            EntryMigrationGuardV2::begin(&self.home, &"a".repeat(64), self.manifest.clone())
                .unwrap();
        let gate = ProductionEntryGateV2::ActiveV2 {
            protocol_version: 2,
            migration_id: "a".repeat(64),
            entry_manifest_hash: self.manifest.canonical_hash().unwrap(),
            active_intent_hash: "b".repeat(64),
        };
        let bytes = serde_json::to_vec(&gate).unwrap();
        // Test-only publication. Production has no activation API until the
        // complete protocol migration owner provides its verified intent.
        let mut file = child(&migration.parent, GATE_FILE, libc::O_WRONLY | libc::O_TRUNC).unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
        migration.parent.sync_all().unwrap();
    }
}
#[test]
fn legacy_absence_is_read_only_and_partial_gate_fails_closed() {
    let fixture = Fixture::new();
    let lease = ProductionEntryLeaseV2::enter(&fixture.home, &fixture.binary).unwrap();
    assert!(!lease.is_active_v2());
    assert!(!fixture.home.join(".polaris").exists());
    let guard =
        EntryMigrationGuardV2::begin(&fixture.home, &"a".repeat(64), fixture.manifest.clone())
            .unwrap();
    assert!(matches!(lease.verify(), Err(EntryGateErrorV2::InvalidGate)));
    drop(guard);
    assert!(matches!(
        ProductionEntryLeaseV2::enter(&fixture.home, &fixture.binary),
        Err(EntryGateErrorV2::Migrating)
    ));
}

#[test]
fn borrowed_migration_binding_rechecks_and_returns_the_actual_guard_without_writes() {
    let fixture = Fixture::new();
    let migration_id = "a".repeat(64);
    let guard =
        EntryMigrationGuardV2::begin(&fixture.home, &migration_id, fixture.manifest.clone())
            .unwrap();
    let gate_path = fixture.home.join(".polaris").join(GATE_FILE);
    let manifest_path = fixture.home.join(".polaris").join(ENTRY_MANIFEST);
    let gate_before = std::fs::read(&gate_path).unwrap();
    let manifest_before = std::fs::read(&manifest_path).unwrap();
    let (home, id, manifest, hash) = guard.verified_binding().unwrap();
    assert_eq!(home, fixture.home.as_path());
    assert_eq!(id, migration_id);
    assert!(std::ptr::eq(manifest, &guard.manifest));
    assert_eq!(manifest, &fixture.manifest);
    assert_eq!(hash, fixture.manifest.canonical_hash().unwrap());
    assert_eq!(std::fs::read(gate_path).unwrap(), gate_before);
    assert_eq!(std::fs::read(manifest_path).unwrap(), manifest_before);
}

#[test]
fn borrowed_migration_binding_rejects_tampered_migrating_evidence_before_returning() {
    let fixture = Fixture::new();
    let guard =
        EntryMigrationGuardV2::begin(&fixture.home, &"a".repeat(64), fixture.manifest.clone())
            .unwrap();
    let changed_gate = ProductionEntryGateV2::MigratingV2 {
        protocol_version: 2,
        migration_id: "b".repeat(64),
        entry_manifest_hash: fixture.manifest.canonical_hash().unwrap(),
    };
    let changed_bytes = serde_json::to_vec(&changed_gate).unwrap();
    let gate_path = fixture.home.join(".polaris").join(GATE_FILE);
    std::fs::write(&gate_path, &changed_bytes).unwrap();
    assert!(guard.verified_binding().is_err());
    assert_eq!(std::fs::read(gate_path).unwrap(), changed_bytes);
}
#[test]
fn active_gate_requires_exact_binary_identity_and_holds_shared_lease() {
    let fixture = Fixture::new();
    fixture.active();
    let lease = ProductionEntryLeaseV2::enter(&fixture.home, &fixture.binary).unwrap();
    assert!(lease.is_active_v2());
    lease.verify().unwrap();
    assert!(matches!(
        EntryMigrationGuardV2::begin(&fixture.home, &"a".repeat(64), fixture.manifest.clone()),
        Err(EntryGateErrorV2::Busy)
    ));
    assert!(lease.isolated_read_files().contains(&fixture.binary));
    drop(lease);
    std::fs::set_permissions(&fixture.binary, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(&fixture.binary, b"replaced").unwrap();
    assert!(ProductionEntryLeaseV2::enter(&fixture.home, &fixture.binary).is_err());
}
#[test]
fn symlink_hardlink_unknown_fields_and_bad_mode_never_admit() {
    let fixture = Fixture::new();
    fixture.active();
    let path = fixture.home.join(".polaris").join(GATE_FILE);
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(ProductionEntryLeaseV2::enter(&fixture.home, &fixture.binary).is_err());
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    let alias = fixture.home.join("gate-hardlink");
    std::fs::hard_link(&path, &alias).unwrap();
    assert!(ProductionEntryLeaseV2::enter(&fixture.home, &fixture.binary).is_err());
    std::fs::remove_file(&alias).unwrap();
    let original = std::fs::read(&path).unwrap();
    let mut gate: serde_json::Value = serde_json::from_slice(&original).unwrap();
    gate["ignored"] = true.into();
    std::fs::write(&path, serde_json::to_vec(&gate).unwrap()).unwrap();
    assert!(ProductionEntryLeaseV2::enter(&fixture.home, &fixture.binary).is_err());
    std::fs::write(&path, &original).unwrap();
    std::fs::remove_file(&path).unwrap();
    std::os::unix::fs::symlink(&alias, &path).unwrap();
    assert!(ProductionEntryLeaseV2::enter(&fixture.home, &fixture.binary).is_err());
}
#[test]
fn runtime_copy_requires_digest_and_private_anchors() {
    let mut fixture = Fixture::new();
    fixture
        .manifest
        .runtime_copy_source_hashes
        .push(fixture.manifest.entries[0].binary.sha256.clone());
    fixture.active();
    let run = fixture._root.path().join("run");
    let runtime = run.join("runtime");
    std::fs::create_dir(&run).unwrap();
    std::fs::create_dir(&runtime).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
    let copy = runtime.join("polaris");
    std::fs::copy(&fixture.binary, &copy).unwrap();
    std::fs::set_permissions(&copy, std::fs::Permissions::from_mode(0o500)).unwrap();
    std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o500)).unwrap();
    let lease = ProductionEntryLeaseV2::enter(&fixture.home, &copy).unwrap();
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(
        lease.verify().is_err(),
        "an admitted runtime must retain its private parent"
    );
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o700)).unwrap();
    lease.verify().unwrap();
    lease.verify().unwrap();
    drop(lease);
    std::fs::set_permissions(&run, std::fs::Permissions::from_mode(0o755)).unwrap();
    assert!(ProductionEntryLeaseV2::enter(&fixture.home, &copy).is_err());
}
#[test]
fn entry_lock_replacement_is_detected_without_untrusted_state_repair() {
    let fixture = Fixture::new();
    fixture.active();
    let lease = ProductionEntryLeaseV2::enter(&fixture.home, &fixture.binary).unwrap();
    let lock = fixture.home.join(".polaris").join(GATE_LOCK);
    std::fs::rename(&lock, lock.with_extension("retired")).unwrap();
    let file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&lock)
        .unwrap();
    file.sync_all().unwrap();
    assert!(lease.verify().is_err());
}
#[test]
fn live_entry_candidate_is_never_counted_as_stopped() {
    let fixture = Fixture::new();
    assert_eq!(
        process_inventory::reject_entry_path(&fixture.manifest, &fixture.binary),
        Err(EntryGateErrorV2::ProcessActive)
    );
    assert!(
        process_inventory::reject_entry_path(&fixture.manifest, Path::new("/usr/bin/unrelated"))
            .is_ok()
    );
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
#[test]
fn native_inventory_refuses_owned_live_entry_then_requires_complete_stop_proof() {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    std::fs::create_dir(&home).unwrap();
    std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
    let executable = root.path().join("polaris-owned-stop-probe");
    std::fs::copy("/bin/sleep", &executable).unwrap();
    #[cfg(target_os = "macos")]
    {
        // An Apple platform signature does not authorize relocation. Sign
        // only our disposable copy before binding its final bytes.
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let signed = std::process::Command::new("/usr/bin/codesign")
            .args(["--force", "--sign", "-", "--timestamp=none"])
            .arg(&executable)
            .output()
            .unwrap();
        assert!(signed.status.success(), "owned stop-probe signing failed");
    }
    std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o500)).unwrap();
    let manifest = ProductionEntryManifestV2 {
        protocol_version: 2,
        entries: vec![ProductionEntryIdentityV2 {
            kind: ProductionEntryKindV2::Cli,
            binary: ProductionBinaryIdentityV2::inspect(&executable).unwrap(),
        }],
        runtime_copy_source_hashes: vec![],
    };
    let mut child = std::process::Command::new(&executable)
        .arg("1")
        .spawn()
        .unwrap();
    let guard = EntryMigrationGuardV2::begin(&home, &"c".repeat(64), manifest).unwrap();
    let running = guard.prove_stopped();
    // Reap our child before assertions so an early failed observation cannot
    // strand a fixture. Other processes are never signalled by this test.
    let exit = child.wait().unwrap();
    assert!(
        exit.success(),
        "owned stop probe did not exit normally: {exit:?}"
    );
    assert!(matches!(running, Err(EntryGateErrorV2::ProcessActive)));
    let stopped = guard.prove_stopped().unwrap();
    stopped.verify_for(&guard).unwrap();
}

#[test]
fn trusted_migration_guard_closes_parent_replacement_after_content_read() {
    use super::super::account_home::{TrustedUserHomeV2, with_fixture_home};
    use std::sync::Arc;
    for prove_stopped in [false, true] {
        let fixture = Fixture::new();
        let foreign = fixture.home.join("foreign-parent");
        std::fs::create_dir(&foreign).unwrap();
        std::fs::set_permissions(&foreign, std::fs::Permissions::from_mode(0o700)).unwrap();
        let sentinel = foreign.join("sentinel");
        std::fs::write(&sentinel, b"untouched foreign parent").unwrap();
        with_fixture_home(&fixture.home, || {
            let home = Arc::new(TrustedUserHomeV2::inspect().unwrap());
            let guard = EntryMigrationGuardV2::begin_trusted(
                home,
                &"a".repeat(64),
                fixture.manifest.clone(),
            )
            .unwrap();
            let named = fixture.home.join(".polaris");
            let retired = fixture.home.join("retained-parent");
            let foreign = foreign.clone();
            AFTER_MIGRATING_CONTENT.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    std::fs::rename(&named, &retired).unwrap();
                    std::fs::rename(&foreign, &named).unwrap();
                }))
            });
            if prove_stopped {
                assert!(matches!(
                    guard.prove_stopped(),
                    Err(EntryGateErrorV2::InvalidGate)
                ));
            } else {
                assert!(matches!(
                    guard.verified_binding(),
                    Err(EntryGateErrorV2::InvalidGate)
                ));
            }
            assert!(AFTER_MIGRATING_CONTENT.with(|slot| slot.borrow().is_none()));
            assert_eq!(
                std::fs::read(fixture.home.join(".polaris/sentinel")).unwrap(),
                b"untouched foreign parent"
            );
            assert!(!fixture.home.join(".polaris").join(GATE_FILE).exists());
        });
    }
}
