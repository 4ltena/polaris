//! Package-lock and private-generation fixtures; no real model is executed.
use super::*;
use std::{fs, os::unix::fs::PermissionsExt};

pub(super) fn fixture() -> (tempfile::TempDir, Subject) {
    let root = tempfile::tempdir().unwrap();
    let home = root.path().canonicalize().unwrap();
    let generation = home.join(".polaris/decision-runtime/generations/fixture-v1");
    fs::create_dir_all(&generation).unwrap();
    for path in [
        home.join(".polaris"),
        home.join(".polaris/decision-runtime"),
        home.join(".polaris/decision-runtime/generations"),
        generation.clone(),
    ] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700)).unwrap();
    }
    let runs = home.join(".polaris/decision-runtime/runs");
    fs::create_dir(&runs).unwrap();
    fs::set_permissions(&runs, fs::Permissions::from_mode(0o700)).unwrap();
    let worker = b"synthetic worker, never executed";
    fs::write(generation.join("worker"), worker).unwrap();
    fs::set_permissions(generation.join("worker"), fs::Permissions::from_mode(0o500)).unwrap();
    fs::write(generation.join("model.gguf"), b"dummy").unwrap();
    fs::set_permissions(
        generation.join("model.gguf"),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    let subject = Subject {
        id: "fixture-cpu-v1".into(),
        generation: "fixture-v1".into(),
        platform: "macos".into(),
        architecture: if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "x86_64"
        }
        .into(),
        model_repository: "synthetic/model".into(),
        model_revision: "a".repeat(40),
        quantization: "synthetic".into(),
        runtime_revision: RUNTIME_REVISION.into(),
        protocol: 1,
        worker: LockedFile {
            bytes: worker.len() as u64,
            sha256: format!("{:x}", Sha256::digest(worker)),
        },
        model: LockedFile {
            bytes: 5,
            sha256: format!("{:x}", Sha256::digest(b"dummy")),
        },
        device: Device::Cpu,
        context_tokens: 1024,
        request_timeout_ms: 1000,
        load_timeout_ms: 1000,
        peak_bytes: 1,
        peak_evidence: PeakKind::ValidatedUpperBound,
        reserve_bytes: 0,
    };
    (root, subject)
}

pub(super) fn lock(subject: &Subject) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({"schema_version":1,"profiles":[{
        "subject": subject,
        "acceptance": {"subject_sha256": format!("{:x}", Sha256::digest(serde_json::to_vec(subject).unwrap())), "evidence_sha256": format!("{:x}", Sha256::digest(b"synthetic acceptance"))}
    }]})).unwrap()
}

#[test]
fn a_valid_package_subject_builds_a_lazy_owner_from_fixed_paths() {
    let (root, s) = fixture();
    let home = root.path().canonicalize().unwrap();
    let config = prepare(&lock(&s), &s.id, || Ok(home.clone())).unwrap();
    assert_eq!(
        config.worker,
        home.join(".polaris/decision-runtime/generations/fixture-v1/worker")
    );
    assert_eq!(config.model_sha256, s.model.sha256);
    let guard = config.admission.as_ref().unwrap();
    assert!(guard.profile.accepted);
    assert_eq!(
        guard.profile.peak.unwrap().evidence,
        PeakEvidence::ValidatedUpperBound
    );
    let decider = ClefDecider::new(config).unwrap();
    assert_eq!(decider.name(), "clef");
    let identity = decider.identity();
    assert_eq!(identity.model.as_deref(), Some(s.model_repository.as_str()));
    assert_eq!(
        identity.revision.as_deref(),
        Some(s.model_revision.as_str())
    );
    assert_eq!(
        identity.quantization.as_deref(),
        Some(s.quantization.as_str())
    );
    assert_eq!(
        fs::read_dir(home.join(".polaris/decision-runtime/runs"))
            .unwrap()
            .count(),
        0
    );
}

#[test]
fn the_empty_production_lock_never_consults_home_or_uses_an_adjacent_receipt() {
    assert!(matches!(
        prepare(LOCK, "clef-flash-q4-cpu-v1", || panic!(
            "unverified profile read OS home"
        )),
        Err(ManagedDecisionError::UnverifiedProfile)
    ));
    let (root, s) = fixture();
    fs::write(root.path().join("approved.json"), lock(&s)).unwrap();
    assert!(matches!(
        prepare(LOCK, &s.id, || Ok(root.path().to_owned())),
        Err(ManagedDecisionError::UnverifiedProfile)
    ));
}

#[test]
fn profile_changes_cannot_reuse_another_subjects_acceptance() {
    let (_root, s) = fixture();
    let original = lock(&s);
    for (key, value) in [
        ("runtime_revision", serde_json::json!("different")),
        ("context_tokens", serde_json::json!(2048)),
        ("peak_bytes", serde_json::json!(2)),
        ("device", serde_json::json!("metal")),
    ] {
        let mut changed: serde_json::Value = serde_json::from_slice(&original).unwrap();
        changed["profiles"][0]["subject"][key] = value;
        assert!(
            matches!(
                select(&serde_json::to_vec(&changed).unwrap(), &s.id),
                Err(ManagedDecisionError::Integrity)
            ),
            "{key}"
        );
    }
    let mut duplicate: serde_json::Value = serde_json::from_slice(&original).unwrap();
    let repeated = duplicate["profiles"][0].clone();
    duplicate["profiles"].as_array_mut().unwrap().push(repeated);
    assert!(matches!(
        select(&serde_json::to_vec(&duplicate).unwrap(), &s.id),
        Err(ManagedDecisionError::Integrity)
    ));
}

#[test]
fn untrusted_paths_devices_and_partial_installations_fail_without_fallback() {
    let (root, mut s) = fixture();
    let home = root.path().canonicalize().unwrap();
    s.generation = "../elsewhere".into();
    assert!(matches!(
        prepare(&lock(&s), &s.id, || Ok(home.clone())),
        Err(ManagedDecisionError::Integrity)
    ));
    s.generation = "fixture-v1".into();
    s.device = Device::Metal;
    assert!(matches!(
        prepare(&lock(&s), &s.id, || panic!(
            "unsupported device looked up home"
        )),
        Err(ManagedDecisionError::UnsupportedDevice)
    ));
    s.device = Device::Cpu;
    fs::remove_file(home.join(".polaris/decision-runtime/generations/fixture-v1/model.gguf"))
        .unwrap();
    assert!(matches!(
        prepare(&lock(&s), &s.id, || Ok(home)),
        Err(ManagedDecisionError::NotInstalled)
    ));
}

#[test]
fn aliases_permissions_and_file_metadata_do_not_become_approved_generations() {
    for defect in ["symlink", "hardlink", "permissions", "truncated"] {
        let (root, s) = fixture();
        let home = root.path().canonicalize().unwrap();
        let model = home.join(".polaris/decision-runtime/generations/fixture-v1/model.gguf");
        match defect {
            "symlink" => {
                fs::rename(&model, home.join("actual")).unwrap();
                std::os::unix::fs::symlink(home.join("actual"), &model).unwrap();
            }
            "hardlink" => fs::hard_link(&model, home.join("alias")).unwrap(),
            "permissions" => {
                fs::set_permissions(&model, fs::Permissions::from_mode(0o666)).unwrap()
            }
            "truncated" => fs::write(&model, b"no").unwrap(),
            _ => unreachable!(),
        }
        assert!(
            matches!(
                prepare(&lock(&s), &s.id, || Ok(home)),
                Err(ManagedDecisionError::Integrity)
            ),
            "{defect}"
        );
    }
}
