//! Admit one root request, verify its actual input, and reject every tool response.
use super::*;
use polaris_provider::{
    CompletionRequest, CompletionResponse, Role, Usage, turn_affinity::TurnContext,
};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

pub(super) fn usage_of(result: &Result<CompletionResponse, ProviderError>) -> Option<Usage> {
    match result {
        Ok(response) => response.usage,
        Err(ProviderError::ReceivedUsage { usage, .. }) => Some(*usage),
        Err(ProviderError::Observation {
            completed: Some(response),
            ..
        }) => response.usage,
        _ => None,
    }
}

fn budget(reason: &'static str) -> ProviderError {
    ProviderError::Budget(reason.into())
}

/// Fixed categories and numeric hints only: an upstream message/body may carry
/// account identifiers or arbitrary text and must not enter benchmark evidence.
pub(super) fn provider_diagnostic(error: &ProviderError) -> Value {
    let mut error = error;
    while let ProviderError::ReceivedUsage { source, .. } = error {
        error = source;
    }
    let kind = match error {
        ProviderError::Http(_) => "http",
        ProviderError::Auth(_) => "auth",
        ProviderError::Decode(_) => "decode",
        ProviderError::Budget(_) => "budget",
        ProviderError::Observation { .. } => "observation",
        ProviderError::Unsupported(_) => "unsupported",
        ProviderError::ReceivedUsage { .. } => unreachable!(),
    };
    let mut diagnostic = json!({"kind":kind,"body_retained":false});
    if let ProviderError::Http(message) = error {
        let (status, retry_after, body) =
            if let Some(rest) = message.strip_prefix("rate limited. retry-after: ") {
                let (hint, body) = rest.split_once(" seconds. ").unwrap_or(("", ""));
                (Some(429), hint.parse::<u64>().ok(), body)
            } else if let Some(rest) = message.strip_prefix("status ") {
                let (header, body) = rest.split_once(": ").unwrap_or((rest, ""));
                let status = header
                    .split_whitespace()
                    .next()
                    .and_then(|value| value.parse::<u16>().ok())
                    .filter(|value| (100..600).contains(value));
                (status, None, body)
            } else {
                (None, None, "")
            };
        diagnostic["status"] = json!(status);
        diagnostic["retry_after_seconds"] = json!(retry_after);
        let parsed = (body.len() <= 16_384)
            .then(|| serde_json::from_str::<Value>(body).ok())
            .flatten();
        let details = parsed.as_ref().and_then(|value| value.get("error"));
        let code = |field: &str| {
            details
                .and_then(|value| value.get(field))
                .and_then(Value::as_str)
                .filter(|value| {
                    matches!(
                        *value,
                        "usage_limit_reached"
                            | "rate_limit_exceeded"
                            | "insufficient_quota"
                            | "invalid_request_error"
                            | "server_error"
                    )
                })
        };
        diagnostic["error_type"] = json!(code("type"));
        diagnostic["error_code"] = json!(code("code"));
        diagnostic["resets_at"] = json!(
            details
                .and_then(|value| value.get("resets_at"))
                .and_then(Value::as_u64)
        );
    }
    diagnostic
}

#[derive(Default)]
pub(super) struct Evidence {
    pub request: Option<Value>,
    pub response: Option<Value>,
    pub diagnostics: Option<Value>,
    pub provider_error: Option<Value>,
    pub known_cost_upper: Option<u128>,
    pub error: Option<&'static str>,
}

pub(super) struct Guard {
    inner: Arc<dyn Provider>,
    expected: String,
    instructions: String,
    metrics: PathBuf,
    first_metric: usize,
    fee_remaining: u64,
    claimed: AtomicBool,
    pub evidence: Mutex<Evidence>,
}

impl Guard {
    pub fn new(
        inner: Arc<dyn Provider>,
        expected: String,
        instructions: String,
        metrics: PathBuf,
        first_metric: usize,
        fee_remaining: u64,
    ) -> Self {
        Self {
            inner,
            expected,
            instructions,
            metrics,
            first_metric,
            fee_remaining,
            claimed: AtomicBool::new(false),
            evidence: Mutex::new(Evidence::default()),
        }
    }

    fn reject(&self, reason: &'static str) -> ProviderError {
        self.evidence
            .lock()
            .expect("evidence lock")
            .error
            .get_or_insert(reason);
        budget(reason)
    }

    fn prepare(&self, mut request: CompletionRequest) -> Result<CompletionRequest, ProviderError> {
        if self.evidence.lock().expect("evidence lock").error.is_some() {
            return Err(self.reject("trial already stopped"));
        }
        if self.inner.model().as_deref() != Some(ROOT_MODEL)
            || request.messages.len() != 1
            || request.messages[0].role != Role::User
            || request.messages[0].content != self.expected
            || !request.messages[0].tool_calls.is_empty()
            || request.messages[0].tool_call_id.is_some()
        {
            return Err(self.reject("actual request projection or model differs"));
        }
        if self.claimed.swap(true, Ordering::SeqCst) {
            return Err(self.reject("one root request already admitted"));
        }
        // The evaluation format is identical in both arms. Golden values/anchors
        // never enter it, and the production user projection remains untouched.
        request
            .system
            .push_str("\n\n## Synthetic evaluation response format\n");
        request.system.push_str(&self.instructions);
        request.tools.clear();
        let body = polaris_provider::codex::build_body(ROOT_MODEL, &request, Some(EFFORT));
        let serialized = body.to_string();
        if serialized.len() > REQUEST_BYTES {
            return Err(self.reject("serialized request byte cap reached"));
        }
        self.evidence.lock().expect("evidence lock").request = Some(json!({
            "source":"actual_provider_request", "user_sha256":content_hash(self.expected.as_bytes()),
            "user_bytes":self.expected.len(), "user_tokens":polaris_core::budget::count_tokens(&self.expected),
            "system_sha256":content_hash(request.system.as_bytes()),
            "serialized_sha256":content_hash(serialized.as_bytes()), "serialized_bytes":serialized.len(),
            "tool_schemas":0, "endpoint_output_cap_verified":false
        }));
        Ok(request)
    }

    fn observe(
        &self,
        result: &Result<CompletionResponse, ProviderError>,
    ) -> Result<(), &'static str> {
        let response = match result {
            Ok(response) => Some(response),
            Err(ProviderError::Observation {
                completed: Some(response),
                ..
            }) => Some(response.as_ref()),
            _ => None,
        };
        let mut evidence = self.evidence.lock().expect("evidence lock");
        if let Err(error) = result {
            evidence.provider_error = Some(provider_diagnostic(error));
        }
        if let Some(response) = response {
            // Normal responses cannot contain tools. Rejected arguments are not
            // re-encoded and falsely treated as original wire token counts.
            let bounded = response.text.len() <= 24_576;
            evidence.response = Some(json!({"text":bounded.then_some(&response.text),
                "text_sha256":content_hash(response.text.as_bytes()), "text_bytes":response.text.len(),
                "tool_calls":response.tool_calls.len(), "hosted_actions":response.hosted_web_search.len(),
                "url_citations":response.url_citations.len(), "body_retained":bounded}));
            if bounded && response.tool_calls.is_empty() {
                let parts = polaris_core::measurement::output::parts(&response.text, &[]);
                let output =
                    response
                        .usage
                        .map_or(polaris_core::measurement::Reported::Unknown, |usage| {
                            polaris_core::measurement::Reported::Known {
                                tokens: usage.output_tokens.into(),
                            }
                        });
                evidence.diagnostics = Some(json!({"parts":parts,
                    "recount":polaris_core::measurement::output::recount(&parts, output),
                    "source":"visible body; no tool arguments", "semantic_classification":false}));
            }
        }
        let usage = usage_of(result).ok_or("response usage missing")?;
        let rows =
            accounting::read_metrics(&self.metrics).map_err(|_| "physical metrics unavailable")?;
        if rows.len() != self.first_metric + 1 {
            return Err("one physical metric required");
        }
        let row = rows.last().expect("one metric");
        if rows[..self.first_metric]
            .iter()
            .any(|old| old["attempt_id"] == row["attempt_id"])
        {
            return Err("duplicate physical metric identity");
        }
        let cost = accounting::assess_row(row, ROOT_MODEL, EFFORT, usage)
            .map_err(|_| "physical usage invalid")?;
        evidence.known_cost_upper = Some(cost.upper);
        if usage.output_tokens > 4096 {
            return Err("observed output stop exceeded");
        }
        if cost.upper >= u128::from(self.fee_remaining) {
            return Err("observed fee stop reached");
        }
        if result.is_err() {
            return Err("provider failed");
        }
        let response = response.expect("successful response");
        if response.text.len() > 24_576
            || !response.tool_calls.is_empty()
            || !response.hosted_web_search.is_empty()
            || !response.url_citations.is_empty()
        {
            return Err("tool, hosted action or oversized body prohibited");
        }
        Ok(())
    }
}

#[async_trait::async_trait]
impl Provider for Guard {
    async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Err(self.reject("unscoped, child, summary or compaction request prohibited"))
    }
    async fn complete_in_turn(
        &self,
        request: CompletionRequest,
        turn: &TurnContext,
    ) -> Result<CompletionResponse, ProviderError> {
        let request = self.prepare(request)?;
        let result = match tokio::time::timeout(
            Duration::from_secs(120),
            self.inner.complete_in_turn(request, turn),
        )
        .await
        {
            Ok(result) => result,
            Err(_) => Err(budget("provider deadline reached")),
        };
        if let Err(reason) = self.observe(&result) {
            let source = self.reject(reason);
            // Even rejected/failed responses retain known consumption once.
            return Err(match usage_of(&result) {
                Some(usage) => ProviderError::ReceivedUsage {
                    source: Box::new(source),
                    usage,
                },
                None => source,
            });
        }
        result
    }
    fn model(&self) -> Option<String> {
        Some(ROOT_MODEL.into())
    }
}

pub(super) struct NoChildren;
#[async_trait::async_trait]
impl Provider for NoChildren {
    async fn complete(&self, _: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        Err(budget("children prohibited"))
    }
}
