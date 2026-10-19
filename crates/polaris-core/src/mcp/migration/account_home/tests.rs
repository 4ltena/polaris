//! Account-home identity, alias and namespace-drift migration tests.
use super::*;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};
fn fixture() -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().join("home");
    fs::create_dir(&home).unwrap();
    fs::set_permissions(&home, fs::Permissions::from_mode(0o700)).unwrap();
    (root, home)
}
#[test]
fn migration_account_home_current_os_lookup_is_read_only_and_retained() {
    let before = trusted_user_home().unwrap();
    let home = TrustedUserHomeV2::inspect().unwrap();
    assert_eq!(home.verified_path().unwrap(), before);
    home.verified_directory().unwrap();
    home.verify_current().unwrap();
}
#[test]
fn migration_account_home_rejects_account_path_uid_mode_and_inode_drift() {
    for change in 0..4 {
        let (root, path) = fixture();
        with_fixture_home(&path, || {
            let mut home = TrustedUserHomeV2::inspect().unwrap();
            match change {
                0 => {
                    with_fixture_home(&root.path().join("other-account"), || {
                        assert!(home.verify_current().is_err())
                    });
                    return;
                }
                1 => home.uid = home.uid.wrapping_add(1),
                2 => fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap(),
                _ => {
                    fs::rename(&path, root.path().join("retired")).unwrap();
                    fs::create_dir(&path).unwrap();
                    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
                }
            }
            assert!(home.verified_path().is_err());
            assert!(home.verified_directory().is_err());
        });
    }
}
#[test]
fn migration_account_home_rejects_alias_and_writable_home_without_creating() {
    for alias in [false, true] {
        let (root, path) = fixture();
        let lookup = if alias {
            let name = root.path().join("alias");
            symlink(&path, &name).unwrap();
            name
        } else {
            fs::set_permissions(&path, fs::Permissions::from_mode(0o777)).unwrap();
            path.clone()
        };
        with_fixture_home(&lookup, || assert!(TrustedUserHomeV2::inspect().is_err()));
        assert_eq!(fs::read_dir(path).unwrap().count(), 0);
    }
}
#[test]
fn migration_account_home_accepts_child_creation_without_losing_identity() {
    let (_root, path) = fixture();
    with_fixture_home(&path, || {
        let home = TrustedUserHomeV2::inspect().unwrap();
        fs::create_dir(path.join(".polaris")).unwrap();
        home.verify_current().unwrap();
    });
}
#[test]
fn migration_account_home_aba_reads_held_parent_and_never_foreign_tree() {
    use std::os::unix::fs::MetadataExt;
    let (root, path) = fixture();
    let foreign = root.path().join("foreign-home");
    let retired = root.path().join("retained-home");
    for home in [&path, &foreign] {
        if !home.exists() {
            fs::create_dir(home).unwrap();
        }
        fs::set_permissions(home, fs::Permissions::from_mode(0o700)).unwrap();
        fs::create_dir(home.join(".polaris")).unwrap();
        fs::set_permissions(home.join(".polaris"), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let expected = path.join(".polaris").metadata().unwrap();
    let sentinel = foreign.join(".polaris/sentinel");
    fs::write(&sentinel, b"foreign tree unchanged").unwrap();
    let sentinel_before = sentinel.metadata().unwrap();
    with_fixture_home(&path, || {
        let home = TrustedUserHomeV2::inspect().unwrap();
        {
            let (named, foreign, retired) = (path.clone(), foreign.clone(), retired.clone());
            AFTER_PARENT_HOME_FD.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    fs::rename(&named, &retired).unwrap();
                    fs::rename(&foreign, &named).unwrap();
                }))
            });
        }
        let (named, foreign, retired) = (path.clone(), foreign.clone(), retired.clone());
        AFTER_PARENT_OPEN.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                fs::rename(&named, &foreign).unwrap();
                fs::rename(&retired, &named).unwrap();
            }))
        });
        let observed = home.verified_polaris_parent().unwrap().metadata().unwrap();
        assert_eq!(
            (observed.dev(), observed.ino()),
            (expected.dev(), expected.ino())
        );
        assert!(AFTER_PARENT_HOME_FD.with(|slot| slot.borrow().is_none()));
        assert!(AFTER_PARENT_OPEN.with(|slot| slot.borrow().is_none()));
        home.verify_current().unwrap();
    });
    assert_eq!(fs::read(&sentinel).unwrap(), b"foreign tree unchanged");
    let after = sentinel.metadata().unwrap();
    assert_eq!(
        (
            after.dev(),
            after.ino(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec()
        ),
        (
            sentinel_before.dev(),
            sentinel_before.ino(),
            sentinel_before.mtime(),
            sentinel_before.mtime_nsec(),
            sentinel_before.ctime(),
            sentinel_before.ctime_nsec()
        )
    );
}
