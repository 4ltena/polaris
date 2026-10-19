use super::*;

fn assert_unavailable(path: &Path) {
    // The existing test verifier selects the real acquisition boundary.
    // It neither constructs a credential owner nor returns a native lease.
    for verified in [false, true] {
        match CredentialRebindOwnerV1::acquire_inner(path, BindingVerifier::Test(verified)) {
            Err(CredentialRebindErrorV1::Unavailable(message)) => {
                assert_eq!(message, "credential rebind is unavailable on this platform");
            }
            _ => panic!("Windows C08 acquisition must refuse before project or credential access"),
        }
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
fn windows_credential_rebind_acquire_keeps_absent_project_absent() {
    let root = tempfile::tempdir().unwrap();
    let absent = root.path().join("absent-project");
    let before = names(root.path());
    assert_unavailable(&absent);
    assert_unavailable(Path::new("relative-project"));
    assert!(!absent.exists());
    assert_eq!(names(root.path()), before);
}

#[test]
fn windows_credential_rebind_acquire_preserves_existing_names_and_bytes() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join(".polaris");
    let nested = state.join("sync");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir(&nested).unwrap();
    let files = [
        root.path().join("marker.bin"),
        state.join("participation.json"),
        nested.join("owner.lock"),
    ];
    let contents: [&[u8]; 3] = [
        b"retained\0\xffmarker",
        b"retained-participation",
        b"retained-lock",
    ];
    for (path, bytes) in files.iter().zip(contents) {
        std::fs::write(path, bytes).unwrap();
    }
    let before = [names(root.path()), names(&state), names(&nested)];
    assert_unavailable(root.path());
    assert_unavailable(&files[0]);
    for (path, bytes) in files.iter().zip(contents) {
        assert_eq!(std::fs::read(path).unwrap(), bytes);
    }
    assert_eq!([names(root.path()), names(&state), names(&nested)], before);
}
