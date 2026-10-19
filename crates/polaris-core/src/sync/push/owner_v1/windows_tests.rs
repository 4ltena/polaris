use super::*;

struct UnvisitedHost;
impl SyncPushHostV1 for UnvisitedHost {
    fn require_confirmation_path(&self, _: &SyncPushConfirmationSummaryV1) -> Result<()> {
        panic!("unavailable factory must not visit the confirmation host")
    }
    fn require_recovery_invocation(&self, _: Operation) -> Result<()> {
        panic!("unavailable factory must not visit the recovery host")
    }
    fn confirm_abandon(&self, _: Operation, _: SyncDigestV1) -> Result<()> {
        panic!("unavailable factory must not visit the abandon host")
    }
    fn require_abandon_invocation(&self, _: Operation, _: SyncDigestV1) -> Result<()> {
        panic!("unavailable factory must not visit the abandon invocation host")
    }
}

fn assert_unavailable(path: &Path) {
    assert!(matches!(
        SyncPushOwnerV1::for_project(path, Box::new(UnvisitedHost)),
        Err(SyncPushErrorV1::UnavailablePlatform)
    ));
}

fn names(path: &Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<_> = std::fs::read_dir(path)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    names
}

#[test]
fn windows_sync_push_factory_refuses_before_creating_state_or_visiting_host() {
    let root = tempfile::tempdir().unwrap();
    let absent = root.path().join("absent-project");
    let before = names(root.path());
    assert_unavailable(&absent);
    assert_unavailable(Path::new("relative-project"));
    assert!(!absent.exists());
    assert_eq!(names(root.path()), before);
}

#[test]
fn windows_sync_push_factory_refuses_and_preserves_existing_names_and_bytes() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join(".polaris");
    let sync = state.join("sync");
    let nested = sync.join("nested");
    for path in [&state, &sync, &nested] {
        std::fs::create_dir(path).unwrap();
    }
    let files = [
        root.path().join("marker.bin"),
        sync.join("owner.lock"),
        sync.join("retained.json"),
        nested.join("retained.bin"),
    ];
    let contents: [&[u8]; 4] = [
        b"retained\0\xffmarker",
        b"retained-lock",
        b"retained-record",
        b"retained\0\xffnested",
    ];
    for (path, bytes) in files.iter().zip(contents) {
        std::fs::write(path, bytes).unwrap();
    }
    let before = [
        names(root.path()),
        names(&state),
        names(&sync),
        names(&nested),
    ];
    assert_unavailable(root.path());
    assert_unavailable(&files[0]);
    for (path, bytes) in files.iter().zip(contents) {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    assert_eq!(
        [
            names(root.path()),
            names(&state),
            names(&sync),
            names(&nested)
        ],
        before
    );
}
