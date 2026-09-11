//! Synthetic end-to-end snapshot, confined execution, and source conflict tests.
#![cfg(target_os = "macos")]

use polaris_core::isolated_workspace::{
    Limits, RegisteredSecrets, check_source_conflicts, collect_changes, snapshot,
};
use polaris_sandbox::{SandboxMode, SandboxPolicy, run_confined};
use std::path::{Path, PathBuf};

#[test]
fn auth_registration_protects_custom_names_and_rotated_identities() {
    let home = tempfile::tempdir().unwrap();
    let result = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--ignored",
            "--exact",
            "auth_registration_fixture",
            "--nocapture",
        ])
        .env_clear()
        .env("HOME", home.path().canonicalize().unwrap())
        .env("POLARIS_AUTH_SNAPSHOT_FIXTURE", "1")
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert!(String::from_utf8_lossy(&result.stdout).contains("auth-copy-verified"));
}

#[test]
#[ignore = "subprocess fixture with isolated HOME and dummy credentials"]
fn auth_registration_fixture() {
    if std::env::var("POLARIS_AUTH_SNAPSHOT_FIXTURE").as_deref() != Ok("1") {
        return;
    }
    let source = PathBuf::from(std::env::var_os("HOME").unwrap()).join("source");
    std::fs::create_dir(&source).unwrap();
    let secret = source.join("custom-store");
    polaris_auth::api_key::save_to(&secret, "DUMMY_OLD_TOKEN").unwrap();
    std::fs::rename(&secret, source.join("innocent-name.txt")).unwrap();
    polaris_auth::api_key::save_to(&secret, "DUMMY_NEW_TOKEN").unwrap();
    std::fs::write(source.join("ordinary.rs"), "fn main() {}\n").unwrap();
    let limits = Limits {
        max_entries: 64,
        max_files: 32,
        max_file_bytes: 4096,
        max_total_bytes: 32768,
        max_depth: 8,
    };
    let copy = polaris_core::isolated_workspace::protected_snapshot(&source, limits).unwrap();
    assert!(!copy.path().join("custom-store").exists());
    assert!(!copy.path().join("innocent-name.txt").exists());
    assert_eq!(
        std::fs::read_to_string(copy.path().join("ordinary.rs")).unwrap(),
        "fn main() {}\n"
    );
    assert_eq!(
        polaris_auth::api_key::load_from(&secret)
            .unwrap()
            .as_deref(),
        Some("DUMMY_NEW_TOKEN")
    );
    println!("auth-copy-verified");
}

#[test]
fn sanitized_copy_execution_keeps_scratch_private_and_detects_external_edits() {
    let original = tempfile::tempdir().unwrap();
    let source = original.path().canonicalize().unwrap();
    std::fs::write(source.join("source.txt"), "before\n").unwrap();
    std::fs::write(source.join(".env"), "DUMMY_SECRET_537").unwrap();
    std::fs::create_dir_all(source.join(".git/objects")).unwrap();
    std::fs::write(source.join(".git/objects/history"), "DUMMY_SECRET_537").unwrap();
    let secrets = RegisteredSecrets::new([]).unwrap();
    let limits = Limits {
        max_entries: 64,
        max_files: 32,
        max_file_bytes: 4096,
        max_total_bytes: 32768,
        max_depth: 8,
    };
    let copy = snapshot(&source, &secrets, limits).unwrap();
    assert!(!copy.path().join(".env").exists());
    assert!(!copy.path().join(".git").exists());
    let runtimes: Vec<PathBuf> = ["/System", "/usr/lib", "/bin", "/usr/bin"]
        .into_iter()
        .map(Into::into)
        .collect();
    let policy = SandboxPolicy::isolated(SandboxMode::WorkspaceWrite, copy.path(), &runtimes)
        .unwrap()
        .with_isolated_sibling_environment(copy.scratch_home(), copy.scratch_tmp())
        .unwrap();
    let result = run_confined(&policy, Path::new("/bin/sh"), &[
        "-c".into(),
        "cat source.txt; printf 'after\\n' > source.txt; printf cache > \"$HOME/cache\"; printf temporary > \"$TMPDIR/temp\"; if cat \"$1/.env\"; then exit 77; fi; test ! -e .git".into(),
        "fixture".into(), source.to_string_lossy().into_owned(),
    ], None).unwrap();
    assert_eq!(result.status, 0, "{result:?}");
    assert!(result.stdout.contains("before"));
    assert!(!result.stdout.contains("DUMMY_SECRET_537"));
    assert!(!result.stderr.contains("DUMMY_SECRET_537"));
    assert_eq!(
        std::fs::read_to_string(source.join("source.txt")).unwrap(),
        "before\n"
    );
    let changes = collect_changes(&copy, &secrets, limits).unwrap();
    assert!(changes.is_applicable());
    assert_eq!(changes.changes.len(), 1);
    assert_eq!(changes.changes[0].relative_path, Path::new("source.txt"));
    assert!(
        check_source_conflicts(&copy, &changes, &secrets, limits)
            .unwrap()
            .is_empty()
    );
    std::fs::write(source.join("source.txt"), "external edit\n").unwrap();
    assert!(
        !check_source_conflicts(&copy, &changes, &secrets, limits)
            .unwrap()
            .is_empty()
    );
}
