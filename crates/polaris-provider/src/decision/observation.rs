//! Per-request provenance; no prompt, answer text, credential or shared last-backend slot.
use super::{Decision, DecisionCapabilities, DecisionError};
use serde::Serialize;
use std::time::Duration;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DecisionIdentity {
    pub backend: String,
    pub model: Option<String>,
    pub revision: Option<String>,
    pub quantization: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct DecisionAttempt {
    pub identity: DecisionIdentity,
    /// Adapter invocation, not proof of a network request or completed inference.
    pub invoked: bool,
    pub reason: Option<String>,
    pub elapsed_ms: u64,
    pub error: Option<DecisionError>,
    pub timed_out: bool,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct DecisionObservation {
    pub result: Result<Decision, DecisionError>,
    pub identity: DecisionIdentity,
    /// The adapter that produced this result, not the configured primary.
    pub capabilities: DecisionCapabilities,
    pub attempts: Vec<DecisionAttempt>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DecisionTrace {
    pub configured: DecisionIdentity,
    pub effective: DecisionIdentity,
    pub capabilities: DecisionCapabilities,
    pub error: Option<DecisionError>,
    pub attempts: Vec<DecisionAttempt>,
}

impl DecisionObservation {
    pub fn single(
        identity: DecisionIdentity,
        capabilities: DecisionCapabilities,
        result: Result<Decision, DecisionError>,
        elapsed: Duration,
    ) -> Self {
        let attempt = DecisionAttempt {
            identity: identity.clone(),
            invoked: true,
            reason: None,
            elapsed_ms: elapsed.as_millis().try_into().unwrap_or(u64::MAX),
            error: result.as_ref().err().cloned(),
            timed_out: false,
            input_tokens: result.as_ref().ok().and_then(|reply| reply.input_tokens),
            output_tokens: result.as_ref().ok().and_then(|reply| reply.output_tokens),
        };
        Self {
            result,
            identity,
            capabilities,
            attempts: vec![attempt],
        }
    }
}
