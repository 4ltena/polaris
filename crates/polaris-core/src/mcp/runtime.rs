//! Retained run ownership for lazy MCP connections, tools and resource reads.
mod local;
mod metadata;
/// Visible to the crate for one reason: registration asks `admit_read_root`
/// the question a run will ask, so it proposes a readable root that will
/// actually be admitted instead of one refused later.
pub(crate) mod resources;
pub use local::{ApprovedCall, LocalCall};

use super::{Candidate, CatalogView, PendingSelection, RequestSnapshot};
use crate::{
    desktop_store::{McpPromptResult, McpPublishedResult, OwnerRequestHistory},
    execution_owner::{McpCall, McpCallError, McpPort},
    mcp::results::{Chunk, ResultStore},
};
use futures_util::FutureExt;
use polaris_mcp::{
    Connection,
    catalog::ItemId,
    registration::{Destination, Registry, Server},
    store::Secrets,
};
use polaris_provider::{Message, ToolCall};
use polaris_sandbox::SandboxPolicy;
use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    sync::Arc,
    time::Instant,
};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error(transparent)]
    Transport(#[from] polaris_mcp::Error),
    #[error(transparent)]
    Context(#[from] super::Error),
    #[error(transparent)]
    Owner(#[from] McpCallError),
}

/// Immutable inputs from the trusted configuration owner. Construction performs
/// validation only; it never starts a child, opens a connection or reads stores.
/// The owner must protect and load the stores before model-visible preparation.
#[derive(Clone)]
pub struct RuntimeConfig {
    registry: Registry,
    project: String,
    secrets: Option<Arc<Secrets>>,
    policies: BTreeMap<String, SandboxPolicy>,
    // Cloned into the run, and also retained by the factory through Engine drain.
    stdio_resources: BTreeMap<String, Arc<resources::StdioResources>>,
    quarantined: BTreeMap<String, polaris_mcp::Error>,
}

impl RuntimeConfig {
    pub fn new(
        registry: Registry,
        project: String,
        secrets: impl Into<Option<Arc<Secrets>>>,
        policies: BTreeMap<String, SandboxPolicy>,
    ) -> Result<Self, polaris_mcp::Error> {
        let secrets = secrets.into();
        registry.validate()?;
        if project.is_empty() || project.len() > 128 || project.contains('\0') {
            return Err(polaris_mcp::Error::InvalidRegistration);
        }
        for key in policies.keys() {
            if !registry.servers.iter().any(|s| {
                s.server_id == *key
                    && s.enabled_projects.contains(&project)
                    && matches!(s.transport, Destination::Stdio { .. })
            }) {
                return Err(polaris_mcp::Error::Protection);
            }
        }
        for server in registry
            .servers
            .iter()
            .filter(|s| s.enabled_projects.contains(&project))
        {
            let references = match &server.transport {
                Destination::Stdio {
                    secret_env,
                    sandbox_profile,
                    ..
                } => {
                    let policy = policies
                        .get(&server.server_id)
                        .ok_or(polaris_mcp::Error::Protection)?;
                    let registered = policy.isolated_boundary().map(|b| {
                        if b.loopback_network {
                            "loopback"
                        } else {
                            policy.mode().as_str()
                        }
                    });
                    if registered != Some(sandbox_profile.as_str())
                        || policy
                            .isolated_boundary()
                            .is_none_or(|b| b.environment.is_none())
                    {
                        return Err(polaris_mcp::Error::Protection);
                    }
                    secret_env
                }
                Destination::StreamableHttp { secret_headers, .. } => secret_headers,
            };
            for reference in references.values() {
                secrets
                    .as_ref()
                    .ok_or(polaris_mcp::Error::Auth)?
                    .resolve(reference)?;
            }
        }
        Ok(Self {
            registry,
            project,
            secrets,
            policies,
            stdio_resources: BTreeMap::new(),
            quarantined: BTreeMap::new(),
        })
    }

    /// Registered servers that failed owner preparation, with a closed reason.
    pub fn quarantined(&self) -> &BTreeMap<String, polaris_mcp::Error> {
        &self.quarantined
    }

    /// Fixed host project identity, used before binding this configuration to a run.
    pub fn project_id(&self) -> &str {
        &self.project
    }

    pub fn registry(&self) -> &Registry {
        &self.registry
    }

    /// IDs only. Disabled projects and credential/destination data stay hidden.
    pub fn server_ids(&self) -> Vec<&str> {
        self.registry
            .servers
            .iter()
            .filter(|s| {
                s.enabled_projects.contains(&self.project) && s.capabilities.catalog_metadata
            })
            .map(|s| s.server_id.as_str())
            .collect()
    }

    /// Code-execution and dispatch tools of a known app are never preselected.
    pub fn preselectable(&self, server_id: &str, tool: &str) -> bool {
        self.server(server_id)
            .map(|server| super::apps::find(server).is_none_or(|app| app.preselectable(tool)))
            .unwrap_or(false)
    }

    fn server(&self, id: &str) -> Result<&Server, polaris_mcp::Error> {
        self.registry
            .servers
            .iter()
            .find(|s| {
                s.server_id == id
                    && s.enabled_projects.contains(&self.project)
                    && s.capabilities.catalog_metadata
            })
            .ok_or(polaris_mcp::Error::Auth)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    Loading,
    Ready,
    Failed,
    Closing,
}
struct Entry {
    connection: Connection,
    phase: Phase,
}

/// One provider request's selection. Only this runtime creates its generation;
/// the next freeze invalidates it even when the catalog itself has not changed.
pub struct RunRequest {
    snapshot: RequestSnapshot,
    generation: u64,
    binding: super::results::RunBinding,
}
impl RunRequest {
    pub fn specs(&self) -> Vec<polaris_tools::ToolSpec> {
        self.snapshot.specs()
    }
}

/// Only live catalogs form a view: one expired or failed connection must not
/// hide every other server from selection and explicit search.
fn usable(connections: &[Entry], revision: u64) -> Vec<&Connection> {
    connections
        .iter()
        .filter(|e| e.phase == Phase::Ready && e.connection.catalog(revision).is_ok())
        .map(|e| &e.connection)
        .collect()
}

/// The host's explicit prompt selection. Re-exported so a caller that
/// drives a run does not need the desktop wire crate for one struct.
pub use polaris_desktop_protocol::request::McpPromptChoice;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PromptState {
    Unselected,
    Unconfirmed,
    Confirmed,
}

/// Keep this outside the cancellable agent future. It borrows connections into
/// individual RPCs and retains them through cancellation, panic and publication
/// waits. Call shutdown on the same live Tokio runtime before dropping it.
pub struct RunRuntime {
    config: RuntimeConfig,
    port: McpPort,
    connections: Vec<Entry>,
    catalog_budget: polaris_mcp::catalog::Budget,
    results: ResultStore,
    pending: PendingSelection,
    request_servers: BTreeSet<String>,
    request_generation: u64,
    prompt: PromptState,
    closed: bool,
    preselected: Vec<super::preselection::Pick>,
    preselection_status: Option<super::preselection::Status>,
    local_preselection: Option<super::preselection::Context>,
    // Servers whose refresh failed for the current request only.
    preselection_skipped: BTreeSet<String>,
    local_operations: u64,
}

impl RunRuntime {
    pub fn new(config: RuntimeConfig, port: McpPort) -> Result<Self, Error> {
        let results = ResultStore::new(port.binding().clone())?;
        Ok(Self {
            config,
            port,
            connections: Vec::new(),
            catalog_budget: polaris_mcp::catalog::Budget::default(),
            results,
            pending: PendingSelection::default(),
            request_servers: BTreeSet::new(),
            request_generation: 0,
            prompt: PromptState::Unselected,
            closed: false,
            preselected: Vec::new(),
            preselection_status: None,
            local_preselection: None,
            preselection_skipped: BTreeSet::new(),
            local_operations: 0,
        })
    }

    pub fn server_ids(&self) -> Vec<&str> {
        self.config.server_ids()
    }

    pub fn config(&self) -> &RuntimeConfig {
        &self.config
    }

    pub(crate) fn preselection_pending(&self) -> bool {
        self.request_generation == 0 && self.preselection_status.is_none() && !self.closed
    }

    /// Host inputs only, consumed before the first request in the root's meter.
    pub(super) fn set_local_preselection(&mut self, input: super::preselection::Context) {
        if self.preselection_pending() && self.local_preselection.is_none() {
            self.local_preselection = Some(input);
        }
    }

    pub(crate) async fn prepare_local_preselection(
        &mut self,
        query: &str,
        phase: &str,
        meter: &polaris_provider::UsageMeter,
    ) -> Option<super::preselection::Status> {
        let mut input = self.local_preselection.take()?;
        if !self.preselection_pending() {
            return None;
        }
        input.selector = input.selector.map(|provider| {
            std::sync::Arc::new(meter.wrap(provider))
                as std::sync::Arc<dyn polaris_provider::Provider>
        });
        input
            .prepare(self, query, phase, std::future::pending())
            .await;
        self.preselection_status
    }

    /// Host-chosen picks for this root run, set once before its first request.
    pub fn set_preselection(&mut self, picks: Vec<super::preselection::Pick>) {
        if self.request_generation == 0 && self.preselection_status.is_none() {
            self.preselected = picks.into_iter().take(super::preselection::PICKS).collect();
        }
    }

    pub fn record_preselection_status(&mut self, status: super::preselection::Status) {
        self.preselection_status.get_or_insert(status);
    }

    pub fn preselection_status(&self) -> Option<super::preselection::Status> {
        self.preselection_status
    }

    /// Reload expired preselected catalogs while the run budget allows. A
    /// server that cannot be refreshed is left out of this request only.
    pub async fn refresh_preselected(
        &mut self,
        cancelled: impl Future<Output = ()>,
    ) -> Result<(), Error> {
        let servers: BTreeSet<String> = self
            .preselected
            .iter()
            .map(|pick| pick.server_id.clone())
            .collect();
        self.preselection_skipped.clear();
        tokio::pin!(cancelled);
        for server in servers {
            let fresh = self.connections.iter().any(|entry| {
                entry.phase == Phase::Ready
                    && entry.connection.server_id() == server
                    && entry
                        .connection
                        .catalog(self.config.registry.revision)
                        .is_ok()
            });
            if fresh {
                continue;
            }
            match self.ensure_catalog(&server, &mut cancelled).await {
                Ok(()) => {}
                Err(
                    error @ (Error::Owner(_)
                    | Error::Transport(
                        polaris_mcp::Error::Cancelled | polaris_mcp::Error::StopUnconfirmed,
                    )),
                ) => return Err(error),
                Err(_) => {
                    self.preselection_skipped.insert(server);
                }
            }
        }
        Ok(())
    }

    /// Initial dispatch history comes from the same writer as prompt ACKs.
    /// No transport request or model-supplied target is involved.
    pub async fn request_history(
        &self,
        cancelled: impl Future<Output = ()>,
    ) -> Result<OwnerRequestHistory, Error> {
        self.active()?;
        if self.prompt == PromptState::Unconfirmed {
            return Err(McpCallError::Storage.into());
        }
        let history = self.port.history(cancelled).await?;
        self.active()?;
        Ok(history)
    }

    fn active(&self) -> Result<(), Error> {
        if self.closed || self.port.stopped() {
            return Err(McpCallError::Cancelled.into());
        }
        Ok(())
    }

    fn view(&self) -> Result<CatalogView<'_>, Error> {
        self.active()?;
        // Loading/failed/expired entries remain owned but never contribute a catalog.
        Ok(CatalogView::from_connections(
            &usable(&self.connections, self.config.registry.revision),
            self.config.registry.revision,
        )?)
    }

    pub fn candidates(
        &self,
        query: &str,
        server: Option<&str>,
        phase: &str,
    ) -> Result<Vec<Candidate>, Error> {
        Ok(self.view()?.candidates(query, server, phase)?)
    }

    pub fn unloaded_server_ids(&self) -> Vec<&str> {
        self.server_ids()
            .into_iter()
            .filter(|id| {
                !self.connections.iter().any(|entry| {
                    entry.phase == Phase::Ready
                        && entry.connection.server_id() == *id
                        && entry
                            .connection
                            .catalog(self.config.registry.revision)
                            .is_ok()
                })
            })
            .collect()
    }

    pub(crate) fn candidates_matching(
        &self,
        query: &str,
        server: Option<&str>,
        phase: &str,
        kind: Option<polaris_mcp::catalog::Kind>,
    ) -> Result<super::selection::CandidateSearch, Error> {
        Ok(self
            .view()?
            .candidates_matching(query, server, phase, kind)?)
    }

    pub fn rejected_items(&self) -> Result<usize, Error> {
        Ok(self.view()?.rejected_items()?)
    }

    pub fn select(&mut self, id: &ItemId, generation: &str) -> Result<(), Error> {
        self.active()?;
        let view = CatalogView::from_connections(
            &usable(&self.connections, self.config.registry.revision),
            self.config.registry.revision,
        )?;
        Ok(self.pending.select(&view, id, generation)?)
    }

    /// The host calls once at the next provider boundary, never per tool in a
    /// response. The returned snapshot remains authoritative for that response.
    pub fn freeze(&mut self) -> Result<RunRequest, Error> {
        if self.prompt == PromptState::Unconfirmed {
            return Err(McpCallError::Unavailable.into());
        }
        // Consume before building the view: stale catalogs must not preserve
        // a pending selection for some later request with refreshed metadata.
        let mut pending = std::mem::take(&mut self.pending);
        self.request_generation = self
            .request_generation
            .checked_add(1)
            .ok_or(super::Error::ContextLimit)?;
        self.active()?;
        self.request_servers.clear();
        let live = usable(&self.connections, self.config.registry.revision);
        let view = CatalogView::from_connections(&live, self.config.registry.revision)?;
        // Explicit selections were added first; preselection fills what remains
        // in rank order, and the lowest-ranked picks are the ones left out.
        self.preselected
            .retain(|pick| !matches!(view.preselected(pick), Ok(None)));
        for pick in &self.preselected {
            if self.preselection_skipped.contains(&pick.server_id) {
                continue;
            }
            if let Ok(Some((id, generation))) = view.preselected(pick)
                && matches!(
                    pending.select(&view, &id, &generation),
                    Err(super::Error::ContextLimit)
                )
            {
                break;
            }
        }
        let snapshot = pending.freeze(&view)?;
        self.request_servers = snapshot.servers();
        Ok(RunRequest {
            snapshot,
            generation: self.request_generation,
            binding: self.port.binding().clone(),
        })
    }

    /// No transport handle, registry data, grant or mutable ResultStore is
    /// exposed to model dispatch. Return the owner's exact canonical message.
    pub async fn call(
        &mut self,
        snapshot: &RunRequest,
        call: &ToolCall,
        suffix: Vec<Message>,
        cancelled: impl Future<Output = ()>,
    ) -> Result<McpPublishedResult, Error> {
        if snapshot.generation != self.request_generation
            || &snapshot.binding != self.port.binding()
        {
            return Err(super::Error::SelectionStale.into());
        }
        let identity = snapshot
            .snapshot
            .resolve(&self.view()?, &call.name, &call.arguments)?
            .clone();
        let server = self.config.server(&identity.id.server_id)?;
        let destination = server.destination_hash()?;
        let app = super::apps::find(server);
        let entry = self
            .connections
            .iter_mut()
            .find(|e| e.phase == Phase::Ready && e.connection.server_id() == identity.id.server_id)
            .ok_or(polaris_mcp::Error::Protocol)?;
        let operation = entry.connection.prepare_operation(
            self.config.registry.revision,
            &identity.catalog_generation,
            &identity.id,
            call.arguments.clone(),
        )?;
        let call = McpCall::new(
            operation,
            call.id.clone(),
            self.config.project.clone(),
            suffix,
        )?
        .with_app(app);
        Ok(self
            .port
            .call(
                &mut entry.connection,
                call,
                &mut self.results,
                cancelled,
                |binding| {
                    binding.registry_revision == self.config.registry.revision
                        && binding.destination_hash == destination
                        && binding.item == identity.id
                        && binding.catalog_generation == identity.catalog_generation
                        && binding.schema_hash.as_deref() == Some(&identity.schema_hash)
                },
            )
            .await?)
    }

    /// Read capability is distinct from generic tool approval. The operation
    /// still passes the same durable intent, terminal and cancellation owner.
    pub async fn read_resource(
        &mut self,
        snapshot: &RunRequest,
        call: &ToolCall,
        suffix: Vec<Message>,
        cancelled: impl Future<Output = ()>,
    ) -> Result<McpPublishedResult, Error> {
        self.active()?;
        if snapshot.generation != self.request_generation
            || &snapshot.binding != self.port.binding()
        {
            return Err(super::Error::SelectionStale.into());
        }
        let read = super::operation::read::ResourceRead::parse(call)
            .map_err(|_| super::Error::InvalidArguments)?;
        let id = read.id();
        let server = self.config.server(&id.server_id)?;
        if !server.capabilities.resource_read {
            return Err(polaris_mcp::Error::Auth.into());
        }
        let destination = server.destination_hash()?;
        let secret_revision = self.config.secrets.as_ref().map(|s| s.revision());
        let entry = self
            .connections
            .iter_mut()
            .find(|entry| {
                entry.phase == Phase::Ready && entry.connection.server_id() == id.server_id
            })
            .ok_or(polaris_mcp::Error::Protocol)?;
        let operation = entry.connection.prepare_operation(
            self.config.registry.revision,
            read.generation(),
            &id,
            read.arguments().clone(),
        )?;
        let call = McpCall::resource_read(
            operation,
            call.id.clone(),
            self.config.project.clone(),
            suffix,
        )?;
        Ok(self
            .port
            .call(
                &mut entry.connection,
                call,
                &mut self.results,
                cancelled,
                |binding| {
                    self.config
                        .server(&id.server_id)
                        .is_ok_and(|server| server.capabilities.resource_read)
                        && self.config.secrets.as_ref().map(|s| s.revision()) == secret_revision
                        && binding.registry_revision == self.config.registry.revision
                        && binding.destination_hash == destination
                        && binding.item == id
                        && binding.catalog_generation == read.generation()
                },
            )
            .await?)
    }

    /// One explicit host selection, before the first provider request. Failed
    /// preparation cannot be retried through a second call on the same run.
    /// The durable owner separately verifies the accepted choice and project.
    pub async fn host_prompt(
        &mut self,
        choice: &polaris_desktop_protocol::request::McpPromptChoice,
        cancelled: impl Future<Output = ()>,
    ) -> Result<McpPromptResult, Error> {
        self.active()?;
        if self.prompt != PromptState::Unselected || self.request_generation != 0 {
            return Err(super::Error::SelectionStale.into());
        }
        self.prompt = PromptState::Unconfirmed;
        tokio::pin!(cancelled);
        // Releases the shared metadata permit before requesting the send lane.
        self.ensure_catalog(&choice.server, &mut cancelled).await?;
        self.active()?;
        let server = self.config.server(&choice.server)?;
        if !server.capabilities.prompt_selection {
            return Err(polaris_mcp::Error::Auth.into());
        }
        let destination = server.destination_hash()?;
        let secret_revision = self.config.secrets.as_ref().map(|s| s.revision());
        let entry = self
            .connections
            .iter_mut()
            .find(|entry| {
                entry.phase == Phase::Ready && entry.connection.server_id() == choice.server
            })
            .ok_or(polaris_mcp::Error::Protocol)?;
        let generation = entry
            .connection
            .catalog(self.config.registry.revision)?
            .generation
            .clone();
        let operation = entry.connection.host_select_prompt(
            self.config.registry.revision,
            &generation,
            &choice.prompt,
            serde_json::Value::Object(choice.arguments.clone()),
        )?;
        let result = self
            .port
            .host_prompt(
                &mut entry.connection,
                operation,
                self.config.project.clone(),
                &mut self.results,
                cancelled,
                |binding| {
                    self.config
                        .server(&choice.server)
                        .is_ok_and(|server| server.capabilities.prompt_selection)
                        && self.config.secrets.as_ref().map(|s| s.revision()) == secret_revision
                        && binding.registry_revision == self.config.registry.revision
                        && binding.destination_hash == destination
                        && binding.item.server_id == choice.server
                        && binding.item.key == choice.prompt
                        && binding.catalog_generation == generation
                },
            )
            .await?;
        if result.permits_continuation() {
            self.prompt = PromptState::Confirmed;
        }
        Ok(result)
    }

    pub fn chunk(&mut self, handle: &str, offset: usize, limit: usize) -> Result<Chunk, Error> {
        self.active()?;
        Ok(self
            .results
            .chunk(self.port.binding(), handle, offset, limit, Instant::now())?)
    }

    /// Serial control traffic; failure is retained and never called success.
    /// A cancelled shutdown can be awaited again with the same owned entries.
    /// After a failed shutdown, drop this owner on its live Tokio runtime:
    /// ManagedStdio's PendingCleanup parks unreaped children in the existing
    /// sandbox registry, which the desktop CleanupOwner drains after worker join.
    /// Dropping/parking does not turn StopUnconfirmed into successful shutdown.
    pub async fn shutdown(&mut self) -> Result<(), polaris_mcp::Error> {
        self.closed = true;
        self.results.finish();
        self.pending = PendingSelection::default();
        self.request_servers.clear();
        let mut failure = None;
        for entry in &mut self.connections {
            entry.phase = Phase::Closing;
            if let Err(error) = entry.connection.shutdown().await {
                failure = Some(error);
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        self.connections.clear();
        Ok(())
    }
}

#[cfg(test)]
#[path = "runtime/tests/fixture.rs"]
pub(crate) mod test_fixture;
#[cfg(test)]
mod tests;
