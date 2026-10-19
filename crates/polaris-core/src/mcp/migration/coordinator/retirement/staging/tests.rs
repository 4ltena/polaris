//! Physical global staging and completed-project payload binding tests.
use super::*;
use polaris_mcp::registration::{Capabilities, ProtocolProfile, Server, Timeouts};
use std::{
    collections::BTreeSet,
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
};

#[test]
#[ignore = "requires a quiescent disposable OS account; v1.x MCP M1 acceptance"]
fn management_payload_binds_every_completed_project_and_rejects_structural_omission() {
    let (_fixture, admission, states) = super::super::tests::management_fixture_history(true);
    assert!(
        admission
            .roots
            .management_migrations(&"a".repeat(64))
            .is_err()
    );
    let staged = admission.retire_source().unwrap().stage_source().unwrap();
    staged.verify_current().unwrap();
    let record = &staged.payload.record;
    let rows = &record.management_migrations;
    assert_eq!(rows.len(), states.len());
    assert!(rows.windows(2).all(|r| r[0].state_path < r[1].state_path));
    for state in states {
        let row = rows
            .iter()
            .find(|row| row.state_path == state.as_os_str().as_encoded_bytes())
            .unwrap();
        let path = state.join(format!("management-effects-v2.staging-{}", "a".repeat(64)));
        let manifest_bytes = fs::read(path.join("migration.v2.json")).unwrap();
        let manifest: serde_json::Value = serde_json::from_slice(&manifest_bytes).unwrap();
        assert_eq!(row.migration_id, "a".repeat(64));
        assert_eq!(
            row.project_scope,
            manifest["project_scope"].as_str().unwrap()
        );
        assert_eq!(
            row.migration_manifest_hash,
            format!("{:x}", Sha256::digest(&manifest_bytes))
        );
        assert_eq!(
            row.planned_active_v2_root_anchor,
            EntryAnchorV2::of(&fs::metadata(&path).unwrap())
        );
        assert_eq!(
            row.canonical_parent_anchor,
            EntryAnchorV2::of(&fs::metadata(&state).unwrap())
        );
        assert_eq!(
            row.tombstone.file_hash,
            format!(
                "{:x}",
                Sha256::digest(fs::read(state.join("management-effects-v1")).unwrap())
            )
        );
    }
    StagedPayloadV2::verify_management(&staged.retired.admission, rows).unwrap();
    for kind in 0..4 {
        let mut candidate = record.clone();
        match kind {
            0 => {
                candidate.management_migrations.pop();
            }
            1 => candidate.management_migrations[0].project_scope = "b".repeat(64),
            2 => candidate.management_migrations[0].migration_manifest_hash = "c".repeat(64),
            _ => candidate.management_migrations[0].tombstone.file_hash = "d".repeat(64),
        }
        // Every candidate remains structurally valid. Only the actual held
        // project owner can reject the missing or forged non-secret binding.
        candidate.encode().unwrap();
        assert!(matches!(
            StagedPayloadV2::verify_management(
                &staged.retired.admission,
                &candidate.management_migrations
            ),
            Err(Error::Protection)
        ));
    }
    staged.verify_current().unwrap();
}

fn directory(path: &Path, mode: u32) {
    fs::create_dir(path).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
fn write(path: &Path, bytes: &[u8], mode: u32) {
    fs::write(path, bytes).unwrap();
    fs::set_permissions(path, fs::Permissions::from_mode(mode)).unwrap();
}
fn populated(admission: &mut MigrationAdmissionV2, parent: &Path) {
    let source = parent.join("mcp");
    for id in ["registered", "unregistered"] {
        let path = source.join("servers").join(id);
        directory(&path, 0o700);
        write(&path.join("runtime"), b"synthetic runtime bytes", 0o500);
        write(&path.join("index.js"), b"opaque package payload", 0o400);
        let pin = PinnedPathsV1 {
            schema_version: 1,
            package: format!("opaque-{}", source.display()),
            version: "1.0.0".into(),
            runtime: path.join("runtime"),
            entry: path.join("index.js"),
        };
        write(
            &path.join("pinned.json"),
            &serde_json::to_vec(&pin).unwrap(),
            0o600,
        );
        // A valid source need not be writable: staging children precede its
        // final permission restoration.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o500)).unwrap();
    }
    let path = source.join("servers/registered");
    let registry = Registry {
        schema_version: 1,
        revision: 7,
        servers: vec![
            Server {
                server_id: "http".into(),
                enabled_projects: BTreeSet::new(),
                transport: Destination::StreamableHttp {
                    url: "https://example.test/mcp".into(),
                    secret_headers: BTreeMap::from([("authorization".into(), "owned_ref".into())]),
                },
                protocol_profile: ProtocolProfile::Discover2026,
                capabilities: Capabilities::default(),
                timeouts: Timeouts::default(),
            },
            Server {
                server_id: "registered".into(),
                enabled_projects: BTreeSet::new(),
                transport: Destination::Stdio {
                    command: path.join("runtime"),
                    args: vec![path.join("index.js").to_str().unwrap().into()],
                    cwd: path.clone(),
                    sandbox_profile: "read-only".into(),
                    secret_env: BTreeMap::new(),
                    readable_roots: vec![],
                    executable_sha256: Some(format!(
                        "{:x}",
                        Sha256::digest(b"synthetic runtime bytes")
                    )),
                    script: Some(path.join("index.js")),
                    script_sha256: Some(format!("{:x}", Sha256::digest(b"opaque package payload"))),
                },
                protocol_profile: ProtocolProfile::Discover2026,
                capabilities: Capabilities::default(),
                timeouts: Timeouts::default(),
            },
        ],
    };
    write(
        &source.join("servers.json"),
        &serde_json::to_vec(&registry).unwrap(),
        0o600,
    );
    write(
        &source.join("secrets.json"),
        br#"{"schema_version":1,"revision":9,"values":{"owned_ref":"synthetic-credential"}}"#,
        0o600,
    );
    write(
        &source.join("catalog.json"),
        br#"{"schema_version":1,"servers":{}}"#,
        0o600,
    );
    let home = admission.guard.verified_binding().unwrap().0.to_owned();
    admission.source = SourcePreflightV2::inspect(&home).unwrap();
    admission.verify_before_retire().unwrap();
}

#[test]
#[ignore = "requires a quiescent disposable OS account; v1.x MCP M1 acceptance"]
fn physical_staging_copies_transforms_fsyncs_and_validates_without_epoch_or_activation() {
    let (_root, mut admission, session, parent) = super::super::tests::fixture(true);
    populated(&mut admission, &parent);
    let owner = admission.retire_source().unwrap().stage_source().unwrap();
    owner.verify_current().unwrap();
    let root = parent.join(format!("mcp-v2.staging-{}", "a".repeat(64)));
    assert!(root.is_dir());
    assert!(!parent.join("mcp-v2").exists());
    assert!(!root.join("ledger/epoch.json").exists());
    let record =
        StoreMigrationRecordV2::decode(&fs::read(root.join("migration.v2.json")).unwrap()).unwrap();
    assert_eq!(record, owner.payload.record);
    assert_eq!(
        record.binding.staging_anchor,
        anchor(owner.payload.tree.root()).unwrap()
    );
    assert_eq!(
        record.planned_active_anchor.inode,
        root.metadata().unwrap().ino()
    );
    assert_eq!(record.runtime_rechecks.len(), 1);
    assert_eq!(record.runtime_rechecks[0].resources.len(), 2);
    assert_eq!(record.unregistered_pins.len(), 1);
    assert_eq!(record.package_files.len(), 6);
    assert_eq!(
        root.join("servers/registered").metadata().unwrap().mode() & 0o777,
        0o500
    );
    assert_eq!(
        root.join("servers/registered/index.js")
            .metadata()
            .unwrap()
            .mode()
            & 0o777,
        0o400
    );
    assert_eq!(
        fs::read(root.join("servers/unregistered/index.js")).unwrap(),
        b"opaque package payload"
    );
    let pin: PinnedPathsV1 =
        serde_json::from_slice(&fs::read(root.join("servers/unregistered/pinned.json")).unwrap())
            .unwrap();
    assert!(pin.package.contains("/.polaris/mcp"));
    assert_eq!(
        pin.entry,
        parent.join("mcp-v2/servers/unregistered/index.js")
    );
    let catalog = CatalogRegenerationRecordV2::decode(
        &fs::read(root.join("catalog-regeneration.v2.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(catalog.servers.len(), 2);
    assert!(catalog.empty_catalog.servers.is_empty());
    assert!(
        !fs::read_to_string(root.join("migration.v2.json"))
            .unwrap()
            .contains("synthetic-credential")
    );
    assert!(session.lock().is_err());
    let lock =
        File::open(parent.join(format!("mcp.retired-{}/owner.lock", "a".repeat(64)))).unwrap();
    assert!(matches!(
        lock.try_lock(),
        Err(std::fs::TryLockError::WouldBlock)
    ));
    let reopened = ExternalIntentFileV2::open(&owner.retired.admission.guard).unwrap();
    assert_eq!(reopened.record(), owner.retired.intent.record());
    assert!(matches!(
        reopened.record(),
        ExternalMigrationIntentV2::Staging { .. }
    ));
    // Even byte-identical stage document writes invalidate the retained pins.
    let bytes = fs::read(root.join("secrets.json")).unwrap();
    fs::write(root.join("secrets.json"), bytes).unwrap();
    assert!(owner.verify_current().is_err());
    drop(owner);
    session.lock().unwrap();
    lock.try_lock().unwrap();
}

#[test]
#[ignore = "requires a quiescent disposable OS account; v1.x MCP M1 acceptance"]
fn physical_staging_faults_preserve_source_partial_stage_and_migrating_gate() {
    for point in [
        "after-staging-root",
        "after-package-copy",
        "before-staging-exchange",
        "after-staging-intent",
    ] {
        let (_root, admission, session, parent) = super::super::tests::fixture(false);
        let retired = admission.retire_source().unwrap();
        let reached = std::rc::Rc::new(std::cell::Cell::new(false));
        let mark = reached.clone();
        STAGING_FAULT.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |current| {
                if current == point {
                    mark.set(true);
                    Err(Error::Storage)
                } else {
                    Ok(())
                }
            }))
        });
        assert!(retired.stage_source().is_err(), "{point}");
        STAGING_FAULT.with(|slot| slot.borrow_mut().take());
        assert!(reached.get(), "{point}");
        assert!(parent.join("mcp").is_file());
        assert!(
            parent
                .join(format!("mcp.retired-{}", "a".repeat(64)))
                .is_dir()
        );
        assert!(
            parent
                .join(format!("mcp-v2.staging-{}", "a".repeat(64)))
                .is_dir()
        );
        assert!(!parent.join("mcp-v2").exists());
        let value: serde_json::Value = serde_json::from_slice(
            &fs::read(parent.join(format!("mcp-migration.v2-{}.json", "a".repeat(64)))).unwrap(),
        )
        .unwrap();
        assert_eq!(
            value["phase"],
            if point == "after-staging-intent" {
                "staging"
            } else {
                "retired"
            }
        );
        session.lock().unwrap();
    }
}

#[test]
fn staging_builder_keeps_read_only_package_modes_and_rejects_unknown_membership_or_replacement() {
    let root = tempfile::tempdir().unwrap();
    let parent = File::open(root.path()).unwrap();
    let mut tree = StageTree::create(&parent, "mcp-v2.staging-test").unwrap();
    tree.directory(b"servers", 0o700).unwrap();
    tree.directory(b"servers/one", 0o500).unwrap();
    tree.bytes(b"servers/one/file", b"public bytes", 0o400)
        .unwrap();
    tree.finalize_directories().unwrap();
    tree.sync_bottom_up().unwrap();
    tree.inspect(&parent).unwrap();
    assert_eq!(
        tree.node(b"servers/one").unwrap().metadata.mode() & 0o777,
        0o500
    );
    let path = root.path().join("mcp-v2.staging-test/servers/one/file");
    fs::set_permissions(path.parent().unwrap(), fs::Permissions::from_mode(0o700)).unwrap();
    fs::remove_file(&path).unwrap();
    write(&path, b"public bytes", 0o400);
    assert!(tree.verify(&parent).is_err());
    let mut tree = StageTree::create(&parent, "mcp-v2.staging-extra").unwrap();
    tree.directory(b"servers", 0o700).unwrap();
    tree.directory(b"servers/one", 0o700).unwrap();
    let folder = root.path().join("mcp-v2.staging-extra/servers/one");
    write(&folder.join("extra"), b"unexpected", 0o600);
    // Test the membership check even when a creator observes the newer parent
    // stamp while finishing its own valid child creation.
    tree.nodes
        .get_mut(b"servers/one".as_slice())
        .unwrap()
        .metadata = tree
        .node(b"servers/one")
        .unwrap()
        .held()
        .unwrap()
        .metadata()
        .unwrap();
    tree.bytes(b"servers/one/owned", b"owned", 0o600).unwrap();
    assert!(tree.inspect(&parent).is_err());
}

#[test]
fn staging_builder_rejects_mount_before_secret_body_and_never_reads_secret_as_public() {
    let root = tempfile::tempdir().unwrap();
    let parent = File::open(root.path()).unwrap();
    let mut tree = StageTree::create(&parent, "mcp-v2.staging-test").unwrap();
    let inode = tree.root().metadata().unwrap().ino();
    super::super::super::intent_file::MOUNT_CHECK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |actual_parent, node| {
            assert_eq!(actual_parent.metadata().unwrap().ino(), inode);
            assert_eq!(node.metadata().unwrap().len(), 0);
            Err(Error::Protection)
        }))
    });
    assert!(
        tree.bytes(b"secrets.json", b"must not be written", 0o600)
            .is_err()
    );
    assert!(
        fs::read(root.path().join("mcp-v2.staging-test/secrets.json"))
            .unwrap()
            .is_empty()
    );
    assert!(tree.read_public(b"secrets.json").is_err());
}

#[test]
fn staging_builder_copies_large_package_under_process_fd_limit() {
    const CHILD: &str = "POLARIS_STAGING_FD_LIMIT_CHILD";
    if std::env::var_os(CHILD).is_none() {
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "mcp::migration::coordinator::retirement::staging::tests::staging_builder_copies_large_package_under_process_fd_limit",
                "--test-threads=1",
                "--nocapture",
            ])
            .env(CHILD, "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("FD_LIMIT_COPY_VERIFIED"));
        return;
    }
    // Change only this disposable child's soft limit, preserving the hard limit.
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    assert_eq!(
        unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
        0
    );
    assert!(limit.rlim_max >= 128);
    limit.rlim_cur = 128;
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
    let root = tempfile::tempdir().unwrap();
    let source = root.path().join("source");
    directory(&source, 0o700);
    directory(&source.join("servers"), 0o700);
    directory(&source.join("servers/large"), 0o700);
    for index in 0..300 {
        write(
            &source.join(format!("servers/large/file-{index:03}")),
            b"public package bytes",
            0o400,
        );
    }
    let source_fd = File::open(&source).unwrap();
    let source_tree =
        CompleteTreeProjectionV2::inspect(&source_fd, ProjectionTreeV2::SourceV1).unwrap();
    let copy = source_tree.verify_before_copy().unwrap();
    let parent = File::open(root.path()).unwrap();
    let mut stage = StageTree::create(&parent, "mcp-v2.staging-fd-limit").unwrap();
    let mut count = 0;
    for entry in copy.entries() {
        if entry.is_directory() {
            stage
                .directory(entry.relative_path(), entry.anchor().mode)
                .unwrap();
        } else {
            stage
                .copy(
                    entry.relative_path(),
                    &entry.open_readonly().unwrap(),
                    entry.anchor().mode,
                )
                .unwrap();
            count += 1;
        }
    }
    assert_eq!(count, 300);
    copy.verify_after_copy().unwrap();
    stage.finalize_directories().unwrap();
    stage.sync_bottom_up().unwrap();
    stage.inspect(&parent).unwrap();
    assert_eq!(
        stage.read_public(b"servers/large/file-299").unwrap(),
        b"public package bytes"
    );
    println!("FD_LIMIT_COPY_VERIFIED");
}

#[test]
#[ignore = "requires a quiescent disposable OS account; v1.x MCP M1 acceptance"]
fn physical_staging_streams_unregistered_pins_with_optional_serial_fd_limit() {
    // Ordinary parallel runs never change a process-global limit. Only an
    // explicitly opted-in disposable Cargo child with one test thread may do so.
    struct RestoreLimit(libc::rlimit);
    impl Drop for RestoreLimit {
        fn drop(&mut self) {
            assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &self.0) }, 0);
        }
    }
    let _restore = if std::env::var_os("POLARIS_STAGING_SERIAL_FD_LIMIT").as_deref()
        == Some(std::ffi::OsStr::new("1"))
    {
        let args = std::env::args().collect::<Vec<_>>();
        assert!(args.iter().any(|arg| arg == "--test-threads=1"));
        assert_eq!(
            args.iter()
                .filter(|arg| arg.starts_with("--test-threads"))
                .count(),
            1
        );
        let mut limit = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) },
            0
        );
        assert!(limit.rlim_cur >= 384 && limit.rlim_max >= 384);
        let restore = RestoreLimit(limit);
        limit.rlim_cur = 384;
        assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_NOFILE, &limit) }, 0);
        let mut actual = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        assert_eq!(
            unsafe { libc::getrlimit(libc::RLIMIT_NOFILE, &mut actual) },
            0
        );
        assert_eq!(actual.rlim_cur, 384);
        println!("PIN_FD_LIMIT_APPLIED=384");
        Some(restore)
    } else {
        None
    };
    let (_root, mut admission, _session, parent) = super::super::tests::fixture(false);
    let source = parent.join("mcp");
    for index in 0..90 {
        let path = source.join(format!("servers/orphan-{index:03}"));
        directory(&path, 0o700);
        write(&path.join("runtime"), b"synthetic runtime", 0o500);
        write(&path.join("index.js"), b"synthetic script", 0o400);
        let pin = PinnedPathsV1 {
            schema_version: 1,
            package: "opaque".into(),
            version: "1.0.0".into(),
            runtime: path.join("runtime"),
            entry: path.join("index.js"),
        };
        write(
            &path.join("pinned.json"),
            &serde_json::to_vec(&pin).unwrap(),
            0o600,
        );
    }
    let home = admission.guard.verified_binding().unwrap().0.to_owned();
    admission.source = SourcePreflightV2::inspect(&home).unwrap();
    let retired = admission.retire_source().unwrap();
    let last = std::rc::Rc::new(std::cell::RefCell::new(None::<String>));
    let observed = last.clone();
    STAGING_FAULT.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |point| {
            *observed.borrow_mut() = Some(point.to_owned());
            Ok(())
        }))
    });
    let result = retired.stage_source();
    STAGING_FAULT.with(|slot| slot.borrow_mut().take());
    let owner = result.unwrap_or_else(|error| {
        panic!("staging failed: {error:?}; last phase: {:?}", last.borrow())
    });
    assert_eq!(owner.payload.record.unregistered_pins.len(), 90);
    assert_eq!(owner.payload.record.package_files.len(), 270);
    assert!(!parent.join("mcp-v2").exists());
    println!("PIN_FD_LIMIT_STAGE_VERIFIED");
}
