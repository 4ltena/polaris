//! Explicit configured runtime factory without discovery or credential reads.
//! Preparation consumes a staged copy or creates a fresh copy on the owned
//! preparation thread; the trait must never be invoked on the Engine reactor.
mod recipe;
pub use recipe::WorkspacePreparationRecipe;

use crate::{CurrentSourcePolicy, ServiceError, TrustedRunFactory, TrustedRunInputs};
use polaris_core::{
    audit::AuditLog,
    desktop_execution::{SandboxMode, SandboxPolicy},
    desktop_store::{BootstrapProvider, BootstrapTier, Published, ValidatedOwnerBootstrap},
    isolated_run::PreparedWorkspace,
    local_execution::LocalRouter,
    prompt::AlwaysOn,
    tool_memory::ToolMemory,
};
use polaris_desktop_protocol::{ids::DecimalU64, request::RunTarget, run_state::RunState};
use polaris_provider::{
    Provider, TokenSource,
    codex::CodexProvider,
    local::{Capability, Endpoint, ExecutionLocation, LocalAdapter, ModelSelection, Runtime},
    openai::OpenAiProvider,
    role::RoleProvider,
};
use polaris_skills::{AgentType, Skill};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex},
};

/// Trusted owner-confirmed source capability, separate from bootstrap metadata.
/// Tier is a ceiling; it neither adds approvals nor grants arbitrary shell/build.
pub struct ConfirmedSourceGrant {
    pub policy: CurrentSourcePolicy,
    pub tier: BootstrapTier,
}

/// Credential values must already have been loaded/registered by the trusted
/// owner. Never implement Debug or serialize these inputs. No token method is
/// invoked by construction, staging or prepare.
pub enum TrustedBackend {
    Codex {
        tokens: Arc<dyn TokenSource>,
    },
    Openai {
        api_key: String,
    },
    Local {
        selection: ModelSelection,
        router: Arc<LocalRouter>,
    },
}

pub struct TrustedFactoryResources {
    pub always_on: AlwaysOn,
    pub audit: Arc<tokio::sync::Mutex<AuditLog>>,
    pub skills: Vec<Skill>,
    pub agent_types: Vec<AgentType>,
    pub max_turns: u32,
    pub spawn_concurrency: usize,
    pub spawn_write_concurrency: usize,
    pub tool_memory: Option<ToolMemory>,
    pub history: Option<TrustedMemoryIndex>,
}

/// Fixed local resource snapshot, prepared by the trusted startup owner.
#[derive(Clone)]
pub struct TrustedMemoryIndex {
    pub embedder: polaris_core::conversation_memory::LocalStdioEmbedder,
    pub database: std::path::PathBuf,
    pub identity: polaris_core::desktop_store::MemoryResources,
}

#[derive(Debug, thiserror::Error)]
pub enum FactoryError {
    #[error("configured runtime binding mismatch")]
    Binding,
    #[error("configured runtime resource bounds")]
    Bounds,
    #[error("configured runtime backend unavailable")]
    Backend,
    #[error("configured runtime workspace slot occupied or unavailable")]
    Workspace,
}

struct Staged {
    prepared: Arc<PreparedWorkspace>,
    mutation_policy: Option<SandboxPolicy>,
}

#[derive(Default)]
struct WorkspaceSlot {
    staged: Option<Staged>,
    consumed_revision: Option<DecimalU64>,
    consumed_target: Option<RunTarget>,
}

pub struct ConfiguredRunFactory {
    bootstrap: ValidatedOwnerBootstrap,
    grant: ConfirmedSourceGrant,
    resources: TrustedFactoryResources,
    primary_provider: Arc<dyn Provider>,
    summary_provider: Option<Arc<dyn Provider>>,
    unconfigured_provider: Arc<dyn Provider>,
    // Preserve endpoint quarantine across jobs; never recreate it for a run.
    local_router: Arc<LocalRouter>,
    staged: Mutex<WorkspaceSlot>,
    recipe: Option<WorkspacePreparationRecipe>,
}
impl ConfiguredRunFactory {
    pub fn new(
        bootstrap: ValidatedOwnerBootstrap,
        grant: ConfirmedSourceGrant,
        backend: TrustedBackend,
        resources: TrustedFactoryResources,
    ) -> Result<Self, FactoryError> {
        let document = bootstrap.document();
        let policy = &grant.policy;
        if policy.source_path.as_os_str() != std::path::Path::new(&document.source_path).as_os_str()
            || policy.source_identity.device != document.source_identity.device
            || policy.source_identity.inode != document.source_identity.inode
            || policy.policy_revision != document.policy_revision
            || grant.tier != document.tier
            || !policy.read_allowed
            || (grant.tier == BootstrapTier::ReadOnly && policy.write_allowed)
        {
            return Err(FactoryError::Binding);
        }
        if resources.max_turns == 0
            || resources.max_turns > polaris_core::desktop_run::MAX_TURNS
            || resources.spawn_concurrency == 0
            || resources.spawn_concurrency > polaris_core::desktop_run::MAX_CONCURRENCY
            || resources.spawn_write_concurrency == 0
            || resources.spawn_write_concurrency > resources.spawn_concurrency
            || resources.agent_types.len() > 32
            || resources.skills.len() > 128
        {
            return Err(FactoryError::Bounds);
        }
        let bytes = resources
            .skills
            .iter()
            .fold(resources.always_on.system().len(), |n, s| {
                n.saturating_add(s.name.len())
                    .saturating_add(s.description.len())
                    .saturating_add(s.body.len())
                    .saturating_add(s.path.as_os_str().len())
            });
        let bytes = resources.agent_types.iter().fold(bytes, |n, a| {
            a.allowed_tools
                .iter()
                .fold(n, |n, t| n.saturating_add(t.len()))
                .saturating_add(a.name.len())
                .saturating_add(a.description.len())
                .saturating_add(a.body.len())
                .saturating_add(a.path.as_os_str().len())
                .saturating_add(a.output_schema.as_os_str().len())
        });
        if bytes > 1024 * 1024 {
            return Err(FactoryError::Bounds);
        }
        let mut names = HashSet::new();
        for agent in &resources.agent_types {
            if agent.name.is_empty() || agent.name.len() > 128 || !names.insert(agent.name.clone())
            {
                return Err(FactoryError::Binding);
            }
        }
        let local_router = Arc::new(LocalRouter::new(8));
        let summary_provider: Option<Arc<dyn Provider>> =
            if document.history_mode == polaris_desktop_protocol::snapshot::HistoryMode::Strict10 {
                if resources.history.is_none() {
                    return Err(FactoryError::Backend);
                }
                let summary: Arc<dyn Provider> = match &backend {
                    TrustedBackend::Codex { tokens }
                        if document.provider == BootstrapProvider::Codex =>
                    {
                        Arc::new(
                            CodexProvider::new(
                                polaris_provider::codex::ENDPOINT_BASE.into(),
                                "gpt-6-astra".into(),
                                tokens.clone(),
                            )
                            .map_err(|_| FactoryError::Backend)?,
                        )
                    }
                    TrustedBackend::Openai { api_key }
                        if document.provider == BootstrapProvider::Openai =>
                    {
                        Arc::new(
                            OpenAiProvider::new(
                                "https://api.openai.com/v1".into(),
                                api_key.clone(),
                                "gpt-6-astra".into(),
                            )
                            .map_err(|_| FactoryError::Backend)?,
                        )
                    }
                    _ => return Err(FactoryError::Binding),
                };
                summary.set_effort(Some("medium"));
                Some(summary)
            } else {
                None
            };
        let backend: Arc<dyn Provider> = match backend {
            TrustedBackend::Codex { tokens } if document.provider == BootstrapProvider::Codex => {
                Arc::new(
                    CodexProvider::new(
                        polaris_provider::codex::ENDPOINT_BASE.into(),
                        bootstrap.model().into(),
                        tokens,
                    )
                    .map_err(|_| FactoryError::Backend)?,
                )
            }
            TrustedBackend::Openai { api_key }
                if document.provider == BootstrapProvider::Openai =>
            {
                if api_key.is_empty() {
                    return Err(FactoryError::Backend);
                }
                Arc::new(
                    OpenAiProvider::new(
                        "https://api.openai.com/v1".into(),
                        api_key,
                        bootstrap.model().into(),
                    )
                    .map_err(|_| FactoryError::Backend)?,
                )
            }
            TrustedBackend::Local { selection, router } => {
                let runtime = match document.provider {
                    BootstrapProvider::Ollama => Runtime::Ollama,
                    BootstrapProvider::Lmstudio => Runtime::LmStudio,
                    _ => return Err(FactoryError::Binding),
                };
                if selection.runtime() != runtime
                    || selection.model().id() != bootstrap.model()
                    || Some(selection.endpoint().as_str()) != document.local_endpoint.as_deref()
                {
                    return Err(FactoryError::Binding);
                }
                if !Arc::ptr_eq(&router, &local_router) {
                    // Adopt the owner-supplied long-lived router for both main
                    // and role bindings; it remains the quarantine authority.
                    let binding = router.bind(selection).map_err(|_| FactoryError::Backend)?;
                    return Self::finish(
                        bootstrap,
                        grant,
                        resources,
                        binding,
                        router,
                        summary_provider,
                    );
                }
                router.bind(selection).map_err(|_| FactoryError::Backend)?
            }
            _ => return Err(FactoryError::Binding),
        };
        if let Some(effort) = bootstrap.effective_effort() {
            backend.set_effort(Some(effort));
        }
        Self::finish(
            bootstrap,
            grant,
            resources,
            backend,
            local_router,
            summary_provider,
        )
    }

    fn finish(
        bootstrap: ValidatedOwnerBootstrap,
        grant: ConfirmedSourceGrant,
        resources: TrustedFactoryResources,
        primary_provider: Arc<dyn Provider>,
        local_router: Arc<LocalRouter>,
        summary_provider: Option<Arc<dyn Provider>>,
    ) -> Result<Self, FactoryError> {
        let unconfigured_provider =
            Arc::new(RoleProvider::new(primary_provider.clone(), HashMap::new()));
        Ok(Self {
            bootstrap,
            grant,
            resources,
            primary_provider,
            summary_provider,
            unconfigured_provider,
            local_router,
            staged: Mutex::new(WorkspaceSlot::default()),
            recipe: None,
        })
    }

    /// Reusable per-run preparation. The Engine invokes the synchronous trait
    /// only from its owned preparation thread, never from the reactor.
    pub fn with_preparation_recipe(
        bootstrap: ValidatedOwnerBootstrap,
        grant: ConfirmedSourceGrant,
        backend: TrustedBackend,
        resources: TrustedFactoryResources,
        recipe: WorkspacePreparationRecipe,
    ) -> Result<Self, FactoryError> {
        if recipe.source != grant.policy.source_path
            || (recipe.copy_mutations && !grant.policy.write_allowed)
        {
            return Err(FactoryError::Binding);
        }
        let mut factory = Self::new(bootstrap, grant, backend, resources)?;
        factory.recipe = Some(recipe);
        Ok(factory)
    }

    /// Owner must first verify the packaged helper against the assembly hash,
    /// register credentials, and create this copy outside the Engine reactor.
    /// Opaque PreparedWorkspace retains the checked copy/helper/runtime boundary.
    pub fn stage_workspace(&self, prepared: PreparedWorkspace) -> Result<(), FactoryError> {
        self.stage(prepared, None)
    }
    /// Explicit confirmed COPY mutation grant; never source-apply authorization.
    pub fn stage_workspace_with_mutations(
        &self,
        prepared: PreparedWorkspace,
        mutation_policy: SandboxPolicy,
    ) -> Result<(), FactoryError> {
        self.stage(prepared, Some(mutation_policy))
    }
    fn stage(
        &self,
        prepared: PreparedWorkspace,
        mutation_policy: Option<SandboxPolicy>,
    ) -> Result<(), FactoryError> {
        if self.recipe.is_some() {
            return Err(FactoryError::Workspace);
        }
        self.check_workspace(&prepared)?;
        if let Some(scope) = &mutation_policy
            && (!self.grant.policy.write_allowed
                || self.grant.tier == BootstrapTier::ReadOnly
                || prepared
                    .policy()
                    .restrict(scope.mode(), scope.writable_roots())
                    .map_err(|_| FactoryError::Binding)?
                    != *scope)
        {
            return Err(FactoryError::Binding);
        }
        let mut slot = self.staged.lock().map_err(|_| FactoryError::Workspace)?;
        if slot.staged.is_some() {
            return Err(FactoryError::Workspace);
        }
        slot.staged = Some(Staged {
            prepared: Arc::new(prepared),
            mutation_policy,
        });
        Ok(())
    }
    fn check_workspace(&self, prepared: &PreparedWorkspace) -> Result<(), FactoryError> {
        let snapshot = prepared.snapshot();
        let policy = &self.grant.policy;
        let mode = if policy.write_allowed {
            SandboxMode::WorkspaceWrite
        } else {
            SandboxMode::ReadOnly
        };
        if snapshot.source_path != policy.source_path
            || snapshot.source_identity.device != policy.source_identity.device.get()
            || snapshot.source_identity.inode != policy.source_identity.inode.get()
            || prepared.policy().mode() != mode
        {
            return Err(FactoryError::Binding);
        }
        Ok(())
    }
    fn check_saved(&self, target: &RunTarget, published: &Published) -> Result<(), FactoryError> {
        let doc = self.bootstrap.document();
        let configuration = &published.state.configuration;
        if published.marker.deleted
            || published.marker.project_id != doc.project_id
            || published.marker.session_id != doc.session_id
            || published.state.policy_revision != doc.policy_revision
            || configuration.configuration_revision != doc.configuration_revision
            || configuration.provider != doc.provider.as_str()
            || configuration.model != self.bootstrap.model()
            || configuration.effort != self.bootstrap.stored_effort()
            || configuration.history_mode != doc.history_mode
        {
            return Err(FactoryError::Binding);
        }
        let run = published
            .state
            .runs
            .iter()
            .find(|r| r.run.run_id == target.run_id && r.run.attempt_id == target.attempt_id)
            .ok_or(FactoryError::Binding)?;
        if run.configuration != *configuration
            || run.policy_revision != doc.policy_revision
            || run.run.state.is_terminal()
            || run.run.state == RunState::Cancelling
            || run.operations.is_empty()
        {
            return Err(FactoryError::Binding);
        }
        if let Some(saved) = &run.role_bindings {
            saved
                .validate_catalog(&self.resources.agent_types)
                .map_err(|_| FactoryError::Binding)?;
        }
        Ok(())
    }

    fn provider_for(
        &self,
        target: &RunTarget,
        published: &Published,
    ) -> Result<Arc<dyn Provider>, FactoryError> {
        let run = published
            .state
            .runs
            .iter()
            .find(|run| run.run.run_id == target.run_id && run.run.attempt_id == target.attempt_id)
            .ok_or(FactoryError::Binding)?;
        let mut roles = HashMap::<String, Arc<dyn Provider>>::new();
        let Some(saved) = &run.role_bindings else {
            return Ok(self.unconfigured_provider.clone());
        };
        {
            let runtime_owner = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .map_err(|_| FactoryError::Backend)?;
            for binding in &saved.bindings {
                let agent = self
                    .resources
                    .agent_types
                    .iter()
                    .find(|agent| agent.name == binding.role)
                    .ok_or(FactoryError::Binding)?;
                let runtime = match binding.runtime {
                    polaris_core::desktop_store::SavedRuntime::Ollama => Runtime::Ollama,
                    polaris_core::desktop_store::SavedRuntime::LmStudio => Runtime::LmStudio,
                };
                let endpoint =
                    Endpoint::parse(&binding.endpoint).map_err(|_| FactoryError::Binding)?;
                let selection = runtime_owner.block_on(async {
                    let adapter =
                        LocalAdapter::new(runtime, endpoint).map_err(|_| FactoryError::Backend)?;
                    let inventory = adapter
                        .inventory()
                        .await
                        .map_err(|_| FactoryError::Backend)?;
                    let observed = inventory
                        .iter()
                        .find(|model| model.id() == binding.model)
                        .ok_or(FactoryError::Backend)?;
                    let selection = adapter
                        .select(observed)
                        .await
                        .map_err(|_| FactoryError::Backend)?;
                    let selected = selection.model();
                    if selected.execution_location() == ExecutionLocation::Remote
                        || selected.capabilities().completion == Capability::Unsupported
                        || (!agent.allowed_tools.is_empty()
                            && selected.capabilities().tools != Capability::Supported)
                    {
                        return Err(FactoryError::Backend);
                    }
                    Ok(selection)
                })?;
                let provider: Arc<dyn Provider> = self
                    .local_router
                    .bind(selection)
                    .map_err(|_| FactoryError::Backend)?;
                roles.insert(binding.role.clone(), provider);
            }
        }
        if roles.is_empty() {
            Ok(self.unconfigured_provider.clone())
        } else {
            Ok(Arc::new(RoleProvider::new(
                self.primary_provider.clone(),
                roles,
            )))
        }
    }
}
impl TrustedRunFactory for ConfiguredRunFactory {
    fn role_catalog(&self) -> &[AgentType] {
        &self.resources.agent_types
    }

    fn prepare(
        &self,
        target: &RunTarget,
        published: &Published,
    ) -> Result<TrustedRunInputs, ServiceError> {
        self.check_saved(target, published)
            .map_err(|_| ServiceError::Options)?;
        let mut slot = self.staged.lock().map_err(|_| ServiceError::Worker)?;
        if slot
            .consumed_revision
            .is_some_and(|r| published.marker.session_revision <= r)
            || slot.consumed_target.as_ref() == Some(target)
        {
            return Err(ServiceError::Options);
        }
        let staged = if let Some(recipe) = &self.recipe {
            // A failed preparation does not permit replay of its accepted run.
            slot.consumed_revision = Some(published.marker.session_revision);
            slot.consumed_target = Some(target.clone());
            let prepared = recipe
                .prepare(&self.grant)
                .map_err(|_| ServiceError::Options)?;
            self.check_workspace(&prepared)
                .map_err(|_| ServiceError::Options)?;
            let mutation_policy = recipe.copy_mutations.then(|| prepared.policy().clone());
            Staged {
                prepared: Arc::new(prepared),
                mutation_policy,
            }
        } else {
            let staged = slot.staged.as_ref().ok_or(ServiceError::Options)?;
            self.check_workspace(&staged.prepared)
                .map_err(|_| ServiceError::Options)?;
            let staged = slot.staged.take().ok_or(ServiceError::Options)?;
            slot.consumed_revision = Some(published.marker.session_revision);
            slot.consumed_target = Some(target.clone());
            staged
        };
        let execution_capability = Some(match self.grant.tier {
            BootstrapTier::ReadOnly | BootstrapTier::ReadCreate => {
                polaris_core::desktop_execution::ConfirmedExecutionCapability::FixedHelpersOnly
            }
            BootstrapTier::ReadCreateBuild | BootstrapTier::ReadCreateBuildExternal => {
                polaris_core::desktop_execution::ConfirmedExecutionCapability::ConfinedCode {
                    scope: staged.prepared.policy().clone(),
                }
            }
        });
        let provider = self
            .provider_for(target, published)
            .map_err(|_| ServiceError::Options)?;
        Ok(TrustedRunInputs {
            prepared: staged.prepared,
            provider: provider.clone(),
            provider_pool: provider,
            always_on: self.resources.always_on.clone(),
            audit: self.resources.audit.clone(),
            max_turns: self.resources.max_turns,
            skills: self.resources.skills.clone(),
            agent_types: self.resources.agent_types.clone(),
            spawn_concurrency: self.resources.spawn_concurrency,
            spawn_write_concurrency: self.resources.spawn_write_concurrency,
            tool_memory: self.resources.tool_memory.clone(),
            history: self
                .summary_provider
                .as_ref()
                .zip(self.resources.history.as_ref())
                .map(|(provider, index)| {
                    let ledger = polaris_provider::attempts::AttemptLedger::default();
                    let mut embedder = index.embedder.clone();
                    embedder.attempt_ledger = Some(ledger.clone());
                    crate::TrustedHistoryResources {
                        summary_provider: provider.clone(),
                        embedder: Arc::new(embedder),
                        database: index.database.clone(),
                        identity: index.identity.clone(),
                        embedding_usage: ledger,
                    }
                }),
            mutation_policy: staged.mutation_policy,
            execution_capability,
        })
    }
}

#[cfg(test)]
mod tests;
