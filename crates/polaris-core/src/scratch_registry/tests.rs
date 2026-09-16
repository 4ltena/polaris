//! Temporary directories only; the registry path is process-wide, so these
//! tests share one install and use distinct copies.
use super::*;
use std::os::unix::fs::DirBuilderExt;

fn temporary() -> PathBuf {
    fs::canonicalize(std::env::temp_dir()).unwrap()
}

/// A copy shaped exactly like a real one.
fn copy(name: &str) -> PathBuf {
    let path = temporary().join(format!("{PREFIX}{name}"));
    let _ = fs::remove_dir_all(&path);
    fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
    fs::write(path.join("large-build-output"), "x").unwrap();
    path
}

fn installed() -> &'static Path {
    static ONCE: OnceLock<tempfile::TempDir> = OnceLock::new();
    let directory = ONCE.get_or_init(|| tempfile::tempdir().unwrap());
    install(directory.path());
    REGISTRY.get().unwrap()
}

#[test]
fn only_recorded_copies_whose_owner_is_gone_are_removed() {
    let registry = installed();
    let orphan = copy("orphan");
    let live = copy("live");
    let young = copy("young");
    let stranger = copy("stranger");
    write(
        registry,
        &Registry {
            entries: vec![
                Entry {
                    path: orphan.clone(),
                    pid: 999_999,
                    created_unix_seconds: 0,
                },
                Entry {
                    path: live.clone(),
                    pid: 4242,
                    created_unix_seconds: 0,
                },
                Entry {
                    path: young.clone(),
                    pid: 999_999,
                    created_unix_seconds: 1_000,
                },
            ],
            last_sweep_unix_seconds: 0,
        },
    )
    .unwrap();

    let swept = sweep(1_100, 300, |pid| pid == 4242).unwrap();
    assert_eq!(swept.removed, vec![orphan.clone()]);
    assert_eq!(swept.kept, 2, "a live owner and a young copy are kept");
    assert!(swept.refused.is_empty());
    assert!(!orphan.exists());
    assert!(live.exists() && young.exists());
    // A copy that was never recorded is never even looked at.
    assert!(stranger.exists());

    let held = read(registry);
    assert_eq!(held.entries.len(), 2);
    assert_eq!(held.last_sweep_unix_seconds, 1_100);
    for path in [live, young, stranger] {
        let _ = fs::remove_dir_all(path);
    }

    refused_paths_are_not_repaired(registry);
    a_gone_copy_drops_its_record(registry);
}

/// The registry path is process-wide, so these stages share one test.
fn refused_paths_are_not_repaired(registry: &Path) {
    let renamed = temporary().join("not-a-polaris-copy");
    let _ = fs::remove_dir_all(&renamed);
    fs::DirBuilder::new().mode(0o700).create(&renamed).unwrap();
    let loosened = copy("loosened");
    fs::set_permissions(&loosened, fs::Permissions::from_mode(0o755)).unwrap();
    let elsewhere = tempfile::tempdir().unwrap();
    let outside = elsewhere.path().join(format!("{PREFIX}outside"));
    fs::DirBuilder::new().mode(0o700).create(&outside).unwrap();

    write(
        registry,
        &Registry {
            entries: [&renamed, &loosened, &outside]
                .into_iter()
                .map(|path| Entry {
                    path: path.clone(),
                    pid: 999_999,
                    created_unix_seconds: 0,
                })
                .collect(),
            last_sweep_unix_seconds: 0,
        },
    )
    .unwrap();

    let swept = sweep(10_000, 300, |_| false).unwrap();
    assert!(swept.removed.is_empty());
    assert_eq!(swept.refused.len(), 3);
    assert!(renamed.exists() && loosened.exists() && outside.exists());
    // Refused entries stay recorded so the refusal stays visible.
    assert_eq!(read(registry).entries.len(), 3);
    let _ = fs::remove_dir_all(renamed);
    let _ = fs::remove_dir_all(loosened);
}

fn a_gone_copy_drops_its_record(registry: &Path) {
    let missing = temporary().join(format!("{PREFIX}missing"));
    let _ = fs::remove_dir_all(&missing);
    write(
        registry,
        &Registry {
            entries: vec![Entry {
                path: missing,
                pid: 999_999,
                created_unix_seconds: 0,
            }],
            last_sweep_unix_seconds: 0,
        },
    )
    .unwrap();
    let swept = sweep(10_000, 300, |_| false).unwrap();
    assert_eq!(swept, Swept::default());
    assert!(read(registry).entries.is_empty());
}
