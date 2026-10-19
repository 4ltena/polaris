use super::*;

#[test]
fn windows_private_store_verification_does_not_create_a_missing_root() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("absent");
    assert_eq!(
        VerificationStore::open(&root, "project")
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert!(!root.exists());
    assert_eq!(fs::read_dir(parent.path()).unwrap().count(), 0);
}

#[test]
fn windows_private_store_verification_preserves_existing_records() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().join("verification-records-v1");
    let nested = directory.join("retained");
    fs::create_dir_all(&nested).unwrap();
    let record = directory.join("record.bin");
    let binary = nested.join("deep.bin");
    fs::write(&record, b"retained\0\xffrecord").unwrap();
    fs::write(&binary, b"retained\0\xffnested").unwrap();
    let names = |path: &Path| {
        let mut names: Vec<_> = fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    let before_names = [names(root.path()), names(&directory), names(&nested)];
    let before_bytes = [fs::read(&record).unwrap(), fs::read(&binary).unwrap()];
    assert_eq!(
        VerificationStore::open(root.path(), "project")
            .unwrap_err()
            .kind(),
        io::ErrorKind::Unsupported
    );
    assert_eq!(
        [names(root.path()), names(&directory), names(&nested)],
        before_names
    );
    assert_eq!(
        [fs::read(&record).unwrap(), fs::read(&binary).unwrap()],
        before_bytes
    );
}
