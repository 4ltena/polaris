//! Immutable routing for catalog-validated child roles. No implicit fallback.
use std::{collections::HashMap, sync::Arc};

use crate::{CompletionRequest, CompletionResponse, Provider, ProviderError, ResponseObserver};

/// The caller supplies already fixed providers and retains any shared local
/// router. This wrapper never changes models or owns endpoint admission.
/// Do not mutate a supplied provider through another retained alias.
pub struct RoleProvider {
    parent: Arc<dyn Provider>,
    roles: HashMap<String, Arc<dyn Provider>>,
}

impl RoleProvider {
    pub fn new(parent: Arc<dyn Provider>, roles: HashMap<String, Arc<dyn Provider>>) -> Self {
        Self { parent, roles }
    }
}

#[async_trait::async_trait]
impl Provider for RoleProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        self.parent.complete(req).await
    }

    async fn complete_in_turn(
        &self,
        req: CompletionRequest,
        turn: &crate::turn_affinity::TurnContext,
    ) -> Result<CompletionResponse, ProviderError> {
        self.parent.complete_in_turn(req, turn).await
    }

    async fn complete_in_turn_with_observer(
        &self,
        req: CompletionRequest,
        turn: &crate::turn_affinity::TurnContext,
        observer: Option<&dyn ResponseObserver>,
    ) -> Result<CompletionResponse, ProviderError> {
        self.parent
            .complete_in_turn_with_observer(req, turn, observer)
            .await
    }

    fn resolve_role(&self, role: &str) -> Result<Option<Arc<dyn Provider>>, ProviderError> {
        self.roles.get(role).cloned().map(Some).ok_or_else(|| {
            ProviderError::Unsupported("no provider configured for subagent role".into())
        })
    }
    // Model/effort setters intentionally retain fixed-provider no-op defaults.
}

#[cfg(test)]
#[path = "role/tests.rs"]
mod tests;
