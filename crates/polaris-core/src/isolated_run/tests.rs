//! Prepared workspace, staged helper identity, and runtime allowlist tests.
use super::*;
use std::os::unix::fs::symlink;

fn limits() -> Limits {
    Limits {
        max_entries: 16,
        max_files: 8,
        max_file_bytes: 1024,
        max_total_bytes: 4096,
        max_depth: 4,
    }
}
fn fixture() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let temp = tempfile::Builder::new()
        .permissions(Permissions::from_mode(0o700))
        .tempdir()
        .unwrap();
    let root = fs::canonicalize(temp.path()).unwrap();
    let source = root.join("source");
    fs::create_dir(&source).unwrap();
    fs::write(source.join("ordinary"), b"source bytes").unwrap();
    fs::write(source.join(".env"), b"dummy secret excluded").unwrap();
    let executable = root.join("trusted-polaris");
    fs::write(&executable, b"dummy executable bytes; never launched").unwrap();
    fs::set_permissions(&executable, Permissions::from_mode(0o700)).unwrap();
    (temp, source, executable)
}
fn prepare(
    source: &Path,
    executable: &Path,
    mode: SandboxMode,
    roots: &[PathBuf],
) -> Result<PreparedWorkspace> {
    // Empty metadata snapshot only. Do not access the process's real auth defaults.
    prepare_locked(
        source,
        executable,
        mode,
        roots,
        limits(),
        &ProtectedPathsSnapshot::default(),
    )
}
#[test]
fn all_modes_stage_readonly_runtime_and_sibling_scratch_without_source_exposure() {
    for mode in [
        SandboxMode::ReadOnly,
        SandboxMode::WorkspaceWrite,
        SandboxMode::FullAccess,
    ] {
        let (_temp, source, executable) = fixture();
        let prepared = prepare(&source, &executable, mode, &[PathBuf::from("/usr/lib")]).unwrap();
        let copy = prepared.snapshot().path();
        let root = copy.parent().unwrap().to_owned();
        assert_eq!(fs::read(copy.join("ordinary")).unwrap(), b"source bytes");
        assert!(!copy.join(".env").exists());
        assert_eq!(
            fs::read(prepared.helper_path()).unwrap(),
            fs::read(&executable).unwrap()
        );
        assert_eq!(
            prepared.helper_path().parent().unwrap().parent(),
            Some(root.as_path())
        );
        assert_eq!(
            fs::metadata(prepared.helper_path()).unwrap().mode() & 0o7777,
            0o500
        );
        assert_eq!(
            fs::metadata(prepared.helper_path().parent().unwrap())
                .unwrap()
                .mode()
                & 0o7777,
            0o500
        );
        let policy = prepared.policy();
        assert_eq!(policy.mode(), mode);
        let boundary = policy.isolated_boundary().unwrap();
        assert!(!boundary.readable_roots.iter().any(|p| overlaps(p, &source)));
        assert!(
            boundary
                .readable_roots
                .contains(&prepared.helper_path().parent().unwrap().to_owned())
        );
        assert!(
            !policy
                .writable_roots()
                .iter()
                .any(|p| prepared.helper_path().starts_with(p))
        );
        if mode == SandboxMode::ReadOnly {
            assert!(policy.writable_roots().is_empty());
        } else {
            assert_eq!(policy.writable_roots(), &[copy.to_owned()]);
        }
        let env = boundary.environment.as_ref().unwrap();
        assert_eq!(env.home, prepared.snapshot().scratch_home());
        assert_eq!(env.tmpdir, prepared.snapshot().scratch_tmp());
        assert_eq!(env.home.parent(), Some(root.as_path()));
        drop(prepared);
        assert!(!root.exists());
        assert!(source.exists());
        assert!(executable.exists());
    }
}
#[test]
fn rejects_user_runtime_root_source_and_filesystem_root() {
    let (_temp, source, executable) = fixture();
    for root in [
        PathBuf::from("/"),
        source.clone(),
        source.parent().unwrap().into(),
        PathBuf::from("/usr"),
        PathBuf::from("/usr/local"),
        PathBuf::from("/Applications"),
    ] {
        assert!(matches!(
            prepare(&source, &executable, SandboxMode::FullAccess, &[root]),
            Err(PrepareError::RuntimeDenied)
        ));
    }
    assert!(
        prepare(
            &source,
            &executable,
            SandboxMode::ReadOnly,
            &vec![PathBuf::from("/bin"); 5]
        )
        .is_err()
    );
}
#[test]
fn refuses_source_helper_symlink_hardlink_special_and_unsafe_mode() {
    let (_temp, source, executable) = fixture();
    let embedded = source.join("polaris");
    fs::copy(&executable, &embedded).unwrap();
    assert!(matches!(
        prepare(&source, &embedded, SandboxMode::ReadOnly, &[]),
        Err(PrepareError::UnsafeHelper)
    ));
    let alias = executable.with_extension("alias");
    symlink(&executable, &alias).unwrap();
    assert!(prepare(&source, &alias, SandboxMode::ReadOnly, &[]).is_err());
    fs::remove_file(&alias).unwrap();
    fs::hard_link(&executable, &alias).unwrap();
    assert!(matches!(
        prepare(&source, &executable, SandboxMode::ReadOnly, &[]),
        Err(PrepareError::UnsafeHelper)
    ));
    fs::remove_file(&alias).unwrap();
    let fifo = executable.with_extension("fifo");
    let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o700) }, 0);
    assert!(matches!(
        prepare(&source, &fifo, SandboxMode::ReadOnly, &[]),
        Err(PrepareError::UnsafeHelper)
    ));
    for mode in [0o600, 0o777, 0o4700] {
        fs::set_permissions(&executable, Permissions::from_mode(mode)).unwrap();
        let observed = fs::metadata(&executable).unwrap();
        let result = prepare(&source, &executable, SandboxMode::ReadOnly, &[]);
        assert!(
            matches!(&result, Err(PrepareError::UnsafeHelper)),
            "requested={mode:#o}, observed={:#o}, uid={}, nlink={}, metadata_check={:?}, prepare_error={:?}, prepare_succeeded={}",
            observed.mode() & 0o7777,
            observed.uid(),
            observed.nlink(),
            helper_metadata(&observed),
            result.as_ref().err(),
            result.is_ok(),
        );
    }
}
#[test]
fn bounds_helper_and_source_and_detects_helper_metadata_change() {
    let (_temp, source, executable) = fixture();
    let before = fs::metadata(&executable).unwrap();
    fs::write(&executable, b"changed").unwrap();
    assert!(!stable(&before, &fs::metadata(&executable).unwrap()));
    File::options()
        .write(true)
        .open(&executable)
        .unwrap()
        .set_len(MAX_HELPER_BYTES + 1)
        .unwrap();
    assert!(matches!(
        prepare(&source, &executable, SandboxMode::ReadOnly, &[]),
        Err(PrepareError::Limit)
    ));
    fs::write(&executable, b"dummy").unwrap();
    fs::write(source.join("large"), vec![0; 1025]).unwrap();
    assert!(matches!(
        prepare(&source, &executable, SandboxMode::ReadOnly, &[]),
        Err(PrepareError::Snapshot)
    ));
}
#[test]
fn public_factory_uses_auth_lock_in_disposable_home_subprocess() {
    let (temp, source, executable) = fixture();
    let root = fs::canonicalize(temp.path()).unwrap();
    let home = root.join("auth-home");
    fs::create_dir(&home).unwrap();
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "isolated_run::tests::auth_lock_child",
            "--exact",
            "--ignored",
            "--nocapture",
        ])
        .env_clear()
        .env("HOME", &home)
        .env("ISOLATED_PREPARE_SOURCE", &source)
        .env("ISOLATED_PREPARE_HELPER", &executable)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while child.try_wait().unwrap().is_none() {
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("dummy auth child timed out");
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let output = child.wait_with_output().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));
}
#[test]
#[ignore = "dummy auth defaults only; launched by the parent test with an empty environment"]
fn auth_lock_child() {
    let source = PathBuf::from(std::env::var_os("ISOLATED_PREPARE_SOURCE").unwrap());
    let helper = PathBuf::from(std::env::var_os("ISOLATED_PREPARE_HELPER").unwrap());
    let registered = source.join("ordinary-credential.json");
    polaris_auth::api_key::save_to(&registered, "dummy-only").unwrap();
    let prepared =
        PreparedWorkspace::prepare(&source, &helper, SandboxMode::ReadOnly, &[], limits()).unwrap();
    assert!(
        !prepared
            .snapshot()
            .path()
            .join("ordinary-credential.json")
            .exists()
    );
    assert!(prepared.snapshot().path().join("ordinary").exists());
    drop(prepared);
    // A registered credential identity cannot be admitted as the trusted executable.
    fs::rename(&registered, &helper).unwrap();
    fs::set_permissions(&helper, Permissions::from_mode(0o700)).unwrap();
    assert!(matches!(
        PreparedWorkspace::prepare(&source, &helper, SandboxMode::ReadOnly, &[], limits()),
        Err(PrepareError::UnsafeHelper)
    ));
}

#[test]
fn registered_runtime_descendant_is_rejected_without_reading_its_body() {
    let (_temp, source, _) = fixture();
    let reserved = [PathBuf::from("/usr/lib/dummy-reserved-credential")]
        .into_iter()
        .collect();
    let secrets = RegisteredSecrets::new([]).unwrap();
    assert!(matches!(
        validate_runtime_roots(&[PathBuf::from("/usr/lib")], &source, &reserved, &secrets),
        Err(PrepareError::RuntimeDenied)
    ));
}
