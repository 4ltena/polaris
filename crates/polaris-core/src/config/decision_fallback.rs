//! User-owned typed-routing failover configuration; project files cannot add destinations.
use super::{ConfigError, DecisionBackend, DecisionConfig, RawDecision};
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionFallback {
    pub targets: Vec<DecisionConfig>,
    pub max_attempts: usize,
    pub attempt_timeout_ms: u64,
    pub total_timeout_ms: u64,
}

impl DecisionFallback {
    pub(crate) fn validate(&self, primary: &DecisionConfig) -> Result<(), ConfigError> {
        if self
            .targets
            .iter()
            .any(|target| !target.enabled || target.fallback.is_some())
        {
            return Err(ConfigError::InvalidDecisionFallback);
        }
        for target in std::iter::once(primary).chain(&self.targets) {
            if target.model.as_ref().is_some_and(|model| model.len() > 256) {
                return Err(ConfigError::InvalidDecisionFallback);
            }
            RawDecision::settled_target(target)?;
            match target.backend {
                DecisionBackend::Jev => {
                    endpoint(target)?;
                }
                DecisionBackend::SemIf if target.base_url.is_some() || target.model.is_some() => {
                    return Err(ConfigError::InvalidDecisionFallback);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Destinations and data scope before invocation; no key, URL path or helper path.
    pub fn disclosure(&self, primary: &DecisionConfig) -> Vec<String> {
        let mut lines = vec![
            format!(
                "typed fallback: routing / at most {} attempts / {} ms each / {} ms total",
                self.max_attempts, self.attempt_timeout_ms, self.total_timeout_ms
            ),
            "typed fallback: state and questions are sent to each endpoint (may include requests, conversation or file content, and candidate descriptions)"
                .into(),
        ];
        for (index, target) in std::iter::once(primary).chain(&self.targets).enumerate() {
            let destination = if target.backend == DecisionBackend::Jev {
                endpoint(target)
                    .map(|url| url.origin().ascii_serialization())
                    .unwrap_or_else(|_| "invalid endpoint".into())
            } else if target.backend == DecisionBackend::SystemOne {
                target
                    .system_one
                    .as_ref()
                    .and_then(|profile| profile.origin())
                    .unwrap_or_else(|| "invalid endpoint".into())
            } else {
                "local process".into()
            };
            let model = serde_json::to_string(
                &target
                    .system_one
                    .as_ref()
                    .filter(|_| target.backend == DecisionBackend::SystemOne)
                    .map(|profile| profile.model.as_str())
                    .or(target.model.as_deref())
                    .or((target.backend == DecisionBackend::Jev).then_some("jev-latest")),
            )
            .unwrap_or_else(|_| "null".into());
            lines.push(format!(
                "typed fallback {}: {} / {} / model={}",
                index + 1,
                target.backend.as_str(),
                destination,
                model
            ));
        }
        lines
    }
}

fn endpoint(target: &DecisionConfig) -> Result<url::Url, ConfigError> {
    let url = url::Url::parse(
        target
            .base_url
            .as_deref()
            .unwrap_or("https://api.typesafe.ai/v1"),
    )
    .map_err(|_| ConfigError::InvalidDecisionFallback)?;
    let local = url
        .host_str()
        .and_then(|host| {
            host.trim_matches(['[', ']'])
                .parse::<std::net::IpAddr>()
                .ok()
        })
        .is_some_and(|ip| ip.is_loopback());
    if !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
        || url.host().is_none()
        || !(url.scheme() == "https" || (url.scheme() == "http" && local))
    {
        return Err(ConfigError::InvalidDecisionFallback);
    }
    Ok(url)
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RawFallback {
    purposes: Vec<String>,
    targets: Vec<RawTarget>,
    max_attempts: usize,
    attempt_timeout_ms: u64,
    total_timeout_ms: u64,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawTarget {
    backend: String,
    base_url: Option<String>,
    model: Option<String>,
    helper: Option<String>,
    system_one: Option<super::SystemOneConfig>,
}

impl RawFallback {
    pub(super) fn resolve(self) -> Result<DecisionFallback, ConfigError> {
        if self.purposes != ["routing"]
            || self.targets.is_empty()
            || self.targets.len() > 3
            || self.max_attempts == 0
            || self.max_attempts > self.targets.len() + 1
            || self.attempt_timeout_ms == 0
            || self.attempt_timeout_ms > self.total_timeout_ms
            || self.total_timeout_ms > 300_000
        {
            return Err(ConfigError::InvalidDecisionFallback);
        }
        let targets = self
            .targets
            .into_iter()
            .map(|target| {
                let raw = RawDecision {
                    backend: Some(target.backend),
                    base_url: target.base_url,
                    model: target.model,
                    helper: target.helper,
                    system_one: target.system_one,
                    ..Default::default()
                };
                let mut config = DecisionConfig {
                    enabled: true,
                    ..Default::default()
                };
                raw.apply_to(&mut config)?;
                RawDecision::settled(&config)?;
                if config.model.as_ref().is_some_and(|model| model.len() > 256) {
                    return Err(ConfigError::InvalidDecisionFallback);
                }
                Ok(config)
            })
            .collect::<Result<Vec<_>, ConfigError>>()?;
        Ok(DecisionFallback {
            targets,
            max_attempts: self.max_attempts,
            attempt_timeout_ms: self.attempt_timeout_ms,
            total_timeout_ms: self.total_timeout_ms,
        })
    }
}

#[cfg(test)]
mod tests;
