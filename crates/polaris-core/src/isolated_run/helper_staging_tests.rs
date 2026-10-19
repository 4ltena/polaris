//! Persistent helper staging, publication faults and retained-runtime regression tests.
use super::*;
use polaris_mcp::entry_gate::{
    GATE_FILE, GATE_LOCK, ProductionEntryGateV2, ProductionEntryIdentityV2, ProductionEntryKindV2,
    ProductionEntryManifestV2,
};

struct Fixture {
    _root: tempfile::TempDir,
    home: PathBuf,
    workspace: PathBuf,
    state: PathBuf,
    executable: PathBuf,
    policy: SandboxPolicy,
}
impl Fixture {
    fn new() -> Self {
        let root = tempfile::Builder::new()
            .permissions(Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        let path = root.path().canonicalize().unwrap();
        let home = path.join("home");
        let workspace = path.join("workspace");
        let state = path.join("state");
        for directory in [&home, &workspace, &state] {
            fs::create_dir(directory).unwrap();
            fs::set_permissions(directory, Permissions::from_mode(0o700)).unwrap();
        }
        let executable = workspace.join("polaris");
        fs::write(&executable, b"owned staged executable\n").unwrap();
        fs::set_permissions(&executable, Permissions::from_mode(0o500)).unwrap();
        let policy = SandboxPolicy::new(
            SandboxMode::WorkspaceWrite,
            std::slice::from_ref(&workspace),
        )
        .unwrap();
        Self {
            _root: root,
            home,
            workspace,
            state,
            executable,
            policy,
        }
    }
    fn active(&self) {
        let binary = ProductionBinaryIdentityV2::inspect(&self.executable).unwrap();
        let manifest = ProductionEntryManifestV2 {
            protocol_version: 2,
            entries: vec![ProductionEntryIdentityV2 {
                kind: ProductionEntryKindV2::Cli,
                binary: binary.clone(),
            }],
            runtime_copy_source_hashes: vec![binary.sha256],
        };
        // Disposable test-only Active publication, never a product migration.
        let directory = self.home.join(".polaris");
        fs::create_dir(&directory).unwrap();
        fs::set_permissions(&directory, Permissions::from_mode(0o700)).unwrap();
        let gate = ProductionEntryGateV2::ActiveV2 {
            protocol_version: 2,
            migration_id: "a".repeat(64),
            entry_manifest_hash: manifest.canonical_hash().unwrap(),
            active_intent_hash: "b".repeat(64),
        };
        for (name, bytes) in [
            (GATE_FILE, serde_json::to_vec(&gate).unwrap()),
            (
                polaris_mcp::entry_gate::ENTRY_MANIFEST,
                serde_json::to_vec(&manifest).unwrap(),
            ),
            (GATE_LOCK, Vec::new()),
        ] {
            let path = directory.join(name);
            fs::write(&path, bytes).unwrap();
            fs::set_permissions(&path, Permissions::from_mode(0o600)).unwrap();
            File::open(path).unwrap().sync_all().unwrap();
        }
        File::open(directory).unwrap().sync_all().unwrap();
    }
    fn prepare(&self) -> Result<PreparedConfinedHelper> {
        prepare_confined_locked(
            &self.policy,
            &self.state,
            &self.executable,
            &self.home,
            &ProtectedPathsSnapshot::default(),
        )
    }
    fn entries(&self) -> Vec<PathBuf> {
        let mut entries = fs::read_dir(&self.state)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        entries.sort();
        entries
    }
}

#[test]
fn confined_helper_active_copy_reuses_fixed_identity_and_preserves_completed_on_drop() {
    let fixture = Fixture::new();
    fixture.active();
    let first = fixture.prepare().unwrap();
    let path = first.helper_path().to_owned();
    assert!(path.starts_with(&fixture.state));
    assert_eq!(path.file_name().unwrap(), "polaris");
    assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o500);
    assert_eq!(
        fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o7777,
        0o500
    );
    assert_eq!(
        fs::metadata(path.parent().unwrap().parent().unwrap())
            .unwrap()
            .mode()
            & 0o7777,
        0o700
    );
    assert_eq!(fs::metadata(&path).unwrap().nlink(), 1);
    assert_eq!(
        fs::read(&path).unwrap(),
        fs::read(&fixture.executable).unwrap()
    );
    let entries = fixture.entries();
    assert_eq!(entries.len(), 1);
    let lock =
        polaris_tools::entry_kernel::physical_file(&fixture.home.join(".polaris").join(GATE_LOCK))
            .unwrap();
    assert!(matches!(
        polaris_tools::entry_kernel::lock_exclusive(&lock),
        Err(polaris_tools::entry_kernel::EntryGateErrorV2::Busy)
    ));
    let second = fixture.prepare().unwrap();
    assert_eq!(second.helper_path(), path);
    assert_eq!(fixture.entries(), entries);
    drop(first);
    assert!(polaris_tools::entry_kernel::lock_exclusive(&lock).is_err());
    drop(second);
    assert!(path.exists());
    assert_eq!(
        fs::metadata(path.parent().unwrap()).unwrap().mode() & 0o7777,
        0o500
    );
    polaris_tools::entry_kernel::lock_exclusive(&lock).unwrap();
}

#[test]
fn confined_helper_copy_fault_restart_preserves_complete_or_absent_publication() {
    for point in [
        "file-sync",
        "leaf-mode",
        "runtime-mode",
        "runtime-sync",
        "before-publish",
        "after-publish",
        "state-sync",
    ] {
        let fixture = Fixture::new();
        fixture.active();
        FAULT.with(|fault| fault.set(Some(point)));
        let result = fixture.prepare();
        FAULT.with(|fault| fault.set(None));
        assert!(matches!(result, Err(PrepareError::Io)), "{point}");
        let completed = fixture
            .entries()
            .into_iter()
            .filter(|path| {
                path.file_name()
                    .unwrap()
                    .as_bytes()
                    .starts_with(b"polaris-helper-")
            })
            .collect::<Vec<_>>();
        assert!(completed.len() <= 1);
        for root in &completed {
            let copy = ProductionBinaryIdentityV2::inspect(&root.join("runtime/polaris")).unwrap();
            polaris_tools::entry_kernel::runtime_copy_anchors(&copy).unwrap();
        }
        let recovered = fixture.prepare().unwrap();
        recovered._copy_lease.as_ref().unwrap().verify().unwrap();
        assert_eq!(
            fixture.entries().len(),
            1,
            "{point} left an unpublished normal-error temp"
        );
    }
}

#[test]
fn confined_helper_corrupt_completed_and_unknown_temp_are_never_overwritten_or_deleted() {
    let fixture = Fixture::new();
    let prepared = fixture.prepare().unwrap();
    let path = prepared.helper_path().to_owned();
    drop(prepared);
    fs::set_permissions(&path, Permissions::from_mode(0o700)).unwrap();
    fs::write(&path, b"corrupt completed bytes").unwrap();
    fs::set_permissions(&path, Permissions::from_mode(0o500)).unwrap();
    let unknown = fixture.state.join(".polaris-helper-unknown.tmp");
    fs::create_dir(&unknown).unwrap();
    fs::write(unknown.join("preserve"), b"unknown fixture bytes").unwrap();
    let before = fixture.entries();
    assert!(fixture.prepare().is_err());
    assert_eq!(fixture.entries(), before);
    assert_eq!(fs::read(&path).unwrap(), b"corrupt completed bytes");
    assert_eq!(
        fs::read(unknown.join("preserve")).unwrap(),
        b"unknown fixture bytes"
    );
}

#[test]
fn confined_helper_rejects_writable_alias_unsafe_state_and_unsafe_source_before_copy() {
    let fixture = Fixture::new();
    assert!(
        prepare_confined_locked(
            &fixture.policy,
            &fixture.workspace,
            &fixture.executable,
            &fixture.home,
            &ProtectedPathsSnapshot::default()
        )
        .is_err()
    );
    let alias = fixture.state.with_file_name("state-alias");
    std::os::unix::fs::symlink(&fixture.state, &alias).unwrap();
    assert!(
        prepare_confined_locked(
            &fixture.policy,
            &alias,
            &fixture.executable,
            &fixture.home,
            &ProtectedPathsSnapshot::default()
        )
        .is_err()
    );
    fs::set_permissions(&fixture.state, Permissions::from_mode(0o777)).unwrap();
    assert!(fixture.prepare().is_err());
    fs::set_permissions(&fixture.state, Permissions::from_mode(0o700)).unwrap();
    let link = fixture.executable.with_file_name("source-hardlink");
    fs::hard_link(&fixture.executable, &link).unwrap();
    assert!(fixture.prepare().is_err());
    fs::remove_file(link).unwrap();
    fs::set_permissions(&fixture.executable, Permissions::from_mode(0o777)).unwrap();
    assert!(fixture.prepare().is_err());
    assert!(fixture.entries().is_empty());
}

#[test]
fn confined_helper_outside_writable_source_is_validated_without_copy() {
    let mut fixture = Fixture::new();
    let executable = fixture.state.join("installed-polaris");
    fs::rename(&fixture.executable, &executable).unwrap();
    fixture.executable = executable;
    fixture.active();
    let entries = fixture.entries();
    let owner = fixture.prepare().unwrap();
    assert_eq!(owner.helper_path(), fixture.executable);
    assert_eq!(fixture.entries(), entries);
    owner._source_lease.verify().unwrap();
    assert!(owner._copy_lease.is_none());
}

#[test]
fn confined_helper_parallel_same_hash_publishes_one_complete_copy() {
    let fixture = Fixture::new();
    fixture.active();
    let barrier = std::sync::Barrier::new(2);
    let owners = std::thread::scope(|scope| {
        let first = scope.spawn(|| {
            barrier.wait();
            fixture.prepare()
        });
        let second = scope.spawn(|| {
            barrier.wait();
            fixture.prepare()
        });
        [
            first.join().unwrap().unwrap(),
            second.join().unwrap().unwrap(),
        ]
    });
    assert_eq!(owners[0].helper_path(), owners[1].helper_path());
    assert_eq!(fixture.entries().len(), 1);
    for owner in &owners {
        owner._copy_lease.as_ref().unwrap().verify().unwrap();
    }
}

#[test]
fn confined_helper_new_source_hash_keeps_the_previous_immutable_copy() {
    let fixture = Fixture::new();
    let first = fixture.prepare().unwrap();
    let first_bytes = fs::read(first.helper_path()).unwrap();
    fs::set_permissions(&fixture.executable, Permissions::from_mode(0o700)).unwrap();
    fs::write(&fixture.executable, b"next staged executable!\n").unwrap();
    fs::set_permissions(&fixture.executable, Permissions::from_mode(0o500)).unwrap();
    let second = fixture.prepare().unwrap();
    assert_ne!(first.helper_path(), second.helper_path());
    assert_eq!(fs::read(first.helper_path()).unwrap(), first_bytes);
    assert_eq!(fixture.entries().len(), 2);
}

#[test]
fn confined_helper_unknown_unpublished_tree_is_preserved_and_does_not_wedge_restart() {
    let fixture = Fixture::new();
    let mut temp = TempRun::create(
        &fixture.state,
        &open_physical(&fixture.state, true).unwrap(),
        &RegisteredSecrets::new([]).unwrap(),
    )
    .unwrap();
    temp.runtime = Some(
        RuntimeHelper::create(&temp.path, &temp.root, &RegisteredSecrets::new([]).unwrap())
            .unwrap(),
    );
    let mut input = open_physical(&fixture.executable, false).unwrap();
    let before = input.metadata().unwrap();
    temp.runtime
        .as_mut()
        .unwrap()
        .populate_and_seal(
            &mut input,
            &fixture.executable,
            &before,
            &RegisteredSecrets::new([]).unwrap(),
        )
        .unwrap();
    let path = temp.path.clone();
    fs::write(path.join("unknown"), b"must remain").unwrap();
    drop(temp);
    assert_eq!(fs::read(path.join("unknown")).unwrap(), b"must remain");
    let owner = fixture.prepare().unwrap();
    assert!(owner.helper_path().exists());
    assert_eq!(fs::read(path.join("unknown")).unwrap(), b"must remain");
}

#[test]
fn confined_helper_active_child_keeps_shared_admission_after_parent_owner_drop() {
    let fixture = Fixture::new();
    install_child_executable(&fixture);
    fixture.active();
    let owner = fixture.prepare().unwrap();
    let helper = owner.helper_path().to_owned();
    let mut child = std::process::Command::new(&helper)
        .args([
            "--exact",
            "isolated_run::helper_staging::tests::active_entry_child",
            "--ignored",
            "--test-threads=1",
        ])
        .env_clear()
        .env("HOME", "/invalid-poison-home")
        .env("POLARIS_ACTIVE_COPY_FIXTURE_HOME", &fixture.home)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let ready = fixture.home.join("child-ready");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while !ready.exists()
        && child.try_wait().unwrap().is_none()
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let admitted = ready.exists();
    if !admitted {
        let _ = child.kill();
    }
    let lock =
        polaris_tools::entry_kernel::physical_file(&fixture.home.join(".polaris").join(GATE_LOCK))
            .unwrap();
    drop(owner);
    let blocked = polaris_tools::entry_kernel::lock_exclusive(&lock).is_err();
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(b"x");
    }
    while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if child.try_wait().unwrap().is_none() {
        let _ = child.kill();
    }
    let output = child.wait_with_output().unwrap();
    assert!(admitted, "{}", String::from_utf8_lossy(&output.stderr));
    assert!(
        blocked,
        "child's independently acquired SH must block EX after parent drop"
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    polaris_tools::entry_kernel::lock_exclusive(&lock).unwrap();
    assert!(helper.exists());
    assert_eq!(
        fs::metadata(helper.parent().unwrap()).unwrap().mode() & 0o7777,
        0o500
    );
}

#[test]
#[ignore = "owned Active child fixture, invoked and verified by its parent test"]
fn active_entry_child() {
    let home = PathBuf::from(std::env::var_os("POLARIS_ACTIVE_COPY_FIXTURE_HOME").unwrap());
    let executable = std::env::current_exe().unwrap().canonicalize().unwrap();
    let lease = ProductionEntryLeaseV2::enter(&home, &executable).unwrap();
    fs::write(home.join("child-ready"), b"admitted").unwrap();
    let mut byte = [0u8; 1];
    std::io::stdin().read_exact(&mut byte).unwrap();
    lease.verify().unwrap();
}

fn install_child_executable(fixture: &Fixture) {
    fs::set_permissions(&fixture.executable, Permissions::from_mode(0o700)).unwrap();
    fs::copy(std::env::current_exe().unwrap(), &fixture.executable).unwrap();
    // The linked Core test harness contains large local symbol tables. Strip
    // only the disposable copy, keeping the production 128 MiB limit intact.
    let status = std::process::Command::new("/usr/bin/strip")
        .arg("-x")
        .arg(&fixture.executable)
        .status()
        .unwrap();
    assert!(status.success());
    assert!(fs::metadata(&fixture.executable).unwrap().len() <= MAX_HELPER_BYTES);
    fs::set_permissions(&fixture.executable, Permissions::from_mode(0o500)).unwrap();
}

#[test]
fn confined_helper_physical_overlap_checks_both_ancestor_directions() {
    let fixture = Fixture::new();
    assert!(writable_overlap(&fixture.policy, &fixture.workspace).unwrap());
    assert!(writable_overlap(&fixture.policy, fixture.workspace.parent().unwrap()).unwrap());
    let nested = fixture.workspace.join("nested");
    fs::create_dir(&nested).unwrap();
    assert!(writable_overlap(&fixture.policy, &nested).unwrap());
    assert!(!writable_overlap(&fixture.policy, &fixture.state).unwrap());
    let witness = open_physical(&fixture.workspace, true)
        .unwrap()
        .metadata()
        .unwrap();
    assert!(ancestor_has_identity(&nested, &witness).unwrap());
    assert!(!ancestor_has_identity(&fixture.state, &witness).unwrap());
}

#[test]
fn confined_helper_protected_destinations_refuse_before_writes_or_hashing() {
    let fixture = Fixture::new();
    let state = open_physical(&fixture.state, true).unwrap();
    for protected in [&fixture.state, &fixture.state.join("reserved-secret")] {
        let secrets = RegisteredSecrets::new([protected.clone()]).unwrap();
        assert!(matches!(
            TempRun::create(&fixture.state, &state, &secrets),
            Err(PrepareError::UnsafeHelper)
        ));
        assert!(fixture.entries().is_empty());
    }
    fixture.active();
    let owner = fixture.prepare().unwrap();
    let helper = owner.helper_path();
    let runtime_path = helper.parent().unwrap();
    let run = runtime_path.parent().unwrap();
    let root = open_physical(run, true).unwrap();
    let runtime =
        RuntimeHelper::existing(run, &root, &RegisteredSecrets::new([]).unwrap()).unwrap();
    let digest = ProductionBinaryIdentityV2::inspect(&fixture.executable)
        .unwrap()
        .sha256;
    runtime
        .verify(&digest, &RegisteredSecrets::new([]).unwrap())
        .unwrap();
    let bytes = fs::read(helper).unwrap();
    for protected in [run, runtime_path, helper] {
        let secrets = RegisteredSecrets::new([protected.to_owned()]).unwrap();
        assert!(matches!(
            runtime.verify(&digest, &secrets),
            Err(PrepareError::UnsafeHelper)
        ));
        assert!(matches!(
            RuntimeHelper::existing(run, &root, &secrets),
            Err(PrepareError::UnsafeHelper)
        ));
        assert_eq!(fs::read(helper).unwrap(), bytes);
        assert_eq!(fs::metadata(runtime_path).unwrap().mode() & 0o7777, 0o500);
    }
    let registered = fixture.home.join("registered-secret");
    fs::write(&registered, b"secret inode must never be hashed").unwrap();
    fs::set_permissions(&registered, Permissions::from_mode(0o500)).unwrap();
    let secrets = RegisteredSecrets::new([registered.clone()]).unwrap();
    fs::set_permissions(runtime_path, Permissions::from_mode(0o700)).unwrap();
    fs::rename(&registered, helper).unwrap();
    fs::set_permissions(runtime_path, Permissions::from_mode(0o500)).unwrap();
    assert!(matches!(
        RuntimeHelper::existing(run, &root, &secrets),
        Err(PrepareError::UnsafeHelper)
    ));
    assert_eq!(
        fs::read(helper).unwrap(),
        b"secret inode must never be hashed"
    );
}

#[test]
fn confined_helper_crashed_unpublished_copy_is_preserved_and_restart_completes() {
    let fixture = Fixture::new();
    install_child_executable(&fixture);
    fixture.active();
    let mut child = std::process::Command::new(&fixture.executable)
        .args([
            "--exact",
            "isolated_run::helper_staging::tests::staging_crash_child",
            "--ignored",
            "--test-threads=1",
        ])
        .env_clear()
        .env("HOME", "/invalid-poison-home")
        .env("POLARIS_ACTIVE_COPY_FIXTURE_HOME", &fixture.home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while child.try_wait().unwrap().is_none() && std::time::Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    if child.try_wait().unwrap().is_none() {
        let _ = child.kill();
    }
    let output = child.wait_with_output().unwrap();
    assert_eq!(
        output.status.code(),
        Some(91),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let abandoned = fixture.entries();
    assert_eq!(abandoned.len(), 1);
    assert!(
        abandoned[0]
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with(".tmp")
    );
    let leaf = abandoned[0].join("runtime/polaris");
    let before = fs::metadata(&leaf).unwrap();
    assert_eq!(before.mode() & 0o7777, 0o500);
    assert_eq!(
        fs::metadata(leaf.parent().unwrap()).unwrap().mode() & 0o7777,
        0o500
    );
    let owner = fixture.prepare().unwrap();
    assert_ne!(owner.helper_path(), leaf);
    assert!(stable(&before, &fs::metadata(&leaf).unwrap()));
    assert_eq!(fixture.entries().len(), 2);
    owner._copy_lease.as_ref().unwrap().verify().unwrap();
}

#[test]
#[ignore = "owned crash fixture, invoked and reaped by its parent test"]
fn staging_crash_child() {
    let home = PathBuf::from(std::env::var_os("POLARIS_ACTIVE_COPY_FIXTURE_HOME").unwrap());
    let root = home.parent().unwrap();
    let workspace = root.join("workspace");
    let policy = SandboxPolicy::new(
        SandboxMode::WorkspaceWrite,
        std::slice::from_ref(&workspace),
    )
    .unwrap();
    FAULT.with(|fault| fault.set(Some("runtime-mode")));
    CRASH.with(|crash| crash.set(true));
    let _ = prepare_confined_locked(
        &policy,
        &root.join("state"),
        &workspace.join("polaris"),
        &home,
        &ProtectedPathsSnapshot::default(),
    );
    panic!("crash point was not reached");
}

#[test]
fn confined_helper_extra_child_is_rejected_before_completed_publication() {
    let fixture = Fixture::new();
    fixture.active();
    EXTRA_CHILD.with(|extra| extra.set(true));
    let result = fixture.prepare();
    EXTRA_CHILD.with(|extra| extra.set(false));
    assert!(matches!(result, Err(PrepareError::Changed)));
    let entries = fixture.entries();
    assert_eq!(entries.len(), 1);
    assert!(
        entries[0]
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .ends_with(".tmp")
    );
    assert_eq!(
        fs::read(entries[0].join("unknown")).unwrap(),
        b"must remain unpublished"
    );
    let owner = fixture.prepare().unwrap();
    assert!(!owner.helper_path().starts_with(&entries[0]));
    assert_eq!(
        fs::read(entries[0].join("unknown")).unwrap(),
        b"must remain unpublished"
    );
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // Restore write access only to the known runtime child of this test's
        // private state entries, so TempDir can remove its own fixtures.
        if let Ok(entries) = fs::read_dir(&self.state) {
            for entry in entries.flatten() {
                let root = entry.path();
                let name = entry.file_name();
                if !name.to_string_lossy().contains("polaris-helper-") {
                    continue;
                }
                let Ok(parent) = open_physical(&root, true) else {
                    continue;
                };
                let Ok(runtime) = open_at(&parent, c"runtime", true, false) else {
                    continue;
                };
                let _ = runtime.set_permissions(Permissions::from_mode(0o700));
            }
        }
    }
}
