//! Package-approved local decision generations. Project configuration selects
//! an ID only; it cannot supply a worker path, digest or acceptance record.
use polaris_provider::decision::Decider;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryMetadata {
    pub purpose: String,
    pub worker_source: String,
    pub model_source: String,
    pub license: String,
    pub license_source: String,
    pub notice_source: String,
    pub components: Vec<DeliveryComponent>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub downloads: Option<DownloadMetadata>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadMetadata {
    pub worker: DownloadSource,
    pub model: DownloadSource,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DownloadSource {
    pub url: String,
    pub redirect_origins: Vec<String>,
}

/// A one-use download transaction, bound to the package and private staging.
#[derive(Serialize)]
pub struct DownloadPlan {
    #[serde(flatten)]
    import: ImportPlan,
    transfer_bytes: u64,
    disk_required_bytes: u64,
    transaction_timeout_seconds: u64,
}

impl DownloadPlan {
    pub fn recovery_backup(&self) -> Option<&std::path::Path> {
        self.import.recovery_backup()
    }

    pub fn consent_sha256(&self) -> Result<String, ManagedDecisionError> {
        use sha2::{Digest, Sha256};
        let bytes = serde_json::to_vec(self).map_err(|_| ManagedDecisionError::Integrity)?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }
}

/// Read-only: does not create staging or contact a distribution service.
pub fn plan_download(profile: &str, repair: bool) -> Result<DownloadPlan, ManagedDecisionError> {
    #[cfg(target_os = "macos")]
    {
        native::download::prepare(profile, repair)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (profile, repair);
        Err(ManagedDecisionError::UnsupportedPlatform)
    }
}

/// Explicit download only after the caller confirms the fresh plan. Does not
/// activate a backend or start inference. Cancellation cleans only owned staging.
pub async fn download_local(
    plan: DownloadPlan,
    consent: &str,
    cancelled: Arc<std::sync::atomic::AtomicBool>,
) -> Result<(), ManagedDecisionError> {
    #[cfg(target_os = "macos")]
    {
        native::download::execute(plan, consent, cancelled).await
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (plan, consent, cancelled);
        Err(ManagedDecisionError::UnsupportedPlatform)
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeliveryComponent {
    pub name: String,
    pub version: String,
    pub license: String,
    pub source: String,
}

/// Read-only, point-in-time digest verification; never an activation receipt.
#[derive(Debug, Serialize)]
pub struct VerifiedGeneration {
    pub profile: String,
    pub generation: String,
    pub model_repository: String,
    pub model_revision: String,
    pub quantization: String,
    pub runtime_revision: String,
    pub worker_sha256: String,
    pub model_sha256: String,
    pub verified_bytes: u64,
}

/// Check an existing fixed generation without copying or starting a worker.
pub fn verify_installed(
    profile: &str,
    cancelled: impl FnMut() -> bool,
) -> Result<VerifiedGeneration, ManagedDecisionError> {
    #[cfg(target_os = "macos")]
    {
        native::verification::verify(profile, cancelled)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (profile, cancelled);
        Err(ManagedDecisionError::UnsupportedPlatform)
    }
}

/// A fresh, non-deserializable transaction. Only plan_local can construct it;
/// install_local consumes it so consent is not a reusable persisted receipt.
#[derive(Serialize)]
pub struct ImportPlan {
    operation: ImportOperation,
    replacement: Option<ReplacementPlan>,
    transaction_id: String,
    app_version: String,
    manifest_sha256: String,
    subject_sha256: String,
    profile: String,
    generation: String,
    platform: String,
    architecture: String,
    model_repository: String,
    model_revision: String,
    quantization: String,
    runtime_revision: String,
    device: String,
    context_tokens: u32,
    peak_bytes: u64,
    reserve_bytes: u64,
    worker: ImportFile,
    model: ImportFile,
    destination: std::path::PathBuf,
    installed_bytes: u64,
    copy_reserve_bytes: u64,
    delivery: DeliveryMetadata,
}

#[derive(Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum ImportOperation {
    Import,
    Repair,
}

#[derive(Serialize)]
struct ReplacementPlan {
    backup_destination: std::path::PathBuf,
    identity: GenerationIdentity,
}

#[derive(PartialEq, Eq, Serialize)]
struct GenerationIdentity {
    device: u64,
    inode: u64,
    modified_seconds: i64,
    modified_nanos: i64,
    changed_seconds: i64,
    changed_nanos: i64,
}

#[derive(Serialize)]
struct ImportFile {
    source: std::path::PathBuf,
    bytes: u64,
    sha256: String,
}

impl ImportPlan {
    pub fn recovery_backup(&self) -> Option<&std::path::Path> {
        self.replacement
            .as_ref()
            .map(|p| p.backup_destination.as_path())
    }
    pub fn consent_sha256(&self) -> Result<String, ManagedDecisionError> {
        use sha2::{Digest, Sha256};
        let bytes = serde_json::to_vec(self).map_err(|_| ManagedDecisionError::Integrity)?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum ManagedDecisionError {
    #[error("Clef profileはpackageの受入登録が未完了です")]
    UnverifiedProfile,
    #[error("Clef generationが未導入です")]
    NotInstalled,
    #[error("Clef generationまたは固定lockが一致しません")]
    Integrity,
    #[error("Clef native runtimeはこのplatformで利用できません")]
    UnsupportedPlatform,
    #[error("Clefのこのdeviceの容量観測は未対応です")]
    UnsupportedDevice,
    #[error("Clef generationの導入先が既に存在します。既存資源は変更しません")]
    Conflict,
    #[error("Clef generationの導入を取り消しました")]
    Cancelled,
    #[error("Clef generationの導入を完了できませんでした")]
    Install,
    #[error("Clef generationのcopyと予備領域に必要な空き容量がありません")]
    InsufficientSpace,
    #[error("Clef profileの取得元・license・NOTICE・component情報が未登録です")]
    MissingDeliveryMetadata,
    #[error("Clef profileの固定download情報が未登録または不正です")]
    MissingDownloadMetadata,
    #[error("Clef artifactを取得できませんでした。固定generationは変更していません")]
    Download,
    #[error("Clef取得・導入の期限に達しました。固定generationの状態を再検証してください")]
    DownloadTimeout,
    #[error("Clef導入計画と確認済み内容が一致しません。計画を作り直してください")]
    PlanChanged,
}

/// Read-only preparation; does not download, create directories or infer.
pub fn plan_local(
    profile: &str,
    worker: &std::path::Path,
    model: &std::path::Path,
) -> Result<ImportPlan, ManagedDecisionError> {
    #[cfg(target_os = "macos")]
    {
        native::plan::prepare(profile, worker, model)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (profile, worker, model);
        Err(ManagedDecisionError::UnsupportedPlatform)
    }
}

/// Plan an explicit repair; the previous generation is preserved, not removed.
pub fn plan_repair_local(
    profile: &str,
    worker: &std::path::Path,
    model: &std::path::Path,
) -> Result<ImportPlan, ManagedDecisionError> {
    #[cfg(target_os = "macos")]
    {
        native::plan::prepare_repair(profile, worker, model)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (profile, worker, model);
        Err(ManagedDecisionError::UnsupportedPlatform)
    }
}

/// Registration is not installation, capacity admission or model readiness.
pub fn registered_profiles() -> Result<Vec<String>, ManagedDecisionError> {
    #[cfg(target_os = "macos")]
    {
        native::plan::registered_profiles()
    }
    #[cfg(not(target_os = "macos"))]
    {
        Ok(Vec::new())
    }
}

/// Explicit management operation only. Copies package-approved bytes into a
/// new generation; never changes the selected backend or starts inference.
/// Callers must obtain consent for the selected artifacts before calling.
pub fn install_local(
    plan: ImportPlan,
    consent_sha256: &str,
    cancelled: impl FnMut() -> bool,
) -> Result<(), ManagedDecisionError> {
    #[cfg(target_os = "macos")]
    {
        native::plan::execute(plan, consent_sha256, cancelled)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (plan, consent_sha256, cancelled);
        Err(ManagedDecisionError::UnsupportedPlatform)
    }
}

pub(crate) fn valid_profile_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 96
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"-_.".contains(&c))
}

pub fn build(profile: &str) -> Result<Arc<dyn Decider>, ManagedDecisionError> {
    if !valid_profile_id(profile) {
        return Err(ManagedDecisionError::Integrity);
    }
    #[cfg(target_os = "macos")]
    {
        native::build(profile).map(|decider| Arc::new(decider) as Arc<dyn Decider>)
    }
    #[cfg(not(target_os = "macos"))]
    Err(ManagedDecisionError::UnsupportedPlatform)
}

#[cfg(target_os = "macos")]
mod native;
