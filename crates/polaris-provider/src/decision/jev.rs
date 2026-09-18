//! J-4: the HTTP client for a System One decider.
//!
//! One request carries the state once and every question with it, so a run
//! that asks about five hundred things still makes a single round trip.
//! Nothing here generates or accepts free text: the reply is parsed into
//! the typed answers and checked against the questions that were asked
//! before any of it is returned.
use super::{Decider, Decision, DecisionError, Question, checked};
use std::collections::BTreeMap;
use std::time::Duration;

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
        let client = reqwest::Client::builder()
            // A decider answers questions; it never follows a redirect to
            // somewhere that was not configured.
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(CONNECT_TIMEOUT)
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|_| DecisionError::Unavailable)?;
        Ok(Self {
            endpoint: format!("{}/systemone", base_url.trim_end_matches('/')),
            api_key,
            model,
            client,
        })
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
        response
            .json::<Reply>()
            .await
            .map_err(|_| DecisionError::InvalidAnswer)
    }

    fn retryable(error: &DecisionError) -> bool {
        // Auth, too large and a malformed answer will not change by asking
        // again. Only "the service could not answer now" is worth a wait.
        matches!(error, DecisionError::Unavailable)
    }
}

#[async_trait::async_trait]
impl Decider for JevDecider {
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
        for wait in std::iter::once(None).chain(BACKOFF.iter().map(Some)) {
            if let Some(wait) = wait {
                tokio::time::sleep(*wait).await;
            }
            match self.once(state, questions).await {
                Ok(reply) => {
                    let decision = Decision {
                        answers: reply.answers,
                        input_tokens: reply.usage.as_ref().and_then(|held| held.input_tokens),
                        output_tokens: reply.usage.as_ref().and_then(|held| held.output_tokens),
                    };
                    // Every question answered, each answer usable for the
                    // question it belongs to. A reply that fails this is
                    // not a narrower answer; it answers something else.
                    checked(&decision, questions)?;
                    return Ok(decision);
                }
                Err(error) if Self::retryable(&error) => last = error,
                Err(error) => return Err(error),
            }
        }
        Err(last)
    }
}

#[cfg(test)]
mod tests;
