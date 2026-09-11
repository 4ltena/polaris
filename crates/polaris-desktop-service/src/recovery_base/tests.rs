//! Recovery directory placement, exclusive creation and pinned failure evidence tests.
use super::*;
use polaris_core::desktop_store::SourceApplyIdentity;
use std::{
    fs,
    os::unix::fs::{PermissionsExt, symlink},
};
struct Fixture {
    _temp: tempfile::TempDir,
    source: CurrentSourcePolicy,
    metadata: PathBuf,
    root: DesktopRoot,
    project: ProjectId,
    session: SessionId,
}
impl Fixture {
    fn new(inside: bool) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let source = base.join("source");
        fs::create_dir(&source).unwrap();
        let m = fs::metadata(&source).unwrap();
        let metadata = if inside {
            source.join("metadata")
        } else {
            base.join("metadata")
        };
        fs::create_dir(&metadata).unwrap();
        fs::set_permissions(&metadata, fs::Permissions::from_mode(0o700)).unwrap();
        let root = DesktopRoot::open_owned(&metadata).unwrap();
        Self {
            _temp: temp,
            source: CurrentSourcePolicy {
                source_path: source,
                source_identity: SourceApplyIdentity {
                    device: DecimalU64::new(m.dev()),
                    inode: DecimalU64::new(m.ino()),
                },
                policy_revision: DecimalU64::new(0),
                read_allowed: true,
                write_allowed: false,
            },
            metadata,
            root,
            project: ProjectId::new("project").unwrap(),
            session: SessionId::new("session").unwrap(),
        }
    }
    fn build(&self) -> Result<PinnedRecoveryBase, RecoveryBaseError> {
        prepare_recovery_base(
            &self.source,
            &self.root,
            &self.metadata,
            &self.project,
            &self.session,
        )
    }
    fn count(path: &Path) -> usize {
        fs::read_dir(path)
            .unwrap()
            .filter(|e| {
                e.as_ref()
                    .unwrap()
                    .file_name()
                    .as_bytes()
                    .starts_with(b".polaris-recovery-")
            })
            .count()
    }
}
#[test]
fn prefers_metadata_private_exclusive_and_does_not_grant_source_writes() {
    let f = Fixture::new(false);
    let first = f.build().unwrap();
    let second = f.build().unwrap();
    assert!(first.provenance.starts_with(&f.metadata));
    assert_ne!(first.provenance, second.provenance);
    assert_eq!(first.directory.metadata().unwrap().mode() & 0o7777, 0o700);
    assert_eq!(
        identity(&first.directory).unwrap(),
        (first.identity.device.get(), first.identity.inode.get())
    );
    assert!(!f.source.write_allowed);
    drop(first);
    drop(second);
    assert_eq!(Fixture::count(&f.metadata), 2);
}
#[test]
fn metadata_inside_source_uses_only_source_parent() {
    let f = Fixture::new(true);
    let base = f.build().unwrap();
    assert_eq!(base.provenance.parent(), f.source.source_path.parent());
    assert_eq!(Fixture::count(&f.metadata), 0);
    assert_eq!(
        identity(&base.directory).unwrap().0,
        f.source.source_identity.device.get()
    );
}
#[test]
fn changed_source_wrong_metadata_identity_and_symlink_never_fallback() {
    for variant in 0..3 {
        let mut f = Fixture::new(false);
        match variant {
            0 => f.source.source_identity.inode = DecimalU64::new(0),
            1 => {
                let other = f.metadata.with_extension("other");
                fs::create_dir(&other).unwrap();
                f.metadata = other;
            }
            _ => {
                let old = f.metadata.with_extension("old");
                fs::rename(&f.metadata, &old).unwrap();
                symlink(old, &f.metadata).unwrap();
            }
        }
        let error = f.build().err().unwrap();
        assert!(!error.may_have_created());
        assert_eq!(Fixture::count(f.source.source_path.parent().unwrap()), 0);
    }
}
#[test]
fn collision_is_terminal_without_sibling_fallback() {
    let f = Fixture::new(false);
    let error = prepare(
        &f.source,
        &f.root,
        &f.metadata,
        &f.project,
        &f.session,
        |parent, name| {
            let name = CString::new(name).unwrap();
            assert_eq!(
                unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) },
                0
            );
        },
        |_, _| panic!("EEXIST must stop before post-create path"),
    )
    .err()
    .unwrap();
    assert!(!error.may_have_created());
    assert_eq!(Fixture::count(&f.metadata), 1);
    assert_eq!(Fixture::count(f.source.source_path.parent().unwrap()), 0);
}
#[test]
fn post_mkdir_failure_retains_selected_parent_proof_without_fallback() {
    let f = Fixture::new(false);
    let error = prepare(
        &f.source,
        &f.root,
        &f.metadata,
        &f.project,
        &f.session,
        |_, _| {},
        |parent, name| {
            let name = CString::new(name).unwrap();
            assert_eq!(
                unsafe { libc::fchmodat(parent.as_raw_fd(), name.as_ptr(), 0o500, 0) },
                0
            );
        },
    )
    .err()
    .unwrap();
    assert!(error.may_have_created());
    let path = error.retained_provenance().unwrap().to_path_buf();
    assert!(path.starts_with(&f.metadata));
    let retained = error.retained.as_ref().unwrap();
    assert_eq!(
        identity(&retained.parent).unwrap(),
        f.root.identity().unwrap()
    );
    assert!(retained.child.is_some());
    assert_eq!(Fixture::count(f.source.source_path.parent().unwrap()), 0);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
    drop(error);
    assert!(path.is_dir());
}
#[test]
fn parent_rename_after_create_keeps_original_fd_evidence() {
    let f = Fixture::new(false);
    let moved = f.metadata.with_extension("moved");
    let error = prepare(
        &f.source,
        &f.root,
        &f.metadata,
        &f.project,
        &f.session,
        |_, _| {},
        |_, _| {
            fs::rename(&f.metadata, &moved).unwrap();
            fs::create_dir(&f.metadata).unwrap();
        },
    )
    .err()
    .unwrap();
    assert!(error.may_have_created());
    assert_eq!(Fixture::count(&f.metadata), 0);
    assert_eq!(Fixture::count(&moved), 1);
    assert_eq!(
        identity(&error.retained.as_ref().unwrap().parent).unwrap(),
        identity(&File::open(&moved).unwrap()).unwrap()
    );
}

#[test]
fn registered_source_path_is_refused_before_any_directory_creation() {
    const MARKER: &str = "POLARIS_RECOVERY_BASE_SECRET_FIXTURE";
    if std::env::var_os(MARKER).is_none() {
        let home = tempfile::tempdir().unwrap();
        let output=std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact","recovery_base::tests::registered_source_path_is_refused_before_any_directory_creation","--test-threads=1"])
            .env_clear().env("HOME",home.path()).env(MARKER,"1").output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        return;
    }
    let mut f = Fixture::new(false);
    let reserved = f.source.source_path.with_file_name("registered-source");
    polaris_auth::api_key::save_to(&reserved, "synthetic-fixture-key").unwrap();
    fs::rename(&reserved, reserved.with_extension("old")).unwrap();
    fs::create_dir(&reserved).unwrap();
    let m = fs::metadata(&reserved).unwrap();
    f.source.source_path = reserved;
    f.source.source_identity = SourceApplyIdentity {
        device: DecimalU64::new(m.dev()),
        inode: DecimalU64::new(m.ino()),
    };
    let error = f.build().err().unwrap();
    assert!(!error.may_have_created());
    assert_eq!(Fixture::count(&f.metadata), 0);
    assert_eq!(Fixture::count(f.source.source_path.parent().unwrap()), 0);
}
