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
