//! Metadata-only observation of existing generative selection calls.
//! A response receipt is not selection validation; an unmatched start has
//! unknown completion/usage. The wrapper never retries or selects a provider.
use super::{AuditLog, Record};
use polaris_provider::{
    CompletionRequest, CompletionResponse, NetworkBoundary, ObserverError, Provider, ProviderError,
    ResponseObserver, Usage, turn_affinity::TurnContext,
};
use std::{sync::Arc, time::Instant};
use tokio::sync::Mutex;

type Result = std::result::Result<CompletionResponse, ProviderError>;
enum Dispatch<'a> {
    Plain,
    Turn(&'a TurnContext),
    Observed(&'a TurnContext, Option<&'a dyn ResponseObserver>),
}

pub fn observe_arbitration(
    provider: Arc<dyn Provider>,
    audit: Arc<Mutex<AuditLog>>,
) -> Arc<dyn Provider> {
    Arc::new(Observed { provider, audit })
}
struct Observed {
    provider: Arc<dyn Provider>,
    audit: Arc<Mutex<AuditLog>>,
}
impl Observed {
    async fn record(&self, detail: serde_json::Value, result: &str) -> bool {
        self.audit
            .lock()
            .await
            .record(&Record {
                tool: "generative_arbitration",
                detail: &detail.to_string(),
                sandbox: None,
                target: None,
                result,
                caller: "selection-stage",
            })
            .is_ok()
    }
    async fn complete_observed(
        &self,
        request: CompletionRequest,
        dispatch: Dispatch<'_>,
    ) -> Result {
        let mut entropy = [0u8; 16];
        getrandom::fill(&mut entropy).map_err(|_| unavailable(None))?;
        let id: String = entropy.iter().map(|byte| format!("{byte:02x}")).collect();
        let requested_model = self
            .provider
            .model()
            .map(|name| name.escape_debug().take(512).collect::<String>());
        let started = Instant::now();
        if !self
            .record(
                serde_json::json!({
                    "schema_version":1, "stage":"generative_arbitration", "phase":"start",
                    "request_id":id, "requested_model":requested_model,
                    "network_boundary":self.provider.network_boundary(),
                    "scope":"logical_provider_call_not_physical_dispatch",
                }),
                "started",
            )
            .await
        {
            return Err(unavailable(None));
        }
        let output_observer = match &dispatch {
            Dispatch::Observed(_, observer) => *observer,
            _ => None,
        };
        let result = match dispatch {
            Dispatch::Plain => self.provider.complete(request).await,
            Dispatch::Turn(turn) => self.provider.complete_in_turn(request, turn).await,
            Dispatch::Observed(turn, _) => {
                self.provider
                    .complete_in_turn_with_observer(request, turn, None)
                    .await
            }
        };
        let (received, usage) = received_usage(&result);
        let category = result.as_ref().err().map(error_category);
        if !self
            .record(
                serde_json::json!({
                    "schema_version":1, "stage":"generative_arbitration", "phase":"settled",
                    "request_id":id, "requested_model":requested_model,
                    "scope":"provider_response_not_selection_validation",
                    "response_received":received, "usage":usage,
                    "usage_missing":received && usage.is_none(), "error_category":category,
                    "elapsed_ms":started.elapsed().as_millis().min(u64::MAX as u128) as u64,
                }),
                if result.is_ok() {
                    "response_received"
                } else {
                    "provider_failed"
                },
            )
            .await
        {
            // Preserve known consumption for an outer UsageMeter, but never
            // deliver executable output whose settlement could not be audited.
            return match result {
                Ok(response) => Err(unavailable(Some(Box::new(response)))),
                Err(error) => Err(error),
            };
        }
        match result {
            Ok(response) => {
                // Selection responses are atomic: expose only final text,
                // and only after the settlement is durable. Never stream an
                // unaudited preview through the caller's observer.
                if let Some(observer) = output_observer
                    && let Err(reason) =
                        observer.try_emit(polaris_provider::ResponseEvent::FinalText {
                            text: &response.text,
                            delta_matches: None,
                        })
                {
                    return Err(ProviderError::Observation {
                        reason,
                        completed: Some(Box::new(response)),
                    });
                }
                Ok(response)
            }
            Err(error) => Err(error),
        }
    }
}
fn unavailable(completed: Option<Box<CompletionResponse>>) -> ProviderError {
    ProviderError::Observation {
        reason: ObserverError::Closed,
        completed,
    }
}
fn received_usage(result: &Result) -> (bool, Option<Usage>) {
    match result {
        Ok(response) => (true, response.usage),
        Err(ProviderError::ReceivedUsage { usage, .. }) => (true, Some(*usage)),
        Err(ProviderError::Observation {
            completed: Some(response),
            ..
        }) => (true, response.usage),
        Err(_) => (false, None),
    }
}
fn error_category(error: &ProviderError) -> &'static str {
    match error {
        ProviderError::ReceivedUsage { source, .. } => error_category(source),
        ProviderError::Observation { .. } => "observation",
        ProviderError::Http(_) => "http",
        ProviderError::Decode(_) => "decode",
        ProviderError::Auth(_) => "auth",
        ProviderError::Budget(_) => "budget",
        ProviderError::Unsupported(_) => "unsupported",
    }
}
#[async_trait::async_trait]
impl Provider for Observed {
    fn planned_local_model_bytes(&self) -> Option<u64> {
        self.provider.planned_local_model_bytes()
    }
    fn network_boundary(&self) -> NetworkBoundary {
        self.provider.network_boundary()
    }
    fn model(&self) -> Option<String> {
        self.provider.model()
    }
    fn set_model(&self, model: &str) {
        self.provider.set_model(model);
    }
    fn set_effort(&self, effort: Option<&str>) {
        self.provider.set_effort(effort);
    }
    fn resolve_role(
        &self,
        role: &str,
    ) -> std::result::Result<Option<Arc<dyn Provider>>, ProviderError> {
        self.provider.resolve_role(role)
    }
    fn resolve_tier(
        &self,
        tier: &str,
    ) -> std::result::Result<Option<Arc<dyn Provider>>, ProviderError> {
        self.provider.resolve_tier(tier)
    }
    async fn complete(&self, request: CompletionRequest) -> Result {
        self.complete_observed(request, Dispatch::Plain).await
    }
    async fn complete_in_turn(&self, request: CompletionRequest, turn: &TurnContext) -> Result {
        self.complete_observed(request, Dispatch::Turn(turn)).await
    }
    async fn complete_in_turn_with_observer(
        &self,
        request: CompletionRequest,
        turn: &TurnContext,
        observer: Option<&dyn ResponseObserver>,
    ) -> Result {
        self.complete_observed(request, Dispatch::Observed(turn, observer))
            .await
    }
}
#[cfg(test)]
mod tests;
