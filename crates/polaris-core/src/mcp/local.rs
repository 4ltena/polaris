//! CLI and TUI hosts: the same store, startup admission and setup tool as the
//! desktop owner, with Gate approvals and session-history results.
use crate::{
    execution_owner::McpPort,
    isolated_workspace::Limits,
    mcp::{
        operation::ExternalCeiling,
        runtime::{RunRuntime, RuntimeConfig},
        setup::{Authorization, SetupTool},
    },
};
use polaris_mcp::{Error, store::Store};
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

const RUNTIME_ROOTS: &[&str] = &["/bin", "/usr/bin", "/usr/lib", "/System/Library"];
#[cfg(test)]
mod tests;
const LIMITS: Limits = Limits {
    max_entries: 100_000,
    max_files: 50_000,
    max_file_bytes: 32 * 1024 * 1024,
    max_total_bytes: 512 * 1024 * 1024,
    max_depth: 64,
};

/// Servers left out of a run, with their admission failure.
pub type Quarantined = BTreeMap<String, Error>;

/// One registered server, as a terminal may display it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerState {
    pub server_id: String,
    /// Registered globally but not enabled for this project is the common
    /// reason a server's tools never appear.
    pub enabled_here: bool,
    /// `None` when admission accepted it. Admission happens without
    /// connecting, so this is not "the server is down".
    pub quarantine: Option<String>,
}

pub struct LocalHost {
    store: Option<Store>,
    home: PathBuf,
    root: PathBuf,
    project: String,
    runtime_roots: Vec<PathBuf>,
    excluded_roots: Vec<PathBuf>,
}

impl LocalHost {
    /// Read at root-run start and bind to that run's admitted registry snapshot.
    /// A missing or damaged index disables only preselection.
    /// `decider` is the typed judge, and registering one is optional: a run
    /// without it keeps the two-stage path it always had.
    pub fn configure_preselection(
        &self,
        runtime: &mut RunRuntime,
        selector: Option<std::sync::Arc<dyn polaris_provider::Provider>>,
        decider: Option<std::sync::Arc<dyn polaris_provider::decision::Decider>>,
    ) {
        let index = (|| {
            let opened;
            let store = match &self.store {
                Some(store) => store,
                None if Store::exists(&self.home) => {
                    opened = Store::open(&self.home)?;
                    &opened
                }
                None => return Ok(None),
            };
            store
                .load_catalog_index()
                .map(|index| index.map(|index| index.verified(runtime.config().registry())))
        })()
        .map_err(super::preselection::index_status);
        runtime.set_local_preselection(super::preselection::Context {
            index,
            selector,
            decider,
        });
    }

    /// The account home and the project root the CLI sandbox also writes to.
    /// That root and Polaris state and session trees are excluded from stdio
    /// servers. A run never creates the store; `create_store` is for explicit
    /// MCP commands.
    pub fn open(cwd: &Path, create_store: bool) -> Result<Self, Error> {
        let canonical = |path: &Path| path.canonicalize().map_err(|_| Error::Protection);
        let home = canonical(
            &std::env::var_os("HOME")
                .map(PathBuf::from)
                .ok_or(Error::Protection)?,
        )?;
        let root = canonical(&crate::project::resolve_root(cwd))?;
        let storage = crate::project::storage_root_from_env().map_err(|_| Error::Protection)?;
        let storage = storage.canonicalize().unwrap_or(storage);
        let store = if create_store || Store::exists(&home) {
            Some(Store::open(&home)?)
        } else {
            None
        };
        // A source that cannot be registered is not a regular file and is never read.
        let _ = super::setup::protect_sources(&home);
        Ok(Self {
            store,
            project: crate::project::project_id(&root),
            root: root.clone(),
            home,
            runtime_roots: RUNTIME_ROOTS.iter().map(PathBuf::from).collect(),
            excluded_roots: vec![
                root,
                storage.join("state"),
                storage.join("sessions"),
                storage.join("sessions-v2"),
            ],
        })
    }

    /// Registrations name this identifier in `enabled_projects`.
    pub fn project(&self) -> &str {
        &self.project
    }

    /// A root at or above home contains every server directory, so no stdio
    /// server can run there.
    pub fn root_contains_home(&self) -> bool {
        self.home.starts_with(&self.root)
    }

    /// Present when the store exists or `open` was asked to create it.
    pub fn store(&self) -> Option<&Store> {
        self.store.as_ref()
    }

    /// One run's runtime, or `None` when no server is enabled for the project.
    /// Servers that fail admission are returned with their reason and left out.
    pub fn runtime(
        &self,
        session: &str,
        run: &str,
        ceiling: ExternalCeiling,
    ) -> Result<Option<(RunRuntime, Quarantined)>, Error> {
        // A store created after `open` (a registration in an earlier turn) counts.
        let opened;
        let store = match &self.store {
            Some(store) => store,
            None if Store::exists(&self.home) => {
                opened = Store::open(&self.home)?;
                &opened
            }
            None => return Ok(None),
        };
        let Some(config) = RuntimeConfig::prepare_registered(
            store.load_snapshot()?,
            self.project.clone(),
            &self.home,
            &self.runtime_roots,
            &self.excluded_roots,
            LIMITS,
        )?
        else {
            return Ok(None);
        };
        let quarantined = config.quarantined().clone();
        let runtime = RunRuntime::new(config, McpPort::local(session, run, "1", ceiling))
            .map_err(|_| Error::InvalidRegistration)?;
        Ok(Some((runtime, quarantined)))
    }

    /// What a terminal can show about MCP without connecting to anything.
    ///
    /// Every registered server appears, including the ones this project has
    /// not enabled and the ones admission refused: a server that is missing
    /// from the list for an unstated reason is the thing this is for. No
    /// connection is opened and no model is asked; the state shown is the
    /// registration and the admission, not a live catalogue.
    pub fn state_report(&self, ceiling: ExternalCeiling) -> Result<Vec<ServerState>, Error> {
        let opened;
        let store = match &self.store {
            Some(store) => store,
            None if Store::exists(&self.home) => {
                opened = Store::open(&self.home)?;
                &opened
            }
            None => return Ok(Vec::new()),
        };
        let registered = store.load_snapshot()?;
        let quarantined = self
            .runtime("report", "report", ceiling)?
            .map(|(_, quarantined)| quarantined)
            .unwrap_or_default();
        // No registry is a terminal with nothing registered, not an error.
        let Some(registry) = registered.registry else {
            return Ok(Vec::new());
        };
        Ok(registry
            .servers
            .iter()
            .map(|server| ServerState {
                server_id: server.server_id.clone(),
                enabled_here: server.enabled_projects.contains(&self.project),
                // The reason is the admission's own, not prose about it:
                // a terminal that paraphrases why a server was refused
                // will eventually paraphrase it wrongly.
                quarantine: quarantined.get(&server.server_id).map(ToString::to_string),
            })
            .collect())
    }

    /// The admission a run would apply, for a server saved from the terminal.
    pub fn validate(&self, server: &polaris_mcp::registration::Server) -> Result<(), Error> {
        RuntimeConfig::validate_servers(
            [server],
            &self.project,
            &self.home,
            &self.runtime_roots,
            &self.excluded_roots,
            LIMITS,
        )
    }

    pub fn setup_tool(&self, ceiling: ExternalCeiling) -> SetupTool {
        SetupTool::new(
            self.home.clone(),
            self.runtime_roots.clone(),
            self.excluded_roots.clone(),
            LIMITS,
            self.project.clone(),
            Authorization::Local(ceiling),
        )
    }
}

#[cfg(test)]
mod state_report_tests {
    use super::*;

    fn ceiling() -> ExternalCeiling {
        ExternalCeiling::host_external(false)
    }

    /// The registry lives under `HOME`, so "nothing registered" has to be
    /// **built** here. `LocalHost::open` resolves the real one, which made
    /// this pass only on a machine whose owner had never registered an MCP
    /// server — observed on 2026-09-18, when registering three turned it into
    /// a failure. The host is assembled the way `local/tests.rs::fixture`
    /// does, minus the registry.
    #[test]
    fn nothing_registered_is_an_empty_report_rather_than_an_error() {
        let temp = tempfile::tempdir().expect("a temporary home");
        let home = temp.path().canonicalize().expect("a real path");
        let host = LocalHost {
            store: Some(polaris_mcp::store::Store::open(&home).expect("a store")),
            home: home.clone(),
            root: home.join("project"),
            project: "project".into(),
            runtime_roots: vec![],
            excluded_roots: vec![],
        };
        assert_eq!(
            host.state_report(ceiling()).expect("a report"),
            Vec::new(),
            "a terminal with nothing registered has nothing to explain"
        );
    }
}
