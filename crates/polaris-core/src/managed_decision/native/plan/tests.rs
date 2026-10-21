//! Consent and inventory fixtures; no downloads, credentials or real weights.
use super::*;
use std::fs;

fn fixture() -> (tempfile::TempDir, Vec<u8>, PathBuf, PathBuf) {
    let (root, s) = super::super::tests::fixture();
    let sources = root.path().join("inputs");
    fs::rename(
        root.path()
            .join(".polaris/decision-runtime/generations/fixture-v1"),
        &sources,
    )
    .unwrap();
    let home = root.path().join("new-home");
    fs::create_dir(&home).unwrap();
    let mut lock: serde_json::Value =
        serde_json::from_slice(&super::super::tests::lock(&s)).unwrap();
    lock["profiles"][0]["delivery"] = serde_json::json!({
        "purpose":"synthetic test decisions", "worker_source":"https://example.invalid/worker", "model_source":"https://example.invalid/model",
        "license":"synthetic-only", "license_source":"https://example.invalid/LICENSE", "notice_source":"https://example.invalid/NOTICE",
        "components":[{"name":"synthetic-worker", "version":"1", "license":"synthetic-only", "source":"https://example.invalid/component"}]
    });
    (root, serde_json::to_vec(&lock).unwrap(), home, sources)
}

fn plan(bytes: &[u8], home: &Path, sources: &Path) -> ImportPlan {
    make(
        bytes,
        "fixture-cpu-v1",
        home,
        &sources.join("worker"),
        &sources.join("model.gguf"),
        "synthetic-transaction-1",
    )
    .unwrap()
}

#[test]
fn a_read_only_plan_displays_exact_inventory_then_a_bound_import_can_commit() {
    let (_root, bytes, home, sources) = fixture();
    let plan = plan(&bytes, &home, &sources);
    let visible = serde_json::to_value(&plan).unwrap();
    assert_eq!(visible["installed_bytes"], 37);
    assert_eq!(visible["delivery"]["license"], "synthetic-only");
    assert_eq!(visible["model"]["bytes"], 5);
    assert_eq!(visible["model_revision"], "a".repeat(40));
    assert_eq!(visible["runtime_revision"], RUNTIME_REVISION);
    assert!(!home.join(".polaris").exists());
    let consent = plan.consent_sha256().unwrap();
    execute_with(&bytes, plan, &consent, || false, || Ok(home.clone())).unwrap();
    assert_eq!(
        fs::read(home.join(".polaris/decision-runtime/generations/fixture-v1/model.gguf")).unwrap(),
        b"dummy"
    );
    assert!(!home.join(".polaris/config.toml").exists());
}

#[test]
fn a_missing_inventory_or_secret_bearing_provenance_never_produces_a_plan() {
    let (_root, bytes, home, sources) = fixture();
    for value in [serde_json::Value::Null, serde_json::json!({})] {
        let mut lock: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        lock["profiles"][0]["delivery"] = value;
        assert!(
            make(
                &serde_json::to_vec(&lock).unwrap(),
                "fixture-cpu-v1",
                &home,
                &sources.join("worker"),
                &sources.join("model.gguf"),
                "txn"
            )
            .is_err()
        );
    }
    for bad in [
        "http://example.invalid/file",
        "https://secret@example.invalid/file",
        "https://example.invalid/file?token=dummy",
        "https://example.invalid/file#dummy",
    ] {
        let mut lock: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        lock["profiles"][0]["delivery"]["model_source"] = bad.into();
        assert!(matches!(
            make(
                &serde_json::to_vec(&lock).unwrap(),
                "fixture-cpu-v1",
                &home,
                &sources.join("worker"),
                &sources.join("model.gguf"),
                "txn"
            ),
            Err(ManagedDecisionError::MissingDeliveryMetadata)
        ));
    }
    assert!(!home.join(".polaris").exists());
}

#[test]
fn changed_selection_destination_or_package_cannot_reuse_consent() {
    let (_root, bytes, home, sources) = fixture();
    let original = plan(&bytes, &home, &sources);
    let consent = original.consent_sha256().unwrap();
    let changed_nonce = make(
        &bytes,
        "fixture-cpu-v1",
        &home,
        &sources.join("worker"),
        &sources.join("model.gguf"),
        "another-transaction",
    )
    .unwrap();
    assert_ne!(changed_nonce.consent_sha256().unwrap(), consent);
    assert!(matches!(
        execute_with(
            &bytes,
            changed_nonce,
            &consent,
            || false,
            || panic!("consent mismatch read home")
        ),
        Err(ManagedDecisionError::PlanChanged)
    ));
    let other_home = home.join("different");
    assert!(matches!(
        execute_with(&bytes, original, &consent, || false, || Ok(other_home)),
        Err(ManagedDecisionError::PlanChanged)
    ));
    let p = plan(&bytes, &home, &sources);
    let mut changed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    changed["profiles"][0]["delivery"]["purpose"] = "a changed purpose".into();
    assert!(matches!(
        execute_with(
            &serde_json::to_vec(&changed).unwrap(),
            p,
            &consent,
            || false,
            || Ok(home.clone())
        ),
        Err(ManagedDecisionError::PlanChanged)
    ));
    assert!(!home.join(".polaris").exists());
}

#[test]
fn cancellation_and_relative_sources_have_no_import_effects() {
    let (_root, bytes, home, sources) = fixture();
    assert!(
        make(
            &bytes,
            "fixture-cpu-v1",
            &home,
            Path::new("relative-worker"),
            &sources.join("model.gguf"),
            "txn"
        )
        .is_err()
    );
    let p = plan(&bytes, &home, &sources);
    let consent = p.consent_sha256().unwrap();
    assert!(matches!(
        execute_with(&bytes, p, &consent, || true, || Ok(home.clone())),
        Err(ManagedDecisionError::Cancelled)
    ));
    assert!(!home.join(".polaris").exists());
    assert!(registered_profiles().unwrap().is_empty());
}

#[test]
fn repair_consent_binds_the_operation_existing_identity_and_backup_destination() {
    let (_root, bytes, home, sources) = fixture();
    let initial = plan(&bytes, &home, &sources);
    let consent = initial.consent_sha256().unwrap();
    execute_with(&bytes, initial, &consent, || false, || Ok(home.clone())).unwrap();
    let prepare = || {
        make_operation(
            &bytes,
            "fixture-cpu-v1",
            &home,
            &sources.join("worker"),
            &sources.join("model.gguf"),
            &"d".repeat(32),
            ImportOperation::Repair,
        )
        .unwrap()
    };
    let repair = prepare();
    let visible = serde_json::to_value(&repair).unwrap();
    assert_eq!(visible["operation"], "repair");
    assert!(
        visible["replacement"]["backup_destination"]
            .as_str()
            .unwrap()
            .contains(".recovery-")
    );
    let repaired_consent = repair.consent_sha256().unwrap();
    assert_ne!(repaired_consent, consent);
    assert_eq!(
        execute_with(
            &bytes,
            repair,
            &consent,
            || false,
            || panic!("wrong consent consulted home")
        ),
        Err(ManagedDecisionError::PlanChanged)
    );
    let repair = prepare();
    let backup = repair.recovery_backup().unwrap().to_owned();
    execute_with(
        &bytes,
        repair,
        &repaired_consent,
        || false,
        || Ok(home.clone()),
    )
    .unwrap();
    assert_eq!(fs::read(backup.join("model.gguf")).unwrap(), b"dummy");
    assert_eq!(
        fs::read(home.join(".polaris/decision-runtime/generations/fixture-v1/model.gguf")).unwrap(),
        b"dummy"
    );
    assert!(prepare().consent_sha256().unwrap() != repaired_consent);
}
