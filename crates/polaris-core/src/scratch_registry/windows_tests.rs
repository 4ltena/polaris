use super::*;
use std::cell::Cell;

#[test]
fn windows_scratch_registry_unavailable_refuses_before_alive_callback() {
    let called = Cell::new(false);
    let error = sweep(RETENTION_SECONDS + 1, RETENTION_SECONDS, |_| {
        called.set(true);
        false
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(!called.get());
}

#[test]
fn windows_scratch_registry_unavailable_preserves_installed_registry_and_copy() {
    let root = tempfile::tempdir().unwrap();
    let state = root.path().join("state");
    let copy = root.path().join("polaris-workspace-retained");
    let nested = copy.join("nested");
    let artifact = copy.join("old.bin");
    let deep = nested.join("older.bin");
    std::fs::create_dir(&state).unwrap();
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(&artifact, b"retained\0\xffcopy").unwrap();
    std::fs::write(&deep, b"retained\0\xffnested").unwrap();
    let old = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1);
    for path in [&artifact, &deep] {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(old))
            .unwrap();
    }
    let registry = state.join(FILE);
    let recorded = Registry {
        entries: vec![Entry {
            path: copy.clone(),
            pid: u32::MAX,
            created_unix_seconds: 0,
        }],
        last_sweep_unix_seconds: 0,
    };
    let encoded = serde_json::to_vec(&recorded).unwrap();
    std::fs::write(&registry, &encoded).unwrap();
    install(&state);
    assert_eq!(REGISTRY.get(), Some(&registry));
    let names = |path: &Path| {
        let mut names: Vec<_> = std::fs::read_dir(path)
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        names.sort();
        names
    };
    let before_names = [
        names(root.path()),
        names(&state),
        names(&copy),
        names(&nested),
    ];
    let before: Vec<_> = [&registry, &artifact, &deep]
        .into_iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    let called = Cell::new(false);
    let error = sweep(RETENTION_SECONDS + 1, RETENTION_SECONDS, |_| {
        called.set(true);
        false
    })
    .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::Unsupported);
    assert!(!called.get());
    assert_eq!(
        [
            names(root.path()),
            names(&state),
            names(&copy),
            names(&nested),
        ],
        before_names
    );
    let after: Vec<_> = [&registry, &artifact, &deep]
        .into_iter()
        .map(|path| std::fs::read(path).unwrap())
        .collect();
    assert_eq!(after, before);
}
