//! Package-approved local decision generations. Project configuration selects
//! an ID only; it cannot supply a worker path, digest or acceptance record.
use polaris_provider::decision::Decider;
use std::sync::Arc;

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
}

/// Explicit management operation only. Copies package-approved bytes into a
/// new generation; never changes the selected backend or starts inference.
/// Callers must obtain consent for the selected artifacts before calling.
pub fn install_local(
    profile: &str,
    worker: &std::path::Path,
    model: &std::path::Path,
    cancelled: impl FnMut() -> bool,
) -> Result<(), ManagedDecisionError> {
    #[cfg(target_os = "macos")]
    {
        native::install::install(profile, worker, model, cancelled)
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (profile, worker, model, cancelled);
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
