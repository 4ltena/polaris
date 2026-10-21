//! Explicit System One HTTP compatibility contract; never inferred from a name.
//! Credentials are supplied by a trusted owner with an independent origin/model
//! association. This module performs no environment lookup or automatic fallback.
use super::{
    Decider, Decision, DecisionCapabilities, DecisionError, DecisionIdentity, DecisionObservation,
    Question, checked,
    wire::{WireAnswer, unique_map},
};
use std::{
    collections::BTreeMap,
    io::Write,
    time::{Duration, Instant},
};

#[derive(Clone, PartialEq, Eq)]
pub struct CredentialBinding {
    origin: String,
    model: String,
}
#[derive(Clone)]
pub struct SystemOneTarget {
    endpoint: reqwest::Url,
    binding: CredentialBinding,
    loopback: bool,
}
impl SystemOneTarget {
    /// A complete POST endpoint, not a base URL. HTTP requires an explicit
    /// literal loopback address; DNS names never grant that exception.
    pub fn new(
        endpoint: &str,
        model: &str,
        allow_loopback_http: bool,
    ) -> Result<Self, DecisionError> {
        if endpoint.len() > 2048
            || endpoint.contains(['@', '\\'])
            || endpoint.chars().any(char::is_whitespace)
            || endpoint.chars().any(char::is_control)
            || model.is_empty()
            || model.len() > 256
            || !model
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"._:/-@".contains(&b))
        {
            return Err(DecisionError::Malformed);
        }
        let endpoint = reqwest::Url::parse(endpoint).map_err(|_| DecisionError::Malformed)?;
        let loopback = endpoint
            .host_str()
            .and_then(|host| {
                host.trim_matches(['[', ']'])
                    .parse::<std::net::IpAddr>()
                    .ok()
            })
            .is_some_and(|ip| ip.is_loopback());
        if endpoint.as_str().len() > 2048
            || endpoint.host_str().is_none()
            || !endpoint.username().is_empty()
            || endpoint.password().is_some()
            || endpoint.query().is_some()
            || endpoint.fragment().is_some()
            || !(endpoint.scheme() == "https"
                || (endpoint.scheme() == "http" && loopback && allow_loopback_http))
        {
            return Err(DecisionError::Malformed);
        }
        let binding = CredentialBinding {
            origin: endpoint.origin().ascii_serialization(),
            model: model.into(),
        };
        Ok(Self {
            endpoint,
            binding,
            loopback,
        })
    }
    /// The host records this association when the user registers a credential.
    /// Deriving a project target's binding is not credential authorization.
    pub fn binding(&self) -> CredentialBinding {
        self.binding.clone()
    }
}

/// Never Debug/Serialize. The trusted owner must resolve this independently
/// from the requested target, then let the client compare the association.
pub struct BoundCredential {
    binding: CredentialBinding,
    authorization: reqwest::header::HeaderValue,
}
impl BoundCredential {
    pub fn new(binding: CredentialBinding, key: String) -> Result<Self, DecisionError> {
        if key.is_empty() || key.len() > 8192 || !key.bytes().all(|b| (33..=126).contains(&b)) {
            return Err(DecisionError::Auth);
        }
        let mut authorization = reqwest::header::HeaderValue::from_str(&format!("Bearer {key}"))
            .map_err(|_| DecisionError::Auth)?;
        authorization.set_sensitive(true);
        Ok(Self {
            binding,
            authorization,
        })
    }
}

/// Explicit adapter transport limits, not a measured model context budget.
#[derive(Clone, Copy)]
pub struct SystemOneLimits {
    pub request_bytes: usize,
    pub response_bytes: usize,
    pub questions: usize,
    pub options: usize,
    pub timeout_ms: u64,
}
#[derive(Default)]
struct Receipt {
    invoked: bool,
    model: Option<String>,
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}
pub struct SystemOneDecider {
    target: SystemOneTarget,
    authorization: reqwest::header::HeaderValue,
    client: reqwest::Client,
    limits: SystemOneLimits,
}
impl SystemOneDecider {
    pub fn new(
        target: SystemOneTarget,
        credential: BoundCredential,
        limits: SystemOneLimits,
    ) -> Result<Self, DecisionError> {
        if target.binding != credential.binding {
            return Err(DecisionError::Auth);
        }
        if !(1..=crate::MAX_RESPONSE_BYTES).contains(&limits.request_bytes)
            || !(1..=crate::MAX_RESPONSE_BYTES).contains(&limits.response_bytes)
            || !(1..=4096).contains(&limits.questions)
            || !(1..=4096).contains(&limits.options)
            || !(1..=300000).contains(&limits.timeout_ms)
        {
            return Err(DecisionError::Malformed);
        }
        let client = polaris_http::client_builder()
            .map_err(|_| DecisionError::Unavailable)?
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .connect_timeout(Duration::from_millis(limits.timeout_ms.min(3000)))
            .timeout(Duration::from_millis(limits.timeout_ms))
            .build()
            .map_err(|_| DecisionError::Unavailable)?;
        Ok(Self {
            target,
            authorization: credential.authorization,
            client,
            limits,
        })
    }
    async fn run(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
        receipt: &mut Receipt,
    ) -> Result<Decision, DecisionError> {
        if questions.is_empty()
            || questions.len() > self.limits.questions
            || questions
                .values()
                .any(|q| q.options() > self.limits.options)
        {
            return Err(DecisionError::TooLarge);
        }
        for (id, question) in questions {
            let (instructions, valid) = match question {
                Question::Noul { instructions } => (instructions, true),
                Question::Choice {
                    instructions,
                    criteria,
                } => (
                    instructions,
                    !criteria.is_empty() && criteria.keys().all(|key| !key.is_empty()),
                ),
                Question::Score {
                    instructions,
                    criteria,
                } => (instructions, !criteria.is_empty()),
            };
            if id.is_empty() || instructions.is_empty() || !valid {
                return Err(DecisionError::Malformed);
            }
        }
        let mut frame = Frame {
            bytes: Vec::new(),
            limit: self.limits.request_bytes,
        };
        #[derive(serde::Serialize)]
        struct Request<'a> {
            state: &'a serde_json::Value,
            model: &'a str,
            questions: &'a BTreeMap<String, Question>,
        }
        serde_json::to_writer(
            &mut frame,
            &Request {
                state,
                model: &self.target.binding.model,
                questions,
            },
        )
        .map_err(|_| DecisionError::TooLarge)?;
        receipt.invoked = true;
        // One application request and no reqwest retries. Dropping this future
        // stops transport observation, not necessarily remote computation.
        let response = self
            .client
            .post(self.target.endpoint.clone())
            .header(reqwest::header::AUTHORIZATION, self.authorization.clone())
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .body(frame.bytes)
            .send()
            .await
            .map_err(|_| DecisionError::Unavailable)?;
        if !response.status().is_success() {
            return Err(match response.status().as_u16() {
                401 | 403 => DecisionError::Auth,
                413 => DecisionError::TooLarge,
                400 | 422 => DecisionError::Malformed,
                _ => DecisionError::Unavailable,
            });
        }
        let bytes = crate::read_limited(
            response,
            self.limits.response_bytes,
            Duration::from_millis(self.limits.timeout_ms),
        )
        .await
        .map_err(|error| match error {
            crate::ProviderError::Http(_) => DecisionError::Unavailable,
            _ => DecisionError::InvalidAnswer,
        })?;
        // Retain well-formed envelope usage even when the answer is refused.
        // It is service-reported consumption, not validated answer content.
        let envelope: Reply<serde::de::IgnoredAny> =
            serde_json::from_slice(&bytes).map_err(|_| DecisionError::InvalidAnswer)?;
        receipt.input_tokens = envelope.usage.as_ref().and_then(|usage| usage.input_tokens);
        receipt.output_tokens = envelope
            .usage
            .as_ref()
            .and_then(|usage| usage.output_tokens);
        if envelope.model != self.target.binding.model {
            return Err(DecisionError::InvalidAnswer);
        }
        receipt.model = Some(envelope.model);
        let reply: Reply<Answers> =
            serde_json::from_slice(&bytes).map_err(|_| DecisionError::InvalidAnswer)?;
        let mut answers = BTreeMap::new();
        for (id, answer) in reply.answers.0 {
            answers.insert(
                id.clone(),
                answer.checked_for(questions.get(&id).ok_or(DecisionError::InvalidAnswer)?)?,
            );
        }
        let decision = Decision {
            answers,
            input_tokens: reply.usage.as_ref().and_then(|usage| usage.input_tokens),
            output_tokens: reply.usage.as_ref().and_then(|usage| usage.output_tokens),
        };
        checked(&decision, questions)?;
        Ok(decision)
    }
}
struct Frame {
    bytes: Vec<u8>,
    limit: usize,
}
impl Write for Frame {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
            return Err(std::io::Error::other("frame limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Reply<A> {
    model: String,
    answers: A,
    usage: Option<Usage>,
}
#[derive(serde::Deserialize)]
struct Answers(#[serde(deserialize_with = "unique_map")] BTreeMap<String, WireAnswer>);
#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Usage {
    input_tokens: Option<u64>,
    output_tokens: Option<u64>,
}
#[async_trait::async_trait]
impl Decider for SystemOneDecider {
    fn name(&self) -> &str {
        "system-one-compatible"
    }
    fn identity(&self) -> DecisionIdentity {
        DecisionIdentity {
            backend: self.name().into(),
            model: Some(self.target.binding.model.clone()),
            revision: None,
            quantization: None,
        }
    }
    fn capabilities(&self) -> DecisionCapabilities {
        DecisionCapabilities {
            network: crate::NetworkBoundary::RemoteAllowed,
            location: if self.target.loopback {
                super::DecisionLocation::LoopbackHttp
            } else {
                super::DecisionLocation::Remote
            },
            protocol: Some("systemone-http-v1"),
            shapes: Some(super::DecisionShapes::ALL),
            max_questions: Some(self.limits.questions),
            max_options: Some(self.limits.options),
            max_frame_bytes: Some(self.limits.request_bytes),
            cancellation: super::DecisionCancellation::CallerFuture,
            usage: super::DecisionUsageReporting::Optional,
            ..Default::default()
        }
    }
    async fn decide(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        self.observe(state, questions).await.result
    }
    async fn observe(
        &self,
        state: &serde_json::Value,
        questions: &BTreeMap<String, Question>,
    ) -> DecisionObservation {
        let started = Instant::now();
        let mut receipt = Receipt::default();
        let result = self.run(state, questions, &mut receipt).await;
        let mut identity = self.identity();
        identity.model = receipt.model;
        let mut observation =
            DecisionObservation::single(identity, self.capabilities(), result, started.elapsed());
        observation.attempts[0].invoked = receipt.invoked;
        observation.attempts[0].input_tokens = receipt.input_tokens;
        observation.attempts[0].output_tokens = receipt.output_tokens;
        if observation.result.is_err()
            && (receipt.input_tokens.is_some() || receipt.output_tokens.is_some())
        {
            observation.attempts[0].reason = Some("usage_reported_by_rejected_reply".into());
        }
        observation.attempts[0].endpoint_origin = Some(self.target.binding.origin.clone());
        observation
    }
}
#[cfg(test)]
mod tests;
