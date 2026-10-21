//! Read-only reuse verification with tiny synthetic files; no process or weights.
use super::*;
use std::{fs, os::unix::fs::PermissionsExt};
#[test]
fn verified_generation_binds_subject_hashes_without_activation_or_scratch() {
    let (root, s) = super::super::tests::fixture();
    let home = root.path().canonicalize().unwrap();
    let report = verify_with(
        &super::super::tests::lock(&s),
        &s.id,
        || Ok(home.clone()),
        || false,
    )
    .unwrap();
    assert_eq!(report.verified_bytes, s.worker.bytes + s.model.bytes);
    assert_eq!(report.model_revision, s.model_revision);
    assert_eq!(report.quantization, s.quantization);
    assert_eq!(report.model_sha256, s.model.sha256);
    assert_eq!(report.worker_sha256, s.worker.sha256);
    assert_eq!(report.generation, s.generation);
    assert_eq!(
        fs::read_dir(home.join(".polaris/decision-runtime/runs"))
            .unwrap()
            .count(),
        0
    );
    assert!(!home.join(".polaris/config.toml").exists());
}
#[test]
fn same_size_corruption_of_either_artifact_is_refused() {
    for name in ["worker", "model.gguf"] {
        let (root, s) = super::super::tests::fixture();
        let home = root.path().canonicalize().unwrap();
        let path = home
            .join(".polaris/decision-runtime/generations/fixture-v1")
            .join(name);
        let size = fs::metadata(&path).unwrap().len();
        let mode = fs::metadata(&path).unwrap().permissions();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(&path, vec![b'x'; size as usize]).unwrap();
        fs::set_permissions(&path, mode).unwrap();
        assert!(matches!(
            verify_with(&super::super::tests::lock(&s), &s.id, || Ok(home), || false),
            Err(ManagedDecisionError::Integrity)
        ));
    }
}
#[test]
fn cancellation_and_unregistered_profiles_do_not_consult_home() {
    assert!(matches!(
        verify_with(
            LOCK,
            "unregistered",
            || panic!("unregistered profile read home"),
            || false
        ),
        Err(ManagedDecisionError::UnverifiedProfile)
    ));
    assert!(matches!(
        verify_with(
            LOCK,
            "unregistered",
            || panic!("cancelled verification read home"),
            || true
        ),
        Err(ManagedDecisionError::Cancelled)
    ));
}
#[test]
fn symlinks_and_leaf_replacement_during_verification_are_refused() {
    let (root, s) = super::super::tests::fixture();
    let home = root.path().canonicalize().unwrap();
    let model = home.join(".polaris/decision-runtime/generations/fixture-v1/model.gguf");
    let file = File::open(&model).unwrap();
    let mut changed = false;
    assert!(matches!(
        verify_file(&file, &model, &s.model, &mut || {
            if !changed {
                changed = true;
                fs::rename(&model, model.with_extension("old")).unwrap();
                fs::write(&model, b"dummy").unwrap();
            }
            false
        }),
        Err(ManagedDecisionError::Integrity)
    ));
    fs::remove_file(&model).unwrap();
    std::os::unix::fs::symlink(model.with_extension("old"), &model).unwrap();
    assert!(matches!(
        verify_with(&super::super::tests::lock(&s), &s.id, || Ok(home), || false),
        Err(ManagedDecisionError::Integrity)
    ));
}

#[test]
fn replacing_a_verified_worker_during_model_hashing_refuses_the_generation() {
    let (root, s) = super::super::tests::fixture();
    let home = root.path().canonicalize().unwrap();
    let worker = home.join(".polaris/decision-runtime/generations/fixture-v1/worker");
    let mut calls = 0;
    let result = verify_with(
        &super::super::tests::lock(&s),
        &s.id,
        || Ok(home.clone()),
        || {
            calls += 1;
            // Initial cancel check, worker data/EOF, then model's first check.
            if calls == 4 {
                let old = worker.with_extension("old");
                fs::rename(&worker, &old).unwrap();
                fs::copy(old, &worker).unwrap();
            }
            false
        },
    );
    assert!(matches!(result, Err(ManagedDecisionError::Integrity)));
    assert!(calls >= 4);
}
