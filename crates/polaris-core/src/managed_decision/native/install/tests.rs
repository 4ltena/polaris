//! Offline import transactions use synthetic bytes and never execute a worker.
use super::*;
use std::fs;

fn fixture() -> (tempfile::TempDir, Subject, PathBuf) {
    let (root, subject) = super::super::tests::fixture();
    let inputs = root.path().join("inputs");
    fs::rename(
        root.path()
            .join(".polaris/decision-runtime/generations/fixture-v1"),
        &inputs,
    )
    .unwrap();
    (root, subject, inputs)
}

fn generations(home: &Path) -> PathBuf {
    home.join(".polaris/decision-runtime/generations")
}

fn run(s: &Subject, home: &Path, inputs: &Path) -> Result<(), ManagedDecisionError> {
    import(
        s,
        home,
        &inputs.join("worker"),
        &inputs.join("model.gguf"),
        || false,
        || Ok(()),
    )
}

#[test]
fn import_publishes_a_complete_private_generation_and_preserves_older_versions() {
    let (root, mut s, inputs) = fixture();
    let home = root.path().canonicalize().unwrap();
    run(&s, &home, &inputs).unwrap();
    let first = generations(&home).join(&s.generation);
    assert_eq!(fs::read(first.join("model.gguf")).unwrap(), b"dummy");
    assert_eq!(
        fs::metadata(first.join("worker")).unwrap().mode() & 0o777,
        0o500
    );
    assert_eq!(
        fs::metadata(first.join("model.gguf")).unwrap().mode() & 0o777,
        0o400
    );
    assert_eq!(fs::metadata(first.join("model.gguf")).unwrap().nlink(), 1);
    assert_eq!(fs::read(inputs.join("model.gguf")).unwrap(), b"dummy");
    assert!(matches!(
        run(&s, &home, &inputs),
        Err(ManagedDecisionError::Conflict)
    ));
    s.generation = "fixture-v2".into();
    run(&s, &home, &inputs).unwrap();
    assert_eq!(
        fs::read(first.join("worker")).unwrap(),
        fs::read(generations(&home).join(&s.generation).join("worker")).unwrap()
    );
    assert!(!home.join(".polaris/config.toml").exists());
    assert!(!home.join(".polaris/managed-resources.toml").exists());
    assert_eq!(fs::read_dir(generations(&home)).unwrap().count(), 2);
}

#[test]
fn unaccepted_production_profile_cannot_import_or_even_consult_source_paths() {
    assert_eq!(
        install(
            "clef-flash-q4-cpu-v1",
            Path::new("/absent/worker"),
            Path::new("/absent/model"),
            || panic!("unaccepted profile began importing")
        ),
        Err(ManagedDecisionError::UnverifiedProfile)
    );
}

#[test]
fn bad_bytes_or_source_aliases_leave_no_generation_or_pending_directory() {
    for defect in ["digest", "truncated", "symlink"] {
        let (root, mut s, inputs) = fixture();
        let home = root.path().canonicalize().unwrap();
        match defect {
            "digest" => s.model.sha256 = "0".repeat(64),
            "truncated" => {
                s.model.bytes += 1;
            }
            "symlink" => {
                fs::rename(inputs.join("model.gguf"), inputs.join("original")).unwrap();
                std::os::unix::fs::symlink(inputs.join("original"), inputs.join("model.gguf"))
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(
            run(&s, &home, &inputs),
            Err(ManagedDecisionError::Integrity),
            "{defect}"
        );
        assert_eq!(
            fs::read_dir(generations(&home)).unwrap().count(),
            0,
            "{defect}"
        );
    }
}

#[test]
fn cancellation_after_copying_worker_is_clean_and_a_fresh_import_can_succeed() {
    let (root, s, inputs) = fixture();
    let home = root.path().canonicalize().unwrap();
    let mut checks = 0;
    let result = import(
        &s,
        &home,
        &inputs.join("worker"),
        &inputs.join("model.gguf"),
        || {
            checks += 1;
            checks == 4
        },
        || Ok(()),
    );
    assert_eq!(result, Err(ManagedDecisionError::Cancelled));
    assert_eq!(fs::read_dir(generations(&home)).unwrap().count(), 0);
    run(&s, &home, &inputs).unwrap();
}

#[test]
fn a_concurrent_destination_is_never_overwritten() {
    let (root, s, inputs) = fixture();
    let home = root.path().canonicalize().unwrap();
    let target = generations(&home).join(&s.generation);
    let result = import(
        &s,
        &home,
        &inputs.join("worker"),
        &inputs.join("model.gguf"),
        || false,
        || {
            fs::create_dir(&target).unwrap();
            fs::write(target.join("other-install"), b"keep").unwrap();
            Ok(())
        },
    );
    assert_eq!(result, Err(ManagedDecisionError::Conflict));
    assert_eq!(fs::read(target.join("other-install")).unwrap(), b"keep");
    assert!(!target.join("worker").exists());
    assert_eq!(fs::read_dir(generations(&home)).unwrap().count(), 1);
}

#[test]
fn a_swapped_parent_is_not_followed_for_publication_or_cleanup() {
    let (root, s, inputs) = fixture();
    let home = root.path().canonicalize().unwrap();
    let outside = home.join("outside");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("keep"), b"unrelated").unwrap();
    let old = home.join("original-generations");
    let result = import(
        &s,
        &home,
        &inputs.join("worker"),
        &inputs.join("model.gguf"),
        || false,
        || {
            fs::rename(generations(&home), &old).unwrap();
            std::os::unix::fs::symlink(&outside, generations(&home)).unwrap();
            Ok(())
        },
    );
    assert_eq!(result, Err(ManagedDecisionError::Integrity));
    assert_eq!(fs::read_dir(old).unwrap().count(), 0);
    assert_eq!(fs::read_dir(&outside).unwrap().count(), 1);
    assert_eq!(fs::read(outside.join("keep")).unwrap(), b"unrelated");
}
