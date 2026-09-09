//! Fixed local selections with endpoint-wide physical inference admission.
//! Share one router across main/child agents. A new router is not recovery from
//! quarantine; its lifetime belongs to the execution owner, not an individual run.
use polaris_provider::local::{Capability, Endpoint, ModelSelection};
use polaris_provider::local_inference::LocalInferenceProvider;
use polaris_provider::{CompletionRequest, CompletionResponse, Provider, ProviderError};
use std::{
    collections::HashMap,
    future::Future,
    pin::Pin,
    sync::{Arc, Mutex},
};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndpointStatus {
    Idle,
    Running,
    Quarantined,
}

pub struct LocalRouter {
    gates: Mutex<HashMap<String, Arc<EndpointGate>>>,
    max_pending: usize,
}
impl LocalRouter {
    pub fn new(max_pending_per_endpoint: usize) -> Self {
        Self {
            gates: Mutex::new(HashMap::new()),
            max_pending: max_pending_per_endpoint.min(1024),
        }
    }
    pub fn bind(&self, selection: ModelSelection) -> Result<Arc<LocalBinding>, ProviderError> {
        let provider = Arc::new(LocalInferenceProvider::new(selection.clone())?);
        Ok(Arc::new(LocalBinding {
            gate: self.gate(selection.endpoint()),
            provider,
            selection: Some(selection),
        }))
    }
    fn gate(&self, endpoint: &Endpoint) -> Arc<EndpointGate> {
        let mut gates = self.gates.lock().unwrap_or_else(|e| e.into_inner());
        gates
            .entry(endpoint.as_str().to_owned())
            .or_insert_with(|| {
                Arc::new(EndpointGate {
                    slot: Arc::new(Semaphore::new(1)),
                    pending: Arc::new(Semaphore::new(self.max_pending)),
                })
            })
            .clone()
    }
}
struct EndpointGate {
    slot: Arc<Semaphore>,
    pending: Arc<Semaphore>,
}
impl EndpointGate {
    fn status(&self) -> EndpointStatus {
        if self.slot.is_closed() {
            EndpointStatus::Quarantined
        } else if self.slot.available_permits() == 0 {
            EndpointStatus::Running
        } else {
            EndpointStatus::Idle
        }
    }
    async fn enter(self: &Arc<Self>) -> Result<Flight, ProviderError> {
        if self.slot.is_closed() {
            return Err(rejected("local endpoint quarantined"));
        }
        let permit = match self.slot.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                let pending = self
                    .pending
                    .clone()
                    .try_acquire_owned()
                    .map_err(|_| rejected("local endpoint pending capacity"))?;
                let permit = self
                    .slot
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| rejected("local endpoint quarantined"))?;
                drop(pending);
                permit
            }
        };
        // A closed semaphore never becomes available again, even after permit drop.
        if self.slot.is_closed() {
            return Err(rejected("local endpoint quarantined"));
        }
        Ok(Flight {
            gate: self.clone(),
            _permit: permit,
            completed: false,
        })
    }
}
fn rejected(reason: &str) -> ProviderError {
    ProviderError::Budget(reason.into())
}
struct Flight {
    gate: Arc<EndpointGate>,
    _permit: OwnedSemaphorePermit,
    completed: bool,
}
impl Drop for Flight {
    fn drop(&mut self) {
        if !self.completed {
            self.gate.slot.close();
        }
    }
}

/// Immutable Provider binding. Setters deliberately keep the trait's no-op defaults.
/// Drop cancels the local future, not necessarily server work. Unknown outcomes
/// permanently quarantine this router's endpoint; no timer/probe can clear it.
pub struct LocalBinding {
    gate: Arc<EndpointGate>,
    provider: Arc<dyn Provider>,
    selection: Option<ModelSelection>,
}
impl LocalBinding {
    pub fn status(&self) -> EndpointStatus {
        self.gate.status()
    }
    pub fn selection(&self) -> Option<&ModelSelection> {
        self.selection.as_ref()
    }
    async fn execute(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        // Reject known unsupported requests before occupying a physical slot.
        if let Some(selection) = &self.selection {
            if (!req.tools.is_empty()
                || req
                    .messages
                    .iter()
                    .any(|m| !m.tool_calls.is_empty() || m.tool_call_id.is_some()))
                && selection.model().capabilities().tools != Capability::Supported
            {
                return Err(ProviderError::Unsupported(
                    "local tools require confirmed support".into(),
                ));
            }
            if req.messages.iter().any(|m| !m.hosted_web_search.is_empty()) {
                return Err(ProviderError::Unsupported(
                    "local hosted web search history".into(),
                ));
            }
        }
        let mut flight = self.gate.enter().await?;
        let response = self.provider.complete(req).await;
        // ProviderError has no transport-stop evidence. Conservatively quarantine
        // every dispatched error, preserving the original error/usage for callers.
        flight.completed = response.is_ok();
        response
    }
}
impl Provider for LocalBinding {
    fn complete<'life0, 'async_trait>(
        &'life0 self,
        req: CompletionRequest,
    ) -> Pin<
        Box<dyn Future<Output = Result<CompletionResponse, ProviderError>> + Send + 'async_trait>,
    >
    where
        'life0: 'async_trait,
        Self: 'async_trait,
    {
        Box::pin(self.execute(req))
    }
}
#[cfg(test)]
#[path = "local_execution/tests.rs"]
mod tests;
