//! Trusted home protection without environment variables, including credential rotation.
use polaris_auth::protection::{use_trusted_home, with_protected_paths};

#[test]
fn trusted_home_without_environment_preserves_both_stores_and_rotation() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().canonicalize().unwrap();
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--ignored", "--exact", "trusted_home_child"])
        .env_clear()
        .arg(&home)
        .status()
        .unwrap();
    assert!(status.success());
}

#[test]
#[ignore = "isolated process with no HOME"]
fn trusted_home_child() {
    let home = std::path::PathBuf::from(std::env::args_os().last().unwrap());
    assert!(std::env::var_os("HOME").is_none());
    assert!(with_protected_paths(|_| ()).is_err());
    use_trusted_home(&home).unwrap();
    use_trusted_home(&home).unwrap();
    assert!(use_trusted_home(&home.join("other")).is_err());
    let auth = home.join(".polaris/auth.json");
    let api = home.join(".polaris/api_key.json");
    with_protected_paths(|paths| {
        for path in [&auth, &api] {
            assert!(paths.paths().contains(path));
            assert!(paths.paths().contains(&path.with_extension("json.tmp")));
        }
    })
    .unwrap();
    std::fs::create_dir_all(auth.parent().unwrap()).unwrap();
    std::fs::write(&auth, b"synthetic").unwrap();
    let old = with_protected_paths(|p| p.identities().clone()).unwrap();
    assert!(!old.is_empty());
    std::fs::rename(&auth, home.join("previous")).unwrap();
    std::fs::write(&auth, b"rotated synthetic").unwrap();
    with_protected_paths(|p| {
        assert!(old.is_subset(p.identities()));
        assert!(p.identities().len() > old.len());
    })
    .unwrap();
}
