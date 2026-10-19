//! Explicit configured runtime factory without discovery or credential reads.
//! Preparation consumes a staged copy or creates a fresh copy on the owned
//! preparation thread; the trait must never be invoked on the Engine reactor.
mod recipe;
pub use recipe::WorkspacePreparationRecipe;

#[cfg(test)]
use crate::CurrentSourcePolicy;
use crate::{ConfirmedSourceGrant, ServiceError, TrustedRunFactory, TrustedRunInputs};
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

/// Credential values must already have been loaded/registered by the trusted
/// owner. Never implement Debug or serialize these inputs. No token method is
/// invoked by construction, staging or prepare.
pub enum TrustedBackend {
    Codex {
        /// The single authentication, or `UnboundAccountTokens` once the
        /// accounts registry has slots: then only a bound run may send.
        tokens: Arc<dyn TokenSource>,
        accounts: Option<Arc<polaris_auth::accounts::owner::AccountOwner>>,
    },
    Openai {
        api_key: String,
    },
    Local {
        selection: ModelSelection,
        router: Arc<LocalRouter>,
    },
}

const MCP_SELECTION_MODEL: &str = polaris_core::mcp::preselection::MODEL;

/// The providers built once from the startup backend.
struct StartupProviders {
    primary: Arc<dyn Provider>,
    summary: Option<Arc<dyn Provider>>,
    selection: Option<Arc<dyn Provider>>,
}

/// Per-run providers for the registration a run was bound to at acceptance.
struct BoundProviders {
    primary: Option<Arc<dyn Provider>>,
    summary: Option<Arc<dyn Provider>>,
    selection: Option<Arc<dyn Provider>>,
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
    /// Loaded and protected by the trusted startup owner. Dedicated stdio
    /// resources must remain owned until all runs and native cleanup finish.
    pub mcp: Option<polaris_core::mcp::runtime::RuntimeConfig>,
    /// Profile text and IDs are trusted startup data. The IPC surface may only
    /// select enabled/mode/initial phase from this fixed configuration.
    pub workflow: polaris_core::workflow::WorkflowConfig,
    /// Frozen top-level project setting; the IPC/GUI cannot alter it.
    pub input_preparation: polaris_core::input_preparation::InputPreparationMode,
    /// The typed judge, read from the same `[decision]` configuration the
    /// terminal reads. `None` is the ordinary state: every path that would
    /// ask it something works without an answer.
    pub decider: Option<Arc<dyn polaris_provider::decision::Decider>>,
    /// User-scoped startup snapshot. `None` is the default/off path.
    pub answer_finalizer: Option<Arc<crate::shadow_finalizer::ShadowFinalizerOwner>>,
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

/// What a run needs to build one more provider on the account it already
/// holds. The owner's credentials are kept, not a second copy of the
/// provider: a bound model is fixed at construction, so naming a different
/// one means building a different instance rather than retuning a shared
/// one — the same reason weights are separate instances in the first place.
///
/// This is not a second admission decision. Nothing here widens which
/// account a run may use; it only lets the account already admitted answer
/// under another model name.
#[derive(Clone)]
enum AccountModels {
    Codex(Arc<dyn TokenSource>),
    Openai(String),
}

impl AccountModels {
    fn build(&self, model: &str) -> Result<Arc<dyn Provider>, FactoryError> {
        match self {
            Self::Codex(tokens) => Ok(Arc::new(
                CodexProvider::new(
                    polaris_provider::codex::ENDPOINT_BASE.into(),
                    model.to_owned(),
                    tokens.clone(),
                )
                .map_err(|_| FactoryError::Backend)?,
            )),
            Self::Openai(api_key) => Ok(Arc::new(
                OpenAiProvider::new(
                    "https://api.openai.com/v1".into(),
                    api_key.clone(),
                    model.to_owned(),
                )
                .map_err(|_| FactoryError::Backend)?,
            )),
        }
    }
}

pub struct ConfiguredRunFactory {
    bootstrap: ValidatedOwnerBootstrap,
    grant: ConfirmedSourceGrant,
    resources: TrustedFactoryResources,
    primary_provider: Arc<dyn Provider>,
    /// Present only with multiple accounts; each run binds its own providers.
    accounts: Option<Arc<polaris_auth::accounts::owner::AccountOwner>>,
    summary_provider: Option<Arc<dyn Provider>>,
    selection_provider: Option<Arc<dyn Provider>>,
    unconfigured_provider: Arc<dyn Provider>,
    // Preserve endpoint quarantine across jobs; never recreate it for a run.
    local_router: Arc<LocalRouter>,
    /// `None` for a local-only bootstrap, which has no account to build on.
    account_models: Option<AccountModels>,
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
            || resources
                .mcp
                .as_ref()
                .is_some_and(|config| config.project_id() != document.project_id.as_str())
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
        let summary_provider: Option<Arc<dyn Provider>> = if !document.history_mode.is_legacy() {
            if resources.history.is_none() {
                return Err(FactoryError::Backend);
            }
            let summary: Arc<dyn Provider> = match &backend {
                TrustedBackend::Codex { tokens, .. }
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
        let selection_provider: Option<Arc<dyn Provider>> = match &backend {
            TrustedBackend::Codex { tokens, .. }
                if document.provider == BootstrapProvider::Codex =>
            {
                CodexProvider::new(
                    polaris_provider::codex::ENDPOINT_BASE.into(),
                    MCP_SELECTION_MODEL.into(),
                    tokens.clone(),
                )
                .ok()
                .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
            }
            TrustedBackend::Openai { api_key }
                if document.provider == BootstrapProvider::Openai =>
            {
                OpenAiProvider::new(
                    "https://api.openai.com/v1".into(),
                    api_key.clone(),
                    MCP_SELECTION_MODEL.into(),
                )
                .ok()
                .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
            }
            _ => None,
        };
        let accounts = match &backend {
            TrustedBackend::Codex { accounts, .. }
                if document.provider == BootstrapProvider::Codex =>
            {
                accounts.clone()
            }
            _ => None,
        };
        let account_models = match &backend {
            TrustedBackend::Codex { tokens, .. }
                if document.provider == BootstrapProvider::Codex =>
            {
                Some(AccountModels::Codex(tokens.clone()))
            }
            TrustedBackend::Openai { api_key }
                if document.provider == BootstrapProvider::Openai =>
            {
                Some(AccountModels::Openai(api_key.clone()))
            }
            _ => None,
        };
        let backend: Arc<dyn Provider> = match backend {
            TrustedBackend::Codex { tokens, .. }
                if document.provider == BootstrapProvider::Codex =>
            {
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
                        StartupProviders {
                            primary: binding,
                            summary: summary_provider,
                            selection: selection_provider,
                        },
                        router,
                        accounts,
                        account_models,
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
            StartupProviders {
                primary: backend,
                summary: summary_provider,
                selection: selection_provider,
            },
            local_router,
            accounts,
            account_models,
        )
    }

    fn finish(
        bootstrap: ValidatedOwnerBootstrap,
        grant: ConfirmedSourceGrant,
        resources: TrustedFactoryResources,
        providers: StartupProviders,
        local_router: Arc<LocalRouter>,
        accounts: Option<Arc<polaris_auth::accounts::owner::AccountOwner>>,
        account_models: Option<AccountModels>,
    ) -> Result<Self, FactoryError> {
        let StartupProviders {
            primary: primary_provider,
            summary: summary_provider,
            selection: selection_provider,
        } = providers;
        let unconfigured_provider =
            Arc::new(RoleProvider::new(primary_provider.clone(), HashMap::new()));
        Ok(Self {
            bootstrap,
            grant,
            resources,
            primary_provider,
            accounts,
            summary_provider,
            selection_provider,
            unconfigured_provider,
            local_router,
            account_models,
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

    /// The providers one accepted run may use. Without a binding the run keeps
    /// the single authentication built at startup.
    fn bound_providers(&self, run: &polaris_core::desktop_store::RunRecord) -> BoundProviders {
        let (Some(owner), Some(account)) = (&self.accounts, &run.account) else {
            return BoundProviders {
                primary: None,
                summary: None,
                selection: None,
            };
        };
        let source: Arc<dyn TokenSource> = Arc::new(crate::auth_tokens::AccountTokens::new(
            polaris_auth::accounts::observation::BoundTokenSource::new(
                owner.clone(),
                account.registration_id.clone(),
                account.generation.get(),
            ),
        ));
        let codex = |model: &str| -> Option<Arc<dyn Provider>> {
            CodexProvider::new(
                polaris_provider::codex::ENDPOINT_BASE.into(),
                model.into(),
                source.clone(),
            )
            .ok()
            .map(|provider| Arc::new(provider) as Arc<dyn Provider>)
        };
        let primary = codex(self.bootstrap.model());
        if let (Some(primary), Some(effort)) = (&primary, self.bootstrap.effective_effort()) {
            primary.set_effort(Some(effort));
        }
        let summary = self.summary_provider.as_ref().and_then(|_| {
            let summary = codex("gpt-6-astra")?;
            summary.set_effort(Some("medium"));
            Some(summary)
        });
        BoundProviders {
            primary,
            summary,
            selection: self
                .selection_provider
                .as_ref()
                .and_then(|_| codex(MCP_SELECTION_MODEL)),
        }
    }

    fn provider_for(
        &self,
        primary: &Arc<dyn Provider>,
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
        let unconfigured = || -> Arc<dyn Provider> {
            if Arc::ptr_eq(primary, &self.primary_provider) {
                self.unconfigured_provider.clone()
            } else {
                Arc::new(RoleProvider::new(primary.clone(), HashMap::new()))
            }
        };
        // Weights are priced independently of roles: a run may price them
        // without binding a single role, and the two are kept apart all the
        // way down because they answer different questions.
        let mut tiers = HashMap::<String, Arc<dyn Provider>>::new();
        if let Some(saved) = &run.tier_bindings {
            for binding in &saved.bindings {
                let models = self.account_models.as_ref().ok_or(FactoryError::Binding)?;
                if binding.runtime != polaris_core::desktop_store::SavedRuntime::Account {
                    // A weight on a local runtime would need the same
                    // inventory round trip a role does, and nothing asks
                    // for it yet. Refusing keeps a saved value from being
                    // quietly ignored.
                    return Err(FactoryError::Binding);
                }
                let provider = models.build(&binding.model)?;
                // Set once, on an instance nothing else holds yet. This is
                // the only safe moment: the same instance is shared across
                // a wave of concurrent delegations afterwards.
                if let Some(effort) = &binding.effort {
                    provider.set_effort(Some(effort));
                }
                tiers.insert(
                    polaris_skills::Tier::from(binding.tier).name().to_owned(),
                    provider,
                );
            }
        }
        let Some(saved) = &run.role_bindings else {
            return Ok(if tiers.is_empty() {
                unconfigured()
            } else {
                Arc::new(RoleProvider::new(primary.clone(), HashMap::new()).with_tiers(tiers))
            });
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
                    polaris_core::desktop_store::SavedRuntime::LlamaCpp => Runtime::LlamaCpp,
                    // An account-backed role answers on the registration
                    // the run already holds. There is no endpoint to bind
                    // and no inventory to consult: the model name is what
                    // the account is asked to answer under, and whether it
                    // will is settled by the first request, not here.
                    polaris_core::desktop_store::SavedRuntime::Account => {
                        let models = self.account_models.as_ref().ok_or(FactoryError::Binding)?;
                        // A type that needs tools must not be bound to a
                        // model nobody has observed supporting them. The
                        // observation is the settings screen's, recorded
                        // when the binding was saved.
                        if !agent.allowed_tools.is_empty()
                            && binding.observed_tool_support
                                != polaris_core::desktop_store::ToolSupport::Supported
                        {
                            return Err(FactoryError::Backend);
                        }
                        let provider = models.build(&binding.model)?;
                        if let Some(effort) = &binding.effort {
                            provider.set_effort(Some(effort));
                        }
                        roles.insert(binding.role.clone(), provider);
                        continue;
                    }
                };
                let endpoint = binding
                    .endpoint
                    .as_deref()
                    .ok_or(FactoryError::Binding)
                    .and_then(|given| Endpoint::parse(given).map_err(|_| FactoryError::Binding))?;
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
        if roles.is_empty() && tiers.is_empty() {
            Ok(unconfigured())
        } else {
            Ok(Arc::new(
                RoleProvider::new(primary.clone(), roles).with_tiers(tiers),
            ))
        }
    }
}
impl TrustedRunFactory for ConfiguredRunFactory {
    fn workflow_config(&self) -> Option<&polaris_core::workflow::WorkflowConfig> {
        Some(&self.resources.workflow)
    }

    fn input_preparation(&self) -> polaris_core::input_preparation::InputPreparationMode {
        self.resources.input_preparation
    }

    fn decider(&self) -> Option<Arc<dyn polaris_provider::decision::Decider>> {
        self.resources.decider.clone()
    }

    fn answer_finalizer(&self) -> Option<Arc<crate::shadow_finalizer::ShadowFinalizerOwner>> {
        self.resources.answer_finalizer.clone()
    }

    fn workflow_skills(&self) -> &[polaris_skills::Skill] {
        &self.resources.skills
    }
    fn role_catalog(&self) -> &[AgentType] {
        &self.resources.agent_types
    }

    /// With one usable registration the run takes it; the quota selection
    /// decides only between several, and refuses rather than guessing.
    fn bind_account(
        &self,
    ) -> Result<Option<polaris_core::desktop_store::SavedAccount>, ServiceError> {
        let Some(owner) = &self.accounts else {
            return Ok(None);
        };
        // Every model this run may send to must be covered by the binding.
        let mut models = vec![self.bootstrap.model()];
        if self.summary_provider.is_some() {
            models.push("gpt-6-astra");
        }
        if self.selection_provider.is_some() {
            models.push(MCP_SELECTION_MODEL);
        }
        let (registration_id, generation) = owner
            .bind(&models, None)
            .map_err(|_| ServiceError::Options)?;
        polaris_core::desktop_store::SavedAccount::new(registration_id, generation)
            .map(Some)
            .map_err(|_| ServiceError::Options)
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
                    scope: Box::new(staged.prepared.policy().clone()),
                }
            }
        });
        let run = published
            .state
            .runs
            .iter()
            .find(|run| run.run.run_id == target.run_id && run.run.attempt_id == target.attempt_id)
            .ok_or(ServiceError::Options)?;
        let bound = self.bound_providers(run);
        // A binding that cannot be built must not fall back to another source.
        if run.account.is_some() && bound.primary.is_none() {
            return Err(ServiceError::Options);
        }
        let primary = bound
            .primary
            .clone()
            .unwrap_or(self.primary_provider.clone());
        let provider = self
            .provider_for(&primary, target, published)
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
            mcp: self
                .resources
                .mcp
                .clone()
                .map(|config| crate::TrustedMcpResources {
                    config,
                    selection_provider: bound
                        .selection
                        .clone()
                        .or_else(|| self.selection_provider.clone()),
                }),
            external_ceiling: polaris_core::mcp::operation::ExternalCeiling::desktop_tier(
                self.grant.tier,
            ),
            history: bound
                .summary
                .as_ref()
                .or(self.summary_provider.as_ref())
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
