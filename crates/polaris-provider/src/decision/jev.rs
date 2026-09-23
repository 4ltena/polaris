//! J-4: the HTTP client for a System One decider.
//!
//! One request carries the state once and every question with it, so a run
//! that asks about five hundred things still makes a single round trip.
//! Nothing here generates or accepts free text: the reply is parsed into
//! the typed answers and checked against the questions that were asked
//! before any of it is returned.
use super::{Decider, Decision, DecisionError, Question, checked};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

mod metrics;

/// The published figures are 70–500ms. A request that has not answered well
/// past that is not going to, and the caller has a safe default to fall
/// back on, so waiting longer costs more than giving up.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Retries for the two codes that mean "later, not never". The delays are
/// short because the caller is holding a run open.
const BACKOFF: &[Duration] = &[
    Duration::from_millis(250),
    Duration::from_millis(1000),
    Duration::from_millis(2000),
];

pub struct JevDecider {
    endpoint: String,
    api_key: String,
    model: String,
    client: reqwest::Client,
    metrics_path: Option<PathBuf>,
    max_attempts: usize,
}

#[derive(serde::Serialize)]
struct Request<'a> {
    state: &'a serde_json::Value,
    model: &'a str,
    questions: &'a BTreeMap<String, Question>,
}

#[derive(serde::Deserialize)]
struct Reply {
    #[serde(default)]
    answers: BTreeMap<String, super::Answer>,
    #[serde(default)]
    usage: Option<Usage>,
}

#[derive(serde::Deserialize)]
struct Usage {
    #[serde(default)]
    input_tokens: Option<u64>,
    #[serde(default)]
    output_tokens: Option<u64>,
}

impl JevDecider {
    /// `base_url` is the versioned root, e.g. `https://api.typesafe.ai/v1`.
    pub fn new(base_url: &str, api_key: String, model: String) -> Result<Self, DecisionError> {
        if api_key.trim().is_empty() {
            return Err(DecisionError::Auth);
        }
        let client = polaris_http::client_builder()
            .map_err(|_| DecisionError::Unavailable)?
            // A decider answers questions; it never follows a redirect to
            // somewhere that was not configured.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| DecisionError::Unavailable)?;
        let (metrics_path, max_attempts) = benchmark_options(
            std::env::var_os("POLARIS_BENCHMARK_REPORT"),
            std::env::var_os("POLARIS_JEV_METRICS_PATH"),
            std::env::var_os("POLARIS_JEV_MAX_ATTEMPTS"),
        )?;
        Ok(Self {
            endpoint: format!("{}/systemone", base_url.trim_end_matches('/')),
            api_key,
            model,
            client,
            metrics_path,
            max_attempts,
        })
    }

    /// A finalization receipt represents a single, explicitly disclosed
    /// observation. It must never turn one consent into retry traffic.
    /// Ordinary Jev callers retain the regular bounded retry policy.
    pub fn new_finalizer(
        base_url: &str,
        api_key: String,
        model: String,
    ) -> Result<Self, DecisionError> {
        let mut decider = Self::new(base_url, api_key, model)?;
        decider.max_attempts = 1;
        Ok(decider)
    }

    /// Reads the endpoint and key from the environment. The key never
    /// reaches a log, an error message or a record: only whether it was
    /// there at all.
    pub fn from_environment() -> Result<Self, DecisionError> {
        let key = std::env::var("POLARIS_JEV_API_KEY").map_err(|_| DecisionError::Auth)?;
        let base = std::env::var("POLARIS_JEV_BASE_URL")
            .unwrap_or_else(|_| "https://api.typesafe.ai/v1".to_owned());
        let model = std::env::var("POLARIS_JEV_MODEL").unwrap_or_else(|_| "jev-latest".to_owned());
        Self::new(&base, key, model)
    }

    async fn once(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
        observation: &mut Option<metrics::Attempt>,
    ) -> Result<Reply, DecisionError> {
        let response = self
            .client
            .post(&self.endpoint)
            .bearer_auth(&self.api_key)
            .json(&Request {
                state,
                model: &self.model,
                questions,
            })
            .send()
            .await
            // A timeout, a refused connection and a TLS failure are all
            // "no answer now"; the caller's safe default is the same for
            // each, and the distinction is not worth a wider error type.
            .map_err(|_| DecisionError::Unavailable)?;
        let status = response.status();
        if let Some(metric) = observation.as_mut() {
            metric.status(status.as_u16());
        }
        if !status.is_success() {
            // The body can echo what was sent, so it is never carried into
            // an error a caller might record.
            return Err(match status.as_u16() {
                401 | 403 => DecisionError::Auth,
                413 => DecisionError::TooLarge,
                // 422 is this service's validation failure. It names the
                // field, but the body can echo the state, so the reason is
                // not carried into an error a caller might record.
                400 | 422 => DecisionError::Malformed,
                _ => DecisionError::Unavailable,
            });
        }
        let body = response
            .json::<serde_json::Value>()
            .await
            .map_err(|_| DecisionError::InvalidAnswer)?;
        if let Some(metric) = observation.as_mut() {
            metric.usage(
                body["usage"]["input_tokens"].as_u64(),
                body["usage"]["output_tokens"].as_u64(),
            );
        }
        serde_json::from_value(body).map_err(|_| DecisionError::InvalidAnswer)
    }

    fn retryable(error: &DecisionError) -> bool {
        // Auth, too large and a malformed answer will not change by asking
        // again. Only "the service could not answer now" is worth a wait.
        matches!(error, DecisionError::Unavailable)
    }
}

fn benchmark_options(
    report: Option<std::ffi::OsString>,
    path: Option<std::ffi::OsString>,
    attempts: Option<std::ffi::OsString>,
) -> Result<(Option<PathBuf>, usize), DecisionError> {
    if path.is_none() && attempts.is_none() {
        return Ok((None, BACKOFF.len() + 1));
    }
    if report.as_deref() != Some(std::ffi::OsStr::new("1"))
        || attempts.as_deref() != Some(std::ffi::OsStr::new("1"))
    {
        return Err(DecisionError::Malformed);
    }
    let path = PathBuf::from(path.ok_or(DecisionError::Malformed)?);
    if !path.is_absolute() {
        return Err(DecisionError::Malformed);
    }
    crate::codex::metrics::private_file(&path).map_err(|_| DecisionError::Malformed)?;
    Ok((Some(path), 1))
}

fn outcome(result: &Result<Decision, DecisionError>) -> &'static str {
    match result {
        Ok(_) => "completed",
        Err(DecisionError::TooLarge) => "too_large",
        Err(DecisionError::InvalidAnswer) => "invalid_answer",
        Err(DecisionError::Malformed) => "malformed",
        Err(DecisionError::Auth) => "auth_error",
        Err(DecisionError::Unavailable) => "unavailable",
        Err(DecisionError::Cancelled) => "cancelled",
    }
}

#[async_trait::async_trait]
impl Decider for JevDecider {
    fn name(&self) -> &str {
        "jev"
    }

    async fn decide(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        // The size check belongs to the caller: `within_limits` takes a
        // token count because only the caller knows which tokenizer its
        // state was built against. Counting again here with a different one
        // would refuse requests the service would have accepted. A state
        // that really is too large comes back as 422 and is mapped below.
        let mut last = DecisionError::Unavailable;
        let logical_id = metrics::logical_id();
        for (index, wait) in std::iter::once(None)
            .chain(BACKOFF.iter().map(Some))
            .take(self.max_attempts)
            .enumerate()
        {
            if let Some(wait) = wait {
                tokio::time::sleep(*wait).await;
            }
            let mut observation = self
                .metrics_path
                .as_ref()
                .map(|path| metrics::Attempt::start(path, &self.model, &logical_id, index + 1))
                .transpose()
                .map_err(|_| DecisionError::Malformed)?;
            let result = match self.once(state, questions, &mut observation).await {
                Ok(reply) => {
                    let decision = Decision {
                        answers: reply.answers,
                        input_tokens: reply.usage.as_ref().and_then(|held| held.input_tokens),
                        output_tokens: reply.usage.as_ref().and_then(|held| held.output_tokens),
                    };
                    // Every question answered, each answer usable for the
                    // question it belongs to. A reply that fails this is
                    // not a narrower answer; it answers something else.
                    checked(&decision, questions).map(|()| decision)
                }
                Err(error) => Err(error),
            };
            if let Some(metric) = observation.as_mut() {
                metric
                    .finish(outcome(&result))
                    .map_err(|_| DecisionError::Malformed)?;
            }
            match result {
                Ok(decision) => return Ok(decision),
                Err(error) if Self::retryable(&error) => last = error,
                Err(error) => return Err(error),
            }
        }
        Err(last)
    }
}

#[cfg(test)]
mod metrics_tests;
#[cfg(test)]
mod tests;
