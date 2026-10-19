//! Windows credential rebind intent inspection refusal and storage preservation tests.
use super::*;

fn assert_unavailable(path: &Path) {
    match C08IntentStoreV1::inspect(path) {
        Err(CredentialRebindErrorV1::Unavailable(message)) => {
            assert_eq!(
                message,
                "credential rebind storage is unavailable on this platform"
            );
        }
        _ => panic!("Windows C08 inspection must refuse before touching the state"),
    }
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
fn windows_credential_rebind_store_inspect_keeps_absent_root_absent() {
    let root = tempfile::tempdir().unwrap();
    let absent = root.path().join("absent-state");
    let before = names(root.path());
    assert_unavailable(&absent);
    assert_unavailable(Path::new("relative-state"));
    assert!(!absent.exists());
    assert_eq!(names(root.path()), before);
}

#[test]
fn windows_credential_rebind_store_inspect_preserves_existing_names_and_bytes() {
    let root = tempfile::tempdir().unwrap();
    let sync = root.path().join("sync");
    let store = sync.join("credential-rebind-v1");
    let archive = store.join("archive");
    for path in [&sync, &store, &archive] {
        std::fs::create_dir(path).unwrap();
    }
    let files = [
        root.path().join("marker.bin"),
        store.join("owner.lock"),
        store.join("00000000000000000000000000000007.json"),
        archive.join("retained.bin"),
    ];
    let contents: [&[u8]; 4] = [
        b"retained\0\xffmarker",
        b"retained-lock",
        b"retained-record",
        b"retained\0\xffarchive",
    ];
    for (path, bytes) in files.iter().zip(contents) {
        std::fs::write(path, bytes).unwrap();
    }
    let before = [
        names(root.path()),
        names(&sync),
        names(&store),
        names(&archive),
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
            names(&archive)
        ],
        before
    );
}
