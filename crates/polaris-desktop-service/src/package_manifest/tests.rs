//! Packaged helper manifest identity, secret exclusion and race tests.
use super::*;
use std::{
    cell::Cell,
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};
struct Fixture {
    _temp: tempfile::TempDir,
    executable: PathBuf,
    manifest: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temp = tempfile::tempdir().unwrap();
        let contents = temp
            .path()
            .canonicalize()
            .unwrap()
            .join("Example.app/Contents");
        fs::create_dir_all(contents.join("Helpers")).unwrap();
        fs::create_dir(contents.join("Resources")).unwrap();
        let executable = contents.join("Helpers/polaris-desktop-service");
        fs::write(&executable, "synthetic executable, never run").unwrap();
        fs::set_permissions(&executable, fs::Permissions::from_mode(0o755)).unwrap();
        let manifest = contents.join("Resources/execution-helper.json");
        fs::write(
            &manifest,
            format!(
                "{{\"schema_version\":1,\"sha256\":\"{}\"}}\n",
                "ab".repeat(32)
            ),
        )
        .unwrap();
        Self {
            _temp: temp,
            executable,
            manifest,
        }
    }
    fn refused_before_body(&self) {
        let called = Cell::new(false);
        assert!(read_manifest(&self.executable, || called.set(true)).is_err());
        assert!(!called.get());
    }
}
#[test]
fn fixed_manifest_returns_only_fixed_helper_and_digest() {
    let f = Fixture::new();
    let found = read_package_manifest(&f.executable).unwrap();
    assert_eq!(found.sha256, [0xab; 32]);
    assert_eq!(
        found.helper_path,
        f.executable
            .parent()
            .unwrap()
            .join("polaris-execution-helper")
    );
    assert!(!found.helper_path.exists()); // Recipe owns actual helper verification.
}
#[test]
fn unknown_duplicate_versions_hashes_and_trailing_documents_are_refused() {
    let f = Fixture::new();
    for body in [
        format!(
            "{{\"schema_version\":2,\"sha256\":\"{}\"}}",
            "ab".repeat(32)
        ),
        format!(
            "{{\"schema_version\":1,\"sha256\":\"{}\",\"path\":\"/tmp/helper\"}}",
            "ab".repeat(32)
        ),
        format!(
            "{{\"schema_version\":1,\"schema_version\":1,\"sha256\":\"{}\"}}",
            "ab".repeat(32)
        ),
        format!(
            "{{\"schema_version\":1,\"sha256\":\"{}\"}}",
            "AB".repeat(32)
        ),
        "{\"schema_version\":1,\"sha256\":null}".into(),
        "{\"schema_version\":1,\"sha256\":\"ab\"} {}".into(),
    ] {
        fs::write(&f.manifest, body).unwrap();
        assert!(read_package_manifest(&f.executable).is_err());
    }
}
#[test]
fn leaf_and_ancestor_symlinks_are_refused_before_body() {
    for ancestor in [false, true] {
        let f = Fixture::new();
        let path = if ancestor {
            f.manifest.parent().unwrap().to_path_buf()
        } else {
            f.manifest.clone()
        };
        let moved = path.with_extension("original");
        fs::rename(&path, &moved).unwrap();
        symlink(&moved, &path).unwrap();
        f.refused_before_body();
    }
}
#[test]
fn mode_hardlink_special_and_bound_are_refused_before_body() {
    for kind in 0..4 {
        let f = Fixture::new();
        match kind {
            0 => fs::set_permissions(&f.manifest, fs::Permissions::from_mode(0o666)).unwrap(),
            1 => fs::hard_link(&f.manifest, f.manifest.with_extension("alias")).unwrap(),
            2 => {
                fs::remove_file(&f.manifest).unwrap();
                let p = CString::new(f.manifest.as_os_str().as_bytes()).unwrap();
                assert_eq!(unsafe { libc::mkfifo(p.as_ptr(), 0o600) }, 0);
            }
            _ => fs::write(&f.manifest, vec![b' '; 4097]).unwrap(),
        }
        f.refused_before_body();
    }
}
#[test]
fn registered_path_and_rotated_secret_inode_are_refused_before_body() {
    // Keep registered old inodes out of unrelated tests in the same process.
    const MARKER: &str = "POLARIS_MANIFEST_SECRET_FIXTURE";
    if std::env::var_os(MARKER).is_none() {
        let home = tempfile::tempdir().unwrap();
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "package_manifest::tests::registered_path_and_rotated_secret_inode_are_refused_before_body", "--test-threads=1"])
            .env_clear().env("HOME", home.path()).env(MARKER, "1").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    }
    for moved in [false, true] {
        let f = Fixture::new();
        let secret = if moved {
            f.manifest.with_extension("dummy-secret")
        } else {
            f.manifest.clone()
        };
        polaris_auth::api_key::save_to(&secret, "synthetic-fixture-key").unwrap();
        if moved {
            fs::rename(&secret, &f.manifest).unwrap();
        }
        f.refused_before_body();
    }
}
#[test]
fn mutation_and_name_replacement_during_read_are_refused() {
    for replace in [false, true] {
        let f = Fixture::new();
        let called = Cell::new(false);
        let result = read_manifest(&f.executable, || {
            called.set(true);
            if replace {
                let new = f.manifest.with_extension("new");
                fs::copy(&f.manifest, &new).unwrap();
                fs::rename(new, &f.manifest).unwrap();
            } else {
                fs::write(&f.manifest, b"{}").unwrap();
            }
        });
        assert!(called.get());
        assert!(result.is_err());
    }
}
#[test]
fn wrong_layout_and_package_directory_mode_are_refused_before_body() {
    let f = Fixture::new();
    assert!(read_package_manifest(&f.executable.with_file_name("other")).is_err());
    fs::set_permissions(
        f.manifest.parent().unwrap(),
        fs::Permissions::from_mode(0o777),
    )
    .unwrap();
    f.refused_before_body();
}
#[test]
fn changed_package_ancestor_is_refused_after_pinned_read() {
    let f = Fixture::new();
    let resources = f.manifest.parent().unwrap();
    assert!(
        read_manifest(&f.executable, || {
            let moved = resources.with_extension("old");
            fs::rename(resources, &moved).unwrap();
            fs::create_dir(resources).unwrap();
            fs::copy(moved.join("execution-helper.json"), &f.manifest).unwrap();
        })
        .is_err()
    );
}
