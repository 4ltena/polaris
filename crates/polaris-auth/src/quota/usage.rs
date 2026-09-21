//! M5-3a: the bound usage request and its normalization into M5-1 input.
//!
//! The endpoint and the model-to-bucket coverage come from a host table that
//! ships unverified: until a real account confirms both the HTTP contract and
//! the coverage, an observation can only be `mapping_unknown`, which keeps it
//! out of automatic selection. Values the upstream normalizer drops
//! (`allowed`, `limit_reached`, the reset seconds) are kept here.

use super::{
    BucketState, ExclusionReason, KnownQuota, ObservationState, RawQuotaWindow, RawWindowKey,
    ReachedState, ServerAvailability, normalize_known_quota,
};
use crate::AuthError;

#[cfg(not(test))]
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
/// Tests exercise the same deadline path without waiting 30 s.
#[cfg(test)]
const TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
/// The host the ChatGPT-shaped endpoint lives on.
pub const USAGE_BASE: &str = "https://chatgpt.com";

/// The two shapes seen in the upstream client. One registration uses exactly
/// the one its profile names; a failure never falls back to the other.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UsageEndpoint {
    ChatGpt,
    Codex,
}

impl UsageEndpoint {
    fn path(self) -> &'static str {
        match self {
            Self::ChatGpt => "/backend-api/wham/usage",
            Self::Codex => "/api/codex/usage",
        }
    }
}

/// Host-side coverage for one model set: which buckets and window slots must
/// be present for an observation to count as complete.
#[derive(Debug)]
pub struct UsageProfile {
    pub id: &'static str,
    pub endpoint: UsageEndpoint,
    models: &'static [&'static str],
    bucket_key: &'static str,
    window_slot_keys: &'static [&'static str],
    /// Set only by a recorded real-account measurement of this exact profile.
    pub verified: bool,
}

pub const PROFILES: &[UsageProfile] = &[UsageProfile {
    id: "chatgpt-primary-secondary",
    endpoint: UsageEndpoint::ChatGpt,
    models: &["gpt-6-astra", "gpt-5.6-luna"],
    bucket_key: "chatgpt",
    window_slot_keys: &["primary", "secondary"],
    verified: false,
}];

/// The HTTP path is exercised only through this profile until a real account
/// verifies a shipped one.
#[cfg(test)]
pub(crate) static VERIFIED_FOR_TESTS: UsageProfile = UsageProfile {
    id: "test-verified",
    endpoint: UsageEndpoint::ChatGpt,
    models: &["test-verified-model"],
    bucket_key: "chatgpt",
    window_slot_keys: &["primary", "secondary"],
    verified: true,
};

pub fn profile_for(model_id: &str) -> Option<&'static UsageProfile> {
    PROFILES
        .iter()
        .find(|profile| profile.models.contains(&model_id))
}

/// One upstream window, kept with every field the response carried.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct RawUsageWindow {
    pub usage_percent: Option<f64>,
    pub window_minutes: Option<i128>,
    pub window_seconds: Option<i128>,
    pub resets_in_seconds: Option<i128>,
    pub resets_at_unix_seconds: Option<i128>,
}

/// The response fields M5-1 needs; nothing else is retained.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct UsageResponse {
    pub allowed: Option<bool>,
    pub limit_reached: Option<bool>,
    pub primary: Option<RawUsageWindow>,
    pub secondary: Option<RawUsageWindow>,
}

fn integer(value: &serde_json::Value) -> Option<i128> {
    value
        .as_i64()
        .map(i128::from)
        .or_else(|| value.as_u64().map(i128::from))
        .or_else(|| {
            // Legacy responses may encode an integer as 300.0 or 3e2. Keep
            // those values without truncating fractions or rounding large IDs.
            value
                .as_f64()
                .filter(|number| {
                    number.is_finite()
                        && number.fract() == 0.0
                        && number.abs() <= 9_007_199_254_740_991.0
                })
                .map(|number| number as i128)
        })
}

fn window(value: &serde_json::Value, current: bool) -> Option<RawUsageWindow> {
    let object = value.as_object()?;
    let number = |key: &str| object.get(key).and_then(serde_json::Value::as_f64);
    let integer = |key: &str| object.get(key).and_then(integer);
    Some(RawUsageWindow {
        usage_percent: number("used_percent").or_else(|| number("usage_percent")),
        window_minutes: (!current).then(|| integer("window_minutes")).flatten(),
        window_seconds: current.then(|| integer("limit_window_seconds")).flatten(),
        resets_in_seconds: integer(if current {
            "reset_after_seconds"
        } else {
            "resets_in_seconds"
        }),
        resets_at_unix_seconds: integer(if current { "reset_at" } else { "resets_at" }),
    })
}

fn decode(value: &serde_json::Value) -> UsageResponse {
    // Select one complete shape. A present but incomplete current response must
    // not be patched with legacy or unrelated bucket data.
    let current = value.get("rate_limit").is_some();
    let details = value
        .get("rate_limit")
        .or_else(|| value.get("rate_limit_status_details"))
        .unwrap_or(value);
    UsageResponse {
        allowed: details.get("allowed").and_then(serde_json::Value::as_bool),
        limit_reached: details
            .get("limit_reached")
            .and_then(serde_json::Value::as_bool),
        primary: details
            .get(if current { "primary_window" } else { "primary" })
            .and_then(|value| window(value, current)),
        secondary: details
            .get(if current {
                "secondary_window"
            } else {
                "secondary"
            })
            .and_then(|value| window(value, current)),
    }
}

/// One GET with the same bounds as the token POST: 30 s, 1 MiB, no redirects
/// and no retry. Errors carry a fixed category, never a response body.
pub(crate) async fn fetch(
    base: &str,
    endpoint: UsageEndpoint,
    access_token: &str,
    account_id: &str,
) -> Result<UsageResponse, AuthError> {
    let client = polaris_http::client_builder()
        .map_err(|_| AuthError::Http("client_configuration".into()))?
        .redirect(reqwest::redirect::Policy::none())
        .timeout(TIMEOUT)
        .build()
        .map_err(|_| AuthError::Http("client_configuration".into()))?;
    let transport = |error: reqwest::Error| {
        AuthError::Http(
            if error.is_timeout() {
                "timeout"
            } else {
                "transport"
            }
            .into(),
        )
    };
    let mut response = client
        .get(format!("{base}{}", endpoint.path()))
        .header("authorization", format!("Bearer {access_token}"))
        .header("chatgpt-account-id", account_id)
        .send()
        .await
        .map_err(transport)?;
    let status = response.status();
    if !status.is_success() {
        return Err(AuthError::Http(format!("status {}", status.as_u16())));
    }
    let mut body = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(transport)? {
        if body.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(AuthError::Decode("response_too_large".into()));
        }
        body.extend_from_slice(&chunk);
    }
    let value: serde_json::Value =
        serde_json::from_slice(&body).map_err(|_| AuthError::Decode("invalid_json".into()))?;
    Ok(decode(&value))
}

/// The observation for one response. An unverified profile never produces a
/// known observation, so it cannot be selected automatically.
pub fn observe(
    profile: &UsageProfile,
    response: &UsageResponse,
    captured_unix_seconds: u64,
) -> Result<ObservationState, ExclusionReason> {
    if !profile.verified {
        return Ok(ObservationState::MappingUnknown);
    }
    let expected: Vec<RawWindowKey> = profile
        .window_slot_keys
        .iter()
        .map(|slot| RawWindowKey {
            bucket_key: Some(profile.bucket_key.to_owned()),
            window_slot_key: Some((*slot).to_owned()),
        })
        .collect();
    let mut windows = Vec::new();
    for slot in profile.window_slot_keys {
        let raw = match *slot {
            "primary" => response.primary.clone(),
            "secondary" => response.secondary.clone(),
            _ => None,
        };
        // A missing required window stays missing; it is never filled in.
        let raw = raw.unwrap_or_default();
        windows.push(RawQuotaWindow {
            key: RawWindowKey {
                bucket_key: Some(profile.bucket_key.to_owned()),
                window_slot_key: Some((*slot).to_owned()),
            },
            usage_percent: raw.usage_percent,
            window_millis: raw
                .window_seconds
                .map(|seconds| seconds.checked_mul(1_000))
                .unwrap_or_else(|| {
                    raw.window_minutes
                        .and_then(|minutes| minutes.checked_mul(60_000))
                }),
            reset_unix_seconds: reset(&raw, captured_unix_seconds),
        });
    }
    let known: KnownQuota = normalize_known_quota(
        Some(profile.id.to_owned()),
        expected,
        windows,
        vec![BucketState {
            bucket_key: profile.bucket_key.to_owned(),
            availability: match response.allowed {
                Some(true) => ServerAvailability::Allowed,
                Some(false) => ServerAvailability::Denied,
                None => ServerAvailability::Unknown,
            },
            reached: match response.limit_reached {
                Some(true) => ReachedState::Reached,
                Some(false) => ReachedState::NotReached,
                None => ReachedState::Unknown,
            },
        }],
    )?;
    Ok(ObservationState::Known(known))
}

/// The absolute reset, preferring the upstream absolute value and otherwise
/// adding the remaining seconds to the capture time.
fn reset(raw: &RawUsageWindow, captured_unix_seconds: u64) -> Option<i128> {
    raw.resets_at_unix_seconds.or_else(|| {
        raw.resets_in_seconds
            .and_then(|seconds| i128::from(captured_unix_seconds).checked_add(seconds))
    })
}

#[cfg(test)]
mod tests;
