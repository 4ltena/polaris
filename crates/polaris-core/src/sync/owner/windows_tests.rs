//! Windows sync remote intent owner refusal and storage preservation tests.
use super::*;

fn assert_unavailable(path: &Path) {
    assert!(matches!(
        SyncRemoteIntentOwnerV1::open(path),
        Err(SyncRemoteOwnerErrorV1::LegacyUnavailable)
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
fn windows_sync_owner_open_refuses_before_creating_state_or_store() {
    let root = tempfile::tempdir().unwrap();
    let absent = root.path().join("absent-state");
    let before = names(root.path());
    assert_unavailable(&absent);
    assert_unavailable(Path::new("relative-state"));
    assert!(!absent.exists());
    assert_eq!(names(root.path()), before);
}

#[test]
fn windows_sync_owner_open_refuses_and_preserves_existing_names_and_bytes() {
    let root = tempfile::tempdir().unwrap();
    let sync = root.path().join("sync");
    let store = sync.join("remote-intents-v1");
    let nested = store.join("nested");
    for path in [&sync, &store, &nested] {
        std::fs::create_dir(path).unwrap();
    }
    let files = [
        root.path().join("marker.bin"),
        store.join("owner.lock"),
        store.join("retained.json"),
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
        names(&sync),
        names(&store),
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
            names(&sync),
            names(&store),
            names(&nested)
        ],
        before
    );
}
