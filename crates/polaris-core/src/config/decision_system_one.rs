//! User-owned System One endpoint/model/environment reference; no secret value.
use super::ConfigError;
use polaris_provider::decision::system_one::{SystemOneLimits, SystemOneTarget};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SystemOneConfig {
    pub endpoint: String,
    pub model: String,
    /// An explicit environment reference, never the credential value itself.
    pub credential_env: String,
    #[serde(default)]
    pub allow_loopback_http: bool,
}
impl SystemOneConfig {
    pub(crate) fn validate(&self) -> Result<(), ConfigError> {
        let reference = self.credential_env.as_bytes();
        if !self.credential_env.starts_with("POLARIS_SYSTEM_ONE_")
            || reference.len() <= "POLARIS_SYSTEM_ONE_".len()
            || reference.len() > 128
            || !reference
                .iter()
                .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || *byte == b'_')
        {
            return Err(ConfigError::InvalidSystemOne);
        }
        self.target()
            .map(|_| ())
            .map_err(|_| ConfigError::InvalidSystemOne)
    }
    pub(crate) fn target(
        &self,
    ) -> Result<SystemOneTarget, polaris_provider::decision::DecisionError> {
        SystemOneTarget::new(&self.endpoint, &self.model, self.allow_loopback_http)
    }
    /// Fixed transport ceilings; these do not attest model context or capacity.
    pub(crate) fn limits(&self) -> SystemOneLimits {
        SystemOneLimits {
            request_bytes: 1024 * 1024,
            response_bytes: 1024 * 1024,
            questions: 256,
            options: 256,
            timeout_ms: 30_000,
        }
    }
    pub fn origin(&self) -> Option<String> {
        self.target().ok()?;
        url::Url::parse(&self.endpoint)
            .ok()
            .map(|url| url.origin().ascii_serialization())
    }
}

#[cfg(test)]
mod tests;
