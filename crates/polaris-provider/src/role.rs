//! Immutable routing for catalog-validated child roles. No implicit fallback.
use std::{collections::HashMap, sync::Arc};

use crate::{CompletionRequest, CompletionResponse, Provider, ProviderError, ResponseObserver};

/// The caller supplies already fixed providers and retains any shared local
/// router. This wrapper never changes models or owns endpoint admission.
/// Do not mutate a supplied provider through another retained alias.
pub struct RoleProvider {
    parent: Arc<dyn Provider>,
    /// `None` is a run that does not route by role at all, which answers
    /// every role with `Ok(None)` and lets the caller use the parent.
    /// `Some` is a run that does, and then a role it has no entry for is a
    /// misconfiguration — including `Some(empty)`, which is how a desktop
    /// run with no bindings refuses every delegation.
    roles: Option<HashMap<String, Arc<dyn Provider>>>,
    /// One entry per configured weight, each a provider already built with
    /// its own fixed model and effort. Routing a task to a weight therefore
    /// never mutates anything — the alternatives exist side by side, which
    /// is what lets concurrent tasks of different weights run without
    /// fighting over one provider's model. A run that configures no weights
    /// leaves this empty and behaves exactly as before.
    tiers: HashMap<String, Arc<dyn Provider>>,
}

impl RoleProvider {
    pub fn new(parent: Arc<dyn Provider>, roles: HashMap<String, Arc<dyn Provider>>) -> Self {
        Self {
            parent,
            roles: Some(roles),
            tiers: HashMap::new(),
        }
    }

    /// A run that routes by weight but not by role — the CLI and TUI, which
    /// have no role bindings and must keep sending delegations to the
    /// parent rather than refusing them.
    pub fn tiers_only(
        parent: Arc<dyn Provider>,
        tiers: HashMap<String, Arc<dyn Provider>>,
    ) -> Self {
        Self {
            parent,
            roles: None,
            tiers,
        }
    }

    /// Weights are supplied separately from roles, so the call sites that
    /// configure only roles stay as they are.
    pub fn with_tiers(mut self, tiers: HashMap<String, Arc<dyn Provider>>) -> Self {
        self.tiers = tiers;
        self
    }
}

#[async_trait::async_trait]
impl Provider for RoleProvider {
    fn network_boundary(&self) -> crate::NetworkBoundary {
        self.parent.network_boundary()
    }
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
        let Some(roles) = &self.roles else {
            return Ok(None);
        };
        roles.get(role).cloned().map(Some).ok_or_else(|| {
            ProviderError::Unsupported("no provider configured for subagent role".into())
        })
    }

    /// Unlike a role, an unconfigured weight is `Ok(None)` rather than an
    /// error. A role names a specific child the caller asked for by name, so
    /// its absence is a misconfiguration; a weight is a preference about
    /// cost that a run is free not to express, and every type declares one
    /// whether or not the run configured anything.
    fn resolve_tier(&self, tier: &str) -> Result<Option<Arc<dyn Provider>>, ProviderError> {
        Ok(self.tiers.get(tier).cloned())
    }
    /// The parent's, because a plain `complete` here goes to the parent. The
    /// weights resolve to their own instances and report their own.
    fn model(&self) -> Option<String> {
        self.parent.model()
    }
    // Root switches affect only the parent. Tier providers remain separate,
    // immutable instances, so concurrent delegated tasks cannot retune one
    // another.
    fn set_model(&self, model: &str) {
        self.parent.set_model(model);
    }

    fn set_effort(&self, effort: Option<&str>) {
        self.parent.set_effort(effort);
    }
}

#[cfg(test)]
#[path = "role/tests.rs"]
mod tests;
