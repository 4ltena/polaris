//! Migration source preflight inventory and production manifest validation tests.
use super::super::entry_gate::{
    ProductionBinaryIdentityV2, ProductionEntryIdentityV2, ProductionEntryKindV2,
    ProductionEntryManifestV2,
};
use super::*;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

type SnapshotEntry = (
    PathBuf,
    (u64, u64, u32, u64, i64, i64, i64, i64),
    Option<Vec<u8>>,
);

struct Fixture {
    _root: tempfile::TempDir,
    _retained: Vec<tempfile::TempDir>,
    home: PathBuf,
    store: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let mut retained = Vec::new();
        for generation in 1..=32 {
            let mut fixture = Self::candidate();
            let collision = polaris_auth::protection::with_protected_paths(|registered| {
                [
                    "servers",
                    "servers/unregistered",
                    "servers/unregistered/index.js",
                ]
                .iter()
                .any(|relative| {
                    let metadata = std::fs::metadata(fixture.store.join(relative)).unwrap();
                    registered
                        .identities()
                        .contains(&polaris_auth::protection::ProtectedIdentity {
                            dev: metadata.dev(),
                            ino: metadata.ino(),
                        })
                })
            })
            .unwrap();
            if collision {
                eprintln!(
                    "preflight fixture generation={generation} retired_identity_collision=true"
                );
                // Retain the colliding synthetic inodes until this fixture ends.
                // Production protection and the tests' later mutations remain unchanged.
                retained.push(fixture._root);
                continue;
            }
            fixture._retained = retained;
            return fixture;
        }
        panic!("preflight fixture exhausted 32 private tree candidates")
    }
    fn candidate() -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        let store = home.join(".polaris/mcp");
        for path in [
            &home,
            &home.join(".polaris"),
            &store,
            &store.join("servers"),
            &store.join("servers/unregistered"),
        ] {
            std::fs::create_dir(path).unwrap();
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let fixture = Self {
            _root: root,
            _retained: Vec::new(),
            home,
            store,
        };
        fixture.write(
            "servers.json",
            br#"{"schema_version":1,"revision":7,"servers":[]}"#,
        );
        fixture.write(
            "secrets.json",
            br#"{"schema_version":1,"revision":9,"values":{"owned_ref":"fixture-secret"}}"#,
        );
        fixture.write("catalog.json", br#"{"schema_version":1,"servers":{}}"#);
        fixture.write("owner.lock", b"");
        fixture.write("servers/unregistered/index.js", b"package payload");
        fixture
    }
    fn write(&self, relative: &str, bytes: &[u8]) {
        let path = self.store.join(relative);
        std::fs::write(&path, bytes).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
    }
    fn manifest(&self) -> ProductionEntryManifestV2 {
        let binary = self._root.path().join("owned-fixture-entry");
        std::fs::write(&binary, b"synthetic entry identity\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o500)).unwrap();
        ProductionEntryManifestV2 {
            protocol_version: 2,
            entries: vec![ProductionEntryIdentityV2 {
                kind: ProductionEntryKindV2::Cli,
                binary: ProductionBinaryIdentityV2::inspect(&binary).unwrap(),
            }],
            runtime_copy_source_hashes: vec![],
        }
    }
    fn snapshot(&self) -> Vec<SnapshotEntry> {
        let mut pending = vec![self.store.clone()];
        let mut result = vec![];
        while let Some(path) = pending.pop() {
            let m = path.symlink_metadata().unwrap();
            if m.is_dir() {
                pending.extend(std::fs::read_dir(&path).unwrap().map(|e| e.unwrap().path()));
            }
            let bytes = m.is_file().then(|| std::fs::read(&path).unwrap());
            result.push((
                path.strip_prefix(&self.store).unwrap().to_path_buf(),
                (
                    m.dev(),
                    m.ino(),
                    m.mode(),
                    m.nlink(),
                    m.mtime(),
                    m.mtime_nsec(),
                    m.ctime(),
                    m.ctime_nsec(),
                ),
                bytes,
            ));
        }
        result.sort_by(|a, b| a.0.cmp(&b.0));
        result
    }
}

#[test]
fn source_preflight_is_read_only_and_binds_pre_gate_source_to_actual_guard() {
    let fixture = Fixture::new();
    let before = fixture.snapshot();
    let source = SourcePreflightV2::inspect(&fixture.home).unwrap();
    source.verify_current().unwrap();
    assert_eq!(before, fixture.snapshot());
    assert_eq!(
        source
            .documents
            .verified_registry()
            .unwrap()
            .unwrap()
            .revision,
        7
    );
    assert!(
        !fixture
            .home
            .join(".polaris")
            .join(polaris_mcp::entry_gate::GATE_FILE)
            .exists()
    );
    let guard =
        EntryMigrationGuardV2::begin(&fixture.home, &"a".repeat(64), fixture.manifest()).unwrap();
    source.bind_guard(&guard).unwrap();
    assert_eq!(before, fixture.snapshot());
    assert!(!fixture.home.join(".polaris/mcp-v2").exists());
}

fn retire_fixture(
    fixture: &Fixture,
    source: &SourcePreflightV2,
    guard: &EntryMigrationGuardV2,
) -> polaris_mcp::migration_observation::RetiredRecordV2 {
    use polaris_mcp::migration_observation::*;
    use polaris_mcp::store::ledger_v1::{RetiredLedgerAnchorV1, TombstoneLedgerAnchorV1};
    use sha2::{Digest, Sha256};
    let (_, id, manifest, hash) = guard.verified_binding().unwrap();
    let binding = MigrationIntentBindingV2 {
        schema_version: 2,
        migration_id: id.into(),
        store_identity: "b".repeat(64),
        source_store_identity: None,
        layout: ProtocolLayoutV2::new(id).unwrap(),
        canonical_parent_anchor: EntryAnchorV2::of(&guard.parent.metadata().unwrap()),
        source_root_anchor: EntryAnchorV2::of(&source.root.metadata().unwrap()),
        source_non_secret_tree_hash: source.inventory.digest().into(),
        entry_manifest: manifest.clone(),
        entry_manifest_hash: hash.into(),
        // Historical DTO only. This fixture tests reobservation, not stopping.
        stop: StopObservationV2 {
            result: StopObservationResultV2::ZeroProductionProcesses,
            coordinator: manifest.entries[0].binary.clone(),
            inspected_processes: 1,
        },
    };
    let name = binding.layout.retired.component();
    let initial = ExternalMigrationIntentV2::retire_prepared(binding).unwrap();
    std::fs::rename(&fixture.store, fixture.home.join(".polaris").join(name)).unwrap();
    let bytes = b"disposable fixture tombstone";
    std::fs::write(&fixture.store, bytes).unwrap();
    std::fs::set_permissions(&fixture.store, std::fs::Permissions::from_mode(0o600)).unwrap();
    let tombstone = TombstoneLedgerAnchorV1 {
        anchor: EntryAnchorV2::of(&fixture.store.metadata().unwrap()),
        file_hash: format!("{:x}", Sha256::digest(bytes)),
    };
    let ExternalMigrationIntentV2::Retired { record } = initial
        .retired(
            EntryAnchorV2::of(&guard.parent.metadata().unwrap()),
            RetiredLedgerAnchorV1 {
                anchor: EntryAnchorV2::of(&source.root.metadata().unwrap()),
                non_secret_tree_hash: source.inventory.digest().into(),
            },
            tombstone,
        )
        .unwrap()
    else {
        panic!("retired")
    };
    record
}

#[test]
fn preflight_retired_reobservation_keeps_original_source_and_full_closing() {
    let fixture = Fixture::new();
    let source = SourcePreflightV2::inspect(&fixture.home).unwrap();
    let guard =
        EntryMigrationGuardV2::begin(&fixture.home, &"a".repeat(64), fixture.manifest()).unwrap();
    let record = retire_fixture(&fixture, &source, &guard);
    let retired = fixture
        .home
        .join(".polaris")
        .join(record.binding.layout.retired.component());
    let rebound = source.inspect_retired(&guard, &record).unwrap();
    rebound.bind_guard(&guard).unwrap();
    assert_eq!(
        rebound
            .documents
            .verified_registry()
            .unwrap()
            .unwrap()
            .revision,
        7
    );
    let secret = retired.join("secrets.json");
    let bytes = std::fs::read(&secret).unwrap();
    std::fs::write(secret, bytes).unwrap();
    assert!(rebound.verify_current().is_err());
    guard.verify_migrating().unwrap();
    assert!(!fixture.home.join(".polaris/mcp-v2").exists());
}

#[test]
fn preflight_retired_reobservation_rejects_same_bytes_child_drift_and_foreign_root() {
    for mutation in 0..5 {
        let fixture = Fixture::new();
        let source = SourcePreflightV2::inspect(&fixture.home).unwrap();
        let guard =
            EntryMigrationGuardV2::begin(&fixture.home, &"a".repeat(64), fixture.manifest())
                .unwrap();
        let record = retire_fixture(&fixture, &source, &guard);
        let retired = fixture
            .home
            .join(".polaris")
            .join(record.binding.layout.retired.component());
        if mutation < 3 {
            let target = retired.join(
                [
                    "secrets.json",
                    "servers/unregistered/index.js",
                    "catalog.json",
                ][mutation],
            );
            let bytes = std::fs::read(&target).unwrap();
            std::fs::write(target, bytes).unwrap();
        } else if mutation == 3 {
            let root = std::fs::File::open(&retired).unwrap();
            let modified = root.metadata().unwrap().modified().unwrap();
            let lock = retired.join("owner.lock");
            std::fs::rename(&lock, fixture.home.join("saved-owner-lock")).unwrap();
            std::fs::write(&lock, b"").unwrap();
            std::fs::set_permissions(&lock, std::fs::Permissions::from_mode(0o600)).unwrap();
            root.set_times(std::fs::FileTimes::new().set_modified(modified))
                .unwrap();
        } else {
            std::fs::rename(&retired, fixture.home.join(".polaris/saved-retired")).unwrap();
            std::fs::create_dir(&retired).unwrap();
            std::fs::set_permissions(&retired, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        assert!(
            source.inspect_retired(&guard, &record).is_err(),
            "{mutation}"
        );
        guard.verify_migrating().unwrap();
        assert!(!fixture.home.join(".polaris/mcp-v2").exists());
    }
}

#[test]
fn source_preflight_blocks_unknown_temp_links_modes_and_special_files_without_repair() {
    for mutation in 0..7 {
        let fixture = Fixture::new();
        match mutation {
            0 => fixture.write("secrets.json.tmp", b"unfinished fixture"),
            1 => fixture.write("unknown.json", b"unknown fixture"),
            2 => fixture.write("owner.lock", b"not an empty lock"),
            3 => std::fs::hard_link(
                fixture.store.join("servers/unregistered/index.js"),
                fixture.store.join("servers/unregistered/alias.js"),
            )
            .unwrap(),
            4 => std::os::unix::fs::symlink(
                "missing",
                fixture.store.join("servers/unregistered/link"),
            )
            .unwrap(),
            5 => std::fs::set_permissions(
                fixture.store.join("servers.json"),
                std::fs::Permissions::from_mode(0o644),
            )
            .unwrap(),
            _ => {
                use std::os::unix::ffi::OsStrExt;
                let path = std::ffi::CString::new(
                    fixture
                        .store
                        .join("servers/unregistered/pipe")
                        .as_os_str()
                        .as_bytes(),
                )
                .unwrap();
                assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
            }
        }
        let before = fixture.snapshot();
        assert!(SourcePreflightV2::inspect(&fixture.home).is_err());
        assert_eq!(before, fixture.snapshot());
    }
}

#[test]
fn source_preflight_blocks_invalid_catalog_registry_and_secret_document() {
    for (name, bytes) in [
        (
            "catalog.json",
            br#"{"schema_version":2,"servers":{}}"#.as_slice(),
        ),
        (
            "servers.json",
            br#"{"schema_version":1,"revision":0,"servers":[]}"#.as_slice(),
        ),
        (
            "secrets.json",
            br#"{"schema_version":1,"revision":0,"values":{}}"#.as_slice(),
        ),
    ] {
        let fixture = Fixture::new();
        fixture.write(name, bytes);
        let before = fixture.snapshot();
        assert!(SourcePreflightV2::inspect(&fixture.home).is_err());
        assert_eq!(before, fixture.snapshot());
    }
}

#[test]
fn source_preflight_rejects_root_parent_catalog_and_payload_drift() {
    for mutation in 0..4 {
        let fixture = Fixture::new();
        let source = SourcePreflightV2::inspect(&fixture.home).unwrap();
        match mutation {
            0 => {
                std::fs::rename(&fixture.store, fixture.home.join(".polaris/saved-mcp")).unwrap();
                std::fs::create_dir(&fixture.store).unwrap();
                std::fs::set_permissions(&fixture.store, std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            }
            1 => {
                std::fs::rename(
                    fixture.home.join(".polaris"),
                    fixture.home.join("saved-parent"),
                )
                .unwrap();
                std::fs::create_dir(fixture.home.join(".polaris")).unwrap();
                std::fs::set_permissions(
                    fixture.home.join(".polaris"),
                    std::fs::Permissions::from_mode(0o700),
                )
                .unwrap();
            }
            2 => fixture.write("catalog.json", br#"{"schema_version":1,"servers":{}} "#),
            _ => fixture.write("servers/unregistered/index.js", b"changed payload"),
        }
        assert!(source.verify_current().is_err());
    }
}

#[test]
fn source_preflight_blocks_package_secret_names_and_registered_missing_paths() {
    for name in [".ENV", ".env.local", ".npmrc", "private.pem"] {
        let fixture = Fixture::new();
        fixture.write(
            &format!("servers/unregistered/{name}"),
            b"fixture sensitive payload",
        );
        let before = fixture.snapshot();
        assert!(SourcePreflightV2::inspect(&fixture.home).is_err());
        assert_eq!(before, fixture.snapshot());
    }
    let fixture = Fixture::new();
    let reserved = fixture
        .store
        .join("servers/unregistered/reserved-secret.json");
    drop(polaris_auth::protection::protect_store(&reserved).unwrap());
    assert!(!reserved.exists());
    assert!(SourcePreflightV2::inspect(&fixture.home).is_err());
}

#[test]
fn source_preflight_blocks_new_protection_and_retired_inode_after_capture() {
    let fixture = Fixture::new();
    let source = SourcePreflightV2::inspect(&fixture.home).unwrap();
    let payload = fixture.store.join("servers/unregistered/index.js");
    drop(polaris_auth::protection::protect_store(&payload).unwrap());
    assert!(source.verify_current().is_err());
    // The protection snapshot keeps retired identities even after rename.
    let renamed = fixture.store.join("servers/unregistered/renamed.js");
    std::fs::rename(&payload, &renamed).unwrap();
    assert!(SourcePreflightV2::inspect(&fixture.home).is_err());
}

#[test]
fn source_preflight_trusted_home_pins_guard_and_rejects_account_drift() {
    use super::super::account_home::{TrustedUserHomeV2, with_fixture_home};
    use std::sync::Arc;
    let fixture = Fixture::new();
    let manifest = fixture.manifest();
    with_fixture_home(&fixture.home, || {
        let home = Arc::new(TrustedUserHomeV2::inspect().unwrap());
        let before = fixture.snapshot();
        let source = SourcePreflightV2::inspect_trusted(home.clone()).unwrap();
        assert_eq!(before, fixture.snapshot());
        let guard =
            EntryMigrationGuardV2::begin_trusted(home.clone(), &"a".repeat(64), manifest.clone())
                .unwrap();
        source.bind_guard(&guard).unwrap();
        assert_eq!(before, fixture.snapshot());
        with_fixture_home(&fixture._root.path().join("different-account"), || {
            assert!(source.verify_current().is_err());
            assert!(source.bind_guard(&guard).is_err());
            assert!(guard.verified_binding().is_err());
            assert!(guard.prove_stopped().is_err());
            assert!(
                EntryMigrationGuardV2::begin_trusted(
                    home.clone(),
                    &"a".repeat(64),
                    manifest.clone()
                )
                .is_err()
            );
        });
    });
}
#[test]
fn source_preflight_trusted_home_rejects_home_replacement_and_mode_drift() {
    use super::super::account_home::{TrustedUserHomeV2, with_fixture_home};
    use std::sync::Arc;
    for replace in [false, true] {
        let fixture = Fixture::new();
        with_fixture_home(&fixture.home, || {
            let home = Arc::new(TrustedUserHomeV2::inspect().unwrap());
            let source = SourcePreflightV2::inspect_trusted(home.clone()).unwrap();
            let guard =
                EntryMigrationGuardV2::begin_trusted(home, &"a".repeat(64), fixture.manifest())
                    .unwrap();
            if replace {
                std::fs::rename(&fixture.home, fixture._root.path().join("retired-home")).unwrap();
                std::fs::create_dir(&fixture.home).unwrap();
                std::fs::set_permissions(&fixture.home, std::fs::Permissions::from_mode(0o700))
                    .unwrap();
            } else {
                std::fs::set_permissions(&fixture.home, std::fs::Permissions::from_mode(0o755))
                    .unwrap();
            }
            assert!(source.verify_current().is_err());
            assert!(guard.verified_binding().is_err());
            assert!(source.bind_guard(&guard).is_err());
        });
    }
}

#[test]
fn source_preflight_trusted_home_never_binds_a_different_account_guard() {
    use super::super::account_home::{TrustedUserHomeV2, with_fixture_home};
    use std::sync::Arc;
    let first = Fixture::new();
    let second = Fixture::new();
    let source = with_fixture_home(&first.home, || {
        SourcePreflightV2::inspect_trusted(Arc::new(TrustedUserHomeV2::inspect().unwrap())).unwrap()
    });
    let guard = with_fixture_home(&second.home, || {
        EntryMigrationGuardV2::begin_trusted(
            Arc::new(TrustedUserHomeV2::inspect().unwrap()),
            &"a".repeat(64),
            second.manifest(),
        )
        .unwrap()
    });
    with_fixture_home(&second.home, || assert!(source.bind_guard(&guard).is_err()));
    with_fixture_home(&first.home, || source.verify_current().unwrap());
}
