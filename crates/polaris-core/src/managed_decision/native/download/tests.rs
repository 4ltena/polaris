//! Dummy transfer and private filesystem fixtures; no external model downloads.
use super::*;
use std::{
    collections::VecDeque,
    fs,
    os::unix::fs::PermissionsExt,
    sync::{Mutex, atomic::AtomicUsize},
};

struct Dummy {
    replies: Mutex<VecDeque<Result<Vec<u8>, artifact::Error>>>,
    calls: AtomicUsize,
}
impl Dummy {
    fn valid() -> Self {
        Self {
            replies: Mutex::new(
                [
                    Ok(b"synthetic worker, never executed".to_vec()),
                    Ok(b"dummy".to_vec()),
                ]
                .into(),
            ),
            calls: AtomicUsize::new(0),
        }
    }
}
#[async_trait::async_trait]
impl Transfer for Dummy {
    async fn receive(
        &self,
        _: &ArtifactSource,
        sink: &mut (dyn for<'chunk> FnMut(&'chunk [u8]) -> Result<(), ()> + Send),
        cancelled: &mut (dyn FnMut() -> bool + Send),
    ) -> Result<(), artifact::Error> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if cancelled() {
            return Err(artifact::Error::Cancelled);
        }
        let bytes = self
            .replies
            .lock()
            .unwrap()
            .pop_front()
            .expect("unexpected extra request")?;
        for chunk in bytes.chunks(3) {
            sink(chunk).map_err(|_| artifact::Error::Sink)?;
        }
        Ok(())
    }
}
fn fixture(existing: bool) -> (tempfile::TempDir, Vec<u8>, PathBuf) {
    let (root, subject) = super::super::tests::fixture();
    let mut bytes: serde_json::Value =
        serde_json::from_slice(&super::super::tests::lock(&subject)).unwrap();
    bytes["profiles"][0]["delivery"] = serde_json::json!({
        "purpose":"synthetic decisions", "worker_source":"https://example.invalid/worker-source", "model_source":"https://example.invalid/model-source",
        "license":"synthetic-only", "license_source":"https://example.invalid/LICENSE", "notice_source":"https://example.invalid/NOTICE",
        "components":[{"name":"synthetic-worker","version":"1","license":"synthetic-only","source":"https://example.invalid/component"}],
        "downloads":{"worker":{"url":"https://example.invalid/worker","redirect_origins":[]},"model":{"url":"https://example.invalid/model","redirect_origins":["https://cdn.example.invalid"]}}
    });
    let home = if existing {
        root.path().canonicalize().unwrap()
    } else {
        let path = root.path().join("new-home");
        fs::create_dir(&path).unwrap();
        path
    };
    (root, serde_json::to_vec(&bytes).unwrap(), home)
}
fn planned(bytes: &[u8], home: &Path, repair: bool) -> DownloadPlan {
    make(bytes, "fixture-cpu-v1", home, &"c".repeat(32), repair).unwrap()
}
fn cancelled() -> Arc<AtomicBool> {
    Arc::new(AtomicBool::new(false))
}
fn generation(home: &Path) -> PathBuf {
    home.join(".polaris/decision-runtime/generations/fixture-v1")
}
fn empty_staging(home: &Path) {
    let path = home.join(".polaris/decision-runtime/downloads");
    if path.exists() {
        assert_eq!(fs::read_dir(path).unwrap().count(), 0);
    }
}

#[tokio::test]
async fn a_confirmed_fixed_download_verifies_both_hashes_and_installs_without_activation() {
    let (_root, bytes, home) = fixture(false);
    let plan = planned(&bytes, &home, false);
    let visible = serde_json::to_value(&plan).unwrap();
    assert_eq!(visible["transfer_bytes"], 37);
    assert_eq!(visible["transaction_timeout_seconds"], 7200);
    assert_eq!(visible["disk_required_bytes"], 74 + 256 * 1024 * 1024);
    assert_eq!(
        visible["delivery"]["downloads"]["model"]["url"],
        "https://example.invalid/model"
    );
    assert!(!home.join(".polaris").exists());
    let consent = plan.consent_sha256().unwrap();
    let transfer = Dummy::valid();
    execute_with(&bytes, plan, &consent, cancelled(), &transfer, || {
        Ok(home.clone())
    })
    .await
    .unwrap();
    assert_eq!(transfer.calls.load(Ordering::SeqCst), 2);
    assert_eq!(
        fs::read(generation(&home).join("model.gguf")).unwrap(),
        b"dummy"
    );
    assert_eq!(
        fs::metadata(generation(&home).join("worker"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o500
    );
    assert_eq!(
        fs::metadata(generation(&home).join("model.gguf"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o400
    );
    assert!(!home.join(".polaris/config.toml").exists());
    empty_staging(&home);
}

#[tokio::test]
async fn consent_package_home_and_operation_changes_are_refused_before_network_or_staging() {
    let (_root, bytes, home) = fixture(false);
    let consent = planned(&bytes, &home, false).consent_sha256().unwrap();
    let transfer = Dummy::valid();
    let other = make(&bytes, "fixture-cpu-v1", &home, &"b".repeat(32), false).unwrap();
    assert_eq!(
        execute_with(&bytes, other, &consent, cancelled(), &transfer, || panic!(
            "wrong consent read home"
        ))
        .await,
        Err(ManagedDecisionError::PlanChanged)
    );
    let mut changed: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    changed["profiles"][0]["delivery"]["downloads"]["model"]["url"] =
        "https://example.invalid/changed".into();
    assert_eq!(
        execute_with(
            &serde_json::to_vec(&changed).unwrap(),
            planned(&bytes, &home, false),
            &consent,
            cancelled(),
            &transfer,
            || Ok(home.clone())
        )
        .await,
        Err(ManagedDecisionError::PlanChanged)
    );
    assert_eq!(
        execute_with(
            &bytes,
            planned(&bytes, &home, false),
            &consent,
            cancelled(),
            &transfer,
            || Ok(home.join("other"))
        )
        .await,
        Err(ManagedDecisionError::PlanChanged)
    );
    assert_eq!(transfer.calls.load(Ordering::SeqCst), 0);
    assert!(!home.join(".polaris").exists());
}

#[tokio::test]
async fn corrupt_short_long_partial_or_cancelled_transfers_publish_nothing_and_clean_only_staging()
{
    for model in [
        Ok(b"wrong".to_vec()),
        Ok(b"no".to_vec()),
        Ok(b"toolong".to_vec()),
        Err(artifact::Error::Transport),
        Err(artifact::Error::Cancelled),
    ] {
        let (_root, bytes, home) = fixture(false);
        let transfer = Dummy::valid();
        transfer.replies.lock().unwrap()[1] = model;
        let plan = planned(&bytes, &home, false);
        let consent = plan.consent_sha256().unwrap();
        assert!(
            execute_with(&bytes, plan, &consent, cancelled(), &transfer, || Ok(
                home.clone()
            ))
            .await
            .is_err()
        );
        assert!(!generation(&home).exists());
        empty_staging(&home);
    }
    let (_root, bytes, home) = fixture(false);
    let flag = cancelled();
    flag.store(true, Ordering::Release);
    let plan = planned(&bytes, &home, false);
    let consent = plan.consent_sha256().unwrap();
    let transfer = Dummy::valid();
    assert_eq!(
        execute_with(&bytes, plan, &consent, flag, &transfer, || panic!(
            "cancelled read home"
        ))
        .await,
        Err(ManagedDecisionError::Cancelled)
    );
    assert_eq!(transfer.calls.load(Ordering::SeqCst), 0);
    assert!(!home.join(".polaris").exists());
}

#[tokio::test]
async fn conflict_preserves_existing_files_and_explicit_repair_keeps_the_entire_old_generation() {
    let (_root, bytes, home) = fixture(true);
    fs::write(generation(&home).join("extra"), b"keep original").unwrap();
    let transfer = Dummy::valid();
    let plan = planned(&bytes, &home, false);
    let consent = plan.consent_sha256().unwrap();
    assert_eq!(
        execute_with(&bytes, plan, &consent, cancelled(), &transfer, || Ok(
            home.clone()
        ))
        .await,
        Err(ManagedDecisionError::Conflict)
    );
    assert_eq!(transfer.calls.load(Ordering::SeqCst), 0);
    let plan = planned(&bytes, &home, true);
    let consent = plan.consent_sha256().unwrap();
    let backup = plan.recovery_backup().unwrap().to_owned();
    execute_with(&bytes, plan, &consent, cancelled(), &transfer, || {
        Ok(home.clone())
    })
    .await
    .unwrap();
    assert_eq!(fs::read(backup.join("extra")).unwrap(), b"keep original");
    assert_eq!(fs::read(backup.join("model.gguf")).unwrap(), b"dummy");
    assert_eq!(
        fs::read(generation(&home).join("model.gguf")).unwrap(),
        b"dummy"
    );
    empty_staging(&home);
}

#[tokio::test]
async fn dropping_an_in_progress_download_cancels_its_owner_and_removes_private_staging() {
    struct Pending;
    #[async_trait::async_trait]
    impl Transfer for Pending {
        async fn receive(
            &self,
            _: &ArtifactSource,
            _: &mut (dyn for<'chunk> FnMut(&'chunk [u8]) -> Result<(), ()> + Send),
            _: &mut (dyn FnMut() -> bool + Send),
        ) -> Result<(), artifact::Error> {
            std::future::pending().await
        }
    }
    let (_root, bytes, home) = fixture(false);
    let plan = planned(&bytes, &home, false);
    let consent = plan.consent_sha256().unwrap();
    let flag = cancelled();
    let result = tokio::time::timeout(
        Duration::from_millis(20),
        execute_with(&bytes, plan, &consent, flag.clone(), &Pending, || {
            Ok(home.clone())
        }),
    )
    .await;
    assert!(result.is_err());
    assert!(flag.load(Ordering::Acquire));
    assert!(!generation(&home).exists());
    empty_staging(&home);
}

#[test]
fn missing_or_credential_bearing_metadata_and_unaccepted_profiles_never_produce_a_download_plan() {
    let (_root, bytes, home) = fixture(false);
    for url in [
        "http://example.invalid/file",
        "https://dummy@example.invalid/file",
        "https://example.invalid/file?dummy=secret",
        "https://example.invalid/file#fragment",
    ] {
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["profiles"][0]["delivery"]["downloads"]["model"]["url"] = url.into();
        assert!(matches!(
            make(
                &serde_json::to_vec(&value).unwrap(),
                "fixture-cpu-v1",
                &home,
                &"a".repeat(32),
                false
            ),
            Err(ManagedDecisionError::MissingDownloadMetadata)
        ));
    }
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    value["profiles"][0]["delivery"]["downloads"] = serde_json::Value::Null;
    assert!(matches!(
        make(
            &serde_json::to_vec(&value).unwrap(),
            "fixture-cpu-v1",
            &home,
            &"a".repeat(32),
            false
        ),
        Err(ManagedDecisionError::MissingDownloadMetadata)
    ));
    assert!(matches!(
        prepare("fixture-cpu-v1", false),
        Err(ManagedDecisionError::UnverifiedProfile)
    ));
    assert!(!home.join(".polaris").exists());
}

#[tokio::test]
async fn aliased_staging_and_insufficient_space_do_not_start_transfers() {
    let (_root, bytes, home) = fixture(true);
    let outside = home.join("outside");
    fs::create_dir(&outside).unwrap();
    std::os::unix::fs::symlink(&outside, home.join(".polaris/decision-runtime/downloads")).unwrap();
    let transfer = Dummy::valid();
    let plan = planned(&bytes, &home, false);
    let consent = plan.consent_sha256().unwrap();
    assert_eq!(
        execute_with(&bytes, plan, &consent, cancelled(), &transfer, || Ok(
            home.clone()
        ))
        .await,
        Err(ManagedDecisionError::Integrity)
    );
    assert_eq!(transfer.calls.load(Ordering::SeqCst), 0);
    assert_eq!(fs::read_dir(outside).unwrap().count(), 0);
    let (_root, _bytes, home) = fixture(false);
    let path = home
        .join(".polaris/decision-runtime/downloads")
        .join(format!(".pending-{}", "a".repeat(32)));
    assert!(matches!(
        staging::Staging::new(&home, &path, u64::MAX),
        Err(ManagedDecisionError::InsufficientSpace)
    ));
    empty_staging(&home);
}

#[tokio::test]
async fn the_whole_transaction_deadline_cancels_the_second_artifact_and_waits_for_cleanup() {
    struct SlowSecond(AtomicUsize);
    #[async_trait::async_trait]
    impl Transfer for SlowSecond {
        async fn receive(
            &self,
            _: &ArtifactSource,
            sink: &mut (dyn for<'chunk> FnMut(&'chunk [u8]) -> Result<(), ()> + Send),
            cancelled: &mut (dyn FnMut() -> bool + Send),
        ) -> Result<(), artifact::Error> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                return sink(b"synthetic worker, never executed")
                    .map_err(|_| artifact::Error::Sink);
            }
            while !cancelled() {
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
            Err(artifact::Error::Cancelled)
        }
    }
    let (_root, bytes, home) = fixture(false);
    let plan = planned(&bytes, &home, false);
    let consent = plan.consent_sha256().unwrap();
    let transfer = SlowSecond(AtomicUsize::new(0));
    let flag = cancelled();
    assert_eq!(
        execute_with_deadline(
            &bytes,
            plan,
            &consent,
            flag.clone(),
            &transfer,
            || Ok(home.clone()),
            Duration::from_millis(50)
        )
        .await,
        Err(ManagedDecisionError::DownloadTimeout)
    );
    assert_eq!(transfer.0.load(Ordering::SeqCst), 2);
    assert!(flag.load(Ordering::Acquire));
    assert!(!generation(&home).exists());
    empty_staging(&home);
}
