//! One-server catalog check with no model and no store run record. The job
//! keeps its stdio resources until native cleanup is confirmed.
use super::*;
use polaris_core::mcp::runtime::{Error as RuntimeError, RunRuntime, RuntimeConfig};
use polaris_desktop_protocol::mcp_settings::{
    McpCatalogCancelState, McpCatalogCancelled, McpCatalogChecked, McpCatalogFailure,
    McpCatalogOutcome, McpCatalogTarget,
};
use polaris_mcp::catalog::index::ServerIndex;

type Outcome = Result<
    (
        Result<ServerIndex, RuntimeError>,
        Result<(), polaris_mcp::Error>,
        RuntimeConfig,
    ),
    McpCatalogFailure,
>;

pub(crate) struct CatalogJob {
    request: Request,
    server_id: String,
    worker: Option<std::thread::JoinHandle<Outcome>>,
    cancel: Option<tokio::sync::oneshot::Sender<()>>,
    execution: crate::execution::RunExecution,
    // Held only so stdio scratch outlives unconfirmed native cleanup.
    resources: Option<RuntimeConfig>,
}

impl CatalogJob {
    fn stop(&mut self) {
        self.execution.cancel();
        if let Some(cancel) = self.cancel.take() {
            let _ = cancel.send(());
        }
    }
}

impl Drop for CatalogJob {
    fn drop(&mut self) {
        // Emergency owner destruction only. Normal loops cancel and poll.
        self.stop();
        if let Some(worker) = self.worker.take()
            && let Err(payload) = worker.join()
        {
            std::mem::forget(payload);
        }
    }
}

fn checked(server_id: &str, outcome: McpCatalogOutcome, tools: usize) -> McpCatalogChecked {
    McpCatalogChecked {
        server_id: server_id.into(),
        outcome,
        tools: DecimalU64::new(tools as u64),
        failure: None,
    }
}

fn failed(server_id: &str, failure: McpCatalogFailure) -> McpCatalogChecked {
    McpCatalogChecked {
        failure: Some(failure),
        ..checked(server_id, McpCatalogOutcome::Failed, 0)
    }
}

fn failure(value: &polaris_mcp::Error) -> McpCatalogFailure {
    use polaris_mcp::Error;
    match value {
        Error::Auth => McpCatalogFailure::Auth,
        Error::Transport | Error::Timeout | Error::Protocol => McpCatalogFailure::Transport,
        Error::Protection => McpCatalogFailure::Protection,
        Error::Limit => McpCatalogFailure::Limit,
        Error::StopUnconfirmed => McpCatalogFailure::StopUnconfirmed,
        _ => McpCatalogFailure::Unavailable,
    }
}

impl Engine {
    /// `Ok(None)` means the job owns the deferred response.
    pub(super) fn start_mcp_catalog(
        &mut self,
        request: &Request,
    ) -> Result<Option<McpCatalogChecked>, ProtocolError> {
        validate_request(self.connection, request).map_err(|_| error(ErrorCode::InvalidRequest))?;
        let RequestBody::McpCatalogCheck(session, target) = &request.body else {
            return Err(error(ErrorCode::InvalidRequest));
        };
        if self.client.as_ref() != Some(&request.client_id) || session != &self.session {
            return Err(error(ErrorCode::PermissionDenied));
        }
        let settings = self
            .mcp_settings
            .as_ref()
            .ok_or_else(|| error(ErrorCode::CapabilityUnavailable))?;
        if self.failed {
            return Err(error(ErrorCode::StorageFailed));
        }
        if self.draining
            || self.disconnected.load(Ordering::Acquire)
            || self.active.is_some()
            || self.mcp_catalog.is_some()
            || self.mcp_setup.job.is_some()
        {
            return Err(error(ErrorCode::SessionBusy));
        }
        let server_id = target.server_id.clone();
        let mut snapshot = match settings.store.load_snapshot() {
            Ok(snapshot) => snapshot,
            Err(e) => return Ok(Some(failed(&server_id, failure(&e)))),
        };
        let project = self
            .store
            .snapshot()
            .map_err(store_error)?
            .marker
            .project_id
            .as_str()
            .to_owned();
        // Prepare only the target, so an unrelated server cannot fail its check.
        if let Some(registry) = &mut snapshot.registry {
            registry.servers.retain(|server| {
                server.server_id == server_id && server.enabled_projects.contains(&project)
            });
        }
        if snapshot
            .registry
            .as_ref()
            .is_none_or(|registry| registry.servers.is_empty())
        {
            return Ok(Some(failed(&server_id, McpCatalogFailure::Unavailable)));
        }
        let (home, runtime_roots, excluded_roots, limits) = (
            settings.home.clone(),
            settings.runtime_roots.clone(),
            settings.excluded_roots.clone(),
            settings.limits,
        );
        let run = RunTarget {
            run_id: RunId::new("mcp-catalog").map_err(|_| error(ErrorCode::InvalidRequest))?,
            attempt_id: AttemptId::new("check").map_err(|_| error(ErrorCode::InvalidRequest))?,
        };
        let mut execution = crate::execution::RunExecution::new(&run, self.disconnected.clone());
        let port = execution
            .configure_mcp(
                &run,
                self.session.clone(),
                polaris_core::mcp::operation::ExternalCeiling::Denied,
                polaris_core::approval::ApprovalPolicy::Never,
            )
            .map_err(|_| error(ErrorCode::CapabilityUnavailable))?;
        let worker_server = server_id.clone();
        let (cancel, cancelled) = tokio::sync::oneshot::channel();
        let worker = std::thread::Builder::new()
            .name("polaris-mcp-catalog".into())
            .spawn(move || {
                // Root walks and pinning stay off the serve loop.
                let config = match RuntimeConfig::prepare_registered(
                    snapshot,
                    project,
                    &home,
                    &runtime_roots,
                    &excluded_roots,
                    limits,
                ) {
                    Ok(Some(config)) if config.server_ids().contains(&worker_server.as_str()) => {
                        config
                    }
                    Ok(Some(config)) => {
                        return Err(config
                            .quarantined()
                            .get(&worker_server)
                            .map_or(McpCatalogFailure::Unavailable, failure));
                    }
                    Ok(None) => return Err(McpCatalogFailure::Unavailable),
                    Err(e) => return Err(failure(&e)),
                };
                let Ok(runtime) = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                else {
                    return Ok((Err(polaris_mcp::Error::Transport.into()), Ok(()), config));
                };
                let worker_config = config.clone();
                let (entry, shutdown) = runtime.block_on(async move {
                    let mut mcp = match RunRuntime::new(worker_config, port) {
                        Ok(mcp) => mcp,
                        Err(e) => return (Err(e), Ok(())),
                    };
                    let entry = tokio::select! {
                        biased;
                        _ = cancelled => Err(polaris_mcp::Error::Cancelled.into()),
                        entry = mcp.index_entry(&worker_server, std::future::pending()) => entry,
                    };
                    // Shut down on this live runtime; failures park native cleanup.
                    let shutdown = mcp.shutdown().await;
                    (entry, shutdown)
                });
                Ok((entry, shutdown, config))
            })
            .map_err(|_| error(ErrorCode::CapabilityUnavailable))?;
        self.mcp_catalog = Some(CatalogJob {
            request: request.clone(),
            server_id,
            worker: Some(worker),
            cancel: Some(cancel),
            execution,
            resources: None,
        });
        Ok(None)
    }

    pub(super) fn cancel_mcp_catalog(&mut self, target: &McpCatalogTarget) -> McpCatalogCancelled {
        let state = match &mut self.mcp_catalog {
            Some(job) if job.server_id == target.server_id && job.worker.is_some() => {
                job.stop();
                McpCatalogCancelState::Cancelling
            }
            _ => McpCatalogCancelState::NotRunning,
        };
        McpCatalogCancelled {
            server_id: target.server_id.clone(),
            state,
        }
    }

    /// Answers once when the worker ends, then keeps the job until cleanup is
    /// confirmed. Stopping suppresses the answer, as for local inventory.
    pub(super) fn finish_mcp_catalog(&mut self, stopping: bool) -> Option<Response> {
        let mut job = self.mcp_catalog.take()?;
        let suppress =
            stopping || self.draining || self.failed || self.disconnected.load(Ordering::Acquire);
        if suppress {
            job.stop();
        }
        let mut response = None;
        if job.worker.as_ref().is_some_and(|w| w.is_finished()) {
            let joined = job.worker.take().expect("finished worker").join();
            let result = match joined {
                Ok(Err(reason)) => failed(&job.server_id, reason),
                Ok(Ok((entry, shutdown, config))) => {
                    job.resources = Some(config);
                    match (entry, shutdown) {
                        (Ok(entry), Ok(())) => match self.mcp_settings.as_ref().map(|settings| {
                            settings.store.save_catalog_entry(&job.server_id, entry)
                        }) {
                            Some(Ok((tools, true))) => {
                                checked(&job.server_id, McpCatalogOutcome::Partial, tools)
                            }
                            Some(Ok((tools, false))) => {
                                checked(&job.server_id, McpCatalogOutcome::Checked, tools)
                            }
                            _ => failed(&job.server_id, McpCatalogFailure::Storage),
                        },
                        (
                            Err(
                                RuntimeError::Transport(polaris_mcp::Error::Cancelled)
                                | RuntimeError::Owner(
                                    polaris_core::execution_owner::McpCallError::Cancelled,
                                ),
                            ),
                            Ok(()),
                        ) => checked(&job.server_id, McpCatalogOutcome::Cancelled, 0),
                        (_, Err(e)) | (Err(RuntimeError::Transport(e)), Ok(())) => {
                            failed(&job.server_id, failure(&e))
                        }
                        _ => failed(&job.server_id, McpCatalogFailure::Unavailable),
                    }
                }
                Err(payload) => {
                    std::mem::forget(payload);
                    failed(&job.server_id, McpCatalogFailure::Unavailable)
                }
            };
            if !suppress {
                response =
                    Response::for_request(&job.request, Ok(SuccessResult::McpCatalogCheck(result)))
                        .ok();
            }
        }
        // Drain parked native cleanup first; an unconfirmed child keeps its scratch.
        self.cleanups.step();
        if job.worker.is_some() || !self.cleanups.is_empty() {
            self.mcp_catalog = Some(job);
        }
        response
    }

    pub(super) fn poll_mcp_catalog(
        &mut self,
        out: Option<&Outbox>,
        stopping: bool,
    ) -> Result<(), ServiceError> {
        if let Some(response) = self.finish_mcp_catalog(stopping)
            && !self.disconnected.load(Ordering::Acquire)
            && let Some(out) = out
        {
            out.send(&response)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod cleanup_tests;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::mcp_fixture as mcp_server;
    use polaris_desktop_protocol::ids::ClientId;
    use std::time::{Duration, Instant};

    pub(super) fn engine(
        home: &std::path::Path,
        server: polaris_mcp::registration::Server,
    ) -> (PrototypeRoot, Engine) {
        let root = PrototypeRoot::new().unwrap();
        let mut engine = Engine::create(&root, Options::default()).unwrap();
        engine.connection = ConnectionState::Ready;
        engine.client = Some(ClientId::new("client").unwrap());
        let store = polaris_mcp::store::Store::open(home).unwrap();
        let mut server = server;
        server.enabled_projects = [project_id().as_str().to_owned()].into();
        store
            .save_registry(
                0,
                &polaris_mcp::registration::Registry {
                    schema_version: 1,
                    revision: 1,
                    servers: vec![server],
                },
            )
            .unwrap();
        engine.mcp_settings = Some(mcp_settings::McpSettings {
            store,
            home: home.to_owned(),
            active_registry_revision: 1,
            active_secrets_revision: 0,
            runtime_roots: vec![],
            excluded_roots: vec![],
            limits: polaris_core::isolated_workspace::Limits {
                max_entries: 8,
                max_files: 4,
                max_file_bytes: 1024,
                max_total_bytes: 4096,
                max_depth: 2,
            },
        });
        (root, engine)
    }

    pub(super) fn check(engine: &Engine, server: &str) -> Request {
        Request {
            protocol_version: Default::default(),
            client_id: ClientId::new("client").unwrap(),
            request_id: RequestId::new("check").unwrap(),
            body: RequestBody::McpCatalogCheck(
                engine.session.clone(),
                McpCatalogTarget {
                    server_id: server.into(),
                },
            ),
        }
    }

    fn answer(engine: &mut Engine) -> McpCatalogChecked {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(response) = engine.finish_mcp_catalog(false) {
                let Ok(SuccessResult::McpCatalogCheck(result)) = response.outcome else {
                    panic!("catalog response");
                };
                return result;
            }
            assert!(Instant::now() < deadline, "catalog job did not answer");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn check_saves_one_server_index_and_blocks_runs_until_it_ends() {
        let (server, methods, fixture) = mcp_server::server_fixture("").await;
        let temp = tempfile::tempdir().unwrap();
        let (_root, mut engine) = engine(&temp.path().canonicalize().unwrap(), server);
        let before = engine.store.snapshot().unwrap();
        assert!(
            engine
                .start_mcp_catalog(&check(&engine, "fixture"))
                .unwrap()
                .is_none()
        );
        assert_eq!(
            engine
                .start_mcp_catalog(&check(&engine, "fixture"))
                .unwrap_err()
                .code,
            ErrorCode::SessionBusy
        );
        let run = Request {
            body: RequestBody::RunStart(
                engine.session.clone(),
                polaris_desktop_protocol::request::RunStart {
                    expected_draft_revision: DecimalU64::new(0),
                    expected_configuration_revision: DecimalU64::new(0),
                    expected_policy_revision: DecimalU64::new(0),
                    mcp_prompt: None,
                },
            ),
            ..check(&engine, "fixture")
        };
        assert_eq!(
            engine.handle(&run, &mut vec![]).unwrap_err().code,
            ErrorCode::SessionBusy
        );
        let result = answer(&mut engine);
        assert_eq!(result, checked("fixture", McpCatalogOutcome::Checked, 1));
        let settings = engine.mcp_settings.as_ref().unwrap();
        let registry = settings.store.load_registry().unwrap().unwrap();
        let index = settings.store.load_catalog_index().unwrap().unwrap();
        assert_eq!(index.verified(&registry)["fixture"].tools[0].key, "fixture");
        let read = Request {
            body: RequestBody::McpRegistryRead(engine.session.clone()),
            ..check(&engine, "fixture")
        };
        let Ok(SuccessResult::McpRegistryRead(state)) = engine.mcp_settings_request(&read) else {
            panic!("registry read");
        };
        assert!(state.unchecked_servers.is_empty());
        let deadline = Instant::now() + Duration::from_secs(5);
        while engine.mcp_catalog.is_some() {
            engine.cleanups.step();
            assert!(engine.finish_mcp_catalog(false).is_none());
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
        let after = engine.store.snapshot().unwrap();
        assert_eq!(after.marker, before.marker);
        assert!(after.state.runs.is_empty());
        assert!(!methods.lock().unwrap().iter().any(|m| m == "tools/call"));
        fixture.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn active_runs_refuse_checks_and_draining_suppresses_the_answer() {
        let (server, _methods, fixture) = mcp_server::server_fixture("tools/list").await;
        let temp = tempfile::tempdir().unwrap();
        let (_root, mut engine) = engine(&temp.path().canonicalize().unwrap(), server);
        let start = Request {
            body: RequestBody::RunStart(
                engine.session.clone(),
                polaris_desktop_protocol::request::RunStart {
                    expected_draft_revision: DecimalU64::new(0),
                    expected_configuration_revision: DecimalU64::new(0),
                    expected_policy_revision: DecimalU64::new(0),
                    mcp_prompt: None,
                },
            ),
            ..check(&engine, "fixture")
        };
        engine.handle(&start, &mut vec![]).unwrap();
        assert!(engine.active.is_some());
        assert_eq!(
            engine
                .start_mcp_catalog(&check(&engine, "fixture"))
                .unwrap_err()
                .code,
            ErrorCode::SessionBusy
        );
        let other = tempfile::tempdir().unwrap();
        let (delayed, _, delayed_task) = mcp_server::server_fixture("tools/list").await;
        let (_root, mut engine) = self::engine(&other.path().canonicalize().unwrap(), delayed);
        assert!(
            engine
                .start_mcp_catalog(&check(&engine, "fixture"))
                .unwrap()
                .is_none()
        );
        engine.draining = true;
        let deadline = Instant::now() + Duration::from_secs(10);
        while engine.mcp_catalog.is_some() {
            assert!(engine.finish_mcp_catalog(false).is_none());
            assert!(Instant::now() < deadline, "draining job did not finish");
            std::thread::sleep(Duration::from_millis(5));
        }
        fixture.abort();
        delayed_task.abort();
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn cancel_and_unavailable_servers_save_nothing() {
        let (server, _methods, fixture) = mcp_server::server_fixture("tools/list").await;
        let temp = tempfile::tempdir().unwrap();
        let (_root, mut engine) = engine(&temp.path().canonicalize().unwrap(), server);
        assert_eq!(
            engine
                .start_mcp_catalog(&check(&engine, "missing"))
                .unwrap(),
            Some(failed("missing", McpCatalogFailure::Unavailable))
        );
        assert!(engine.mcp_catalog.is_none());
        assert!(
            engine
                .start_mcp_catalog(&check(&engine, "fixture"))
                .unwrap()
                .is_none()
        );
        std::thread::sleep(Duration::from_millis(200));
        let target = McpCatalogTarget {
            server_id: "fixture".into(),
        };
        assert_eq!(
            engine.cancel_mcp_catalog(&target).state,
            McpCatalogCancelState::Cancelling
        );
        assert_eq!(
            answer(&mut engine),
            checked("fixture", McpCatalogOutcome::Cancelled, 0)
        );
        assert_eq!(
            engine.cancel_mcp_catalog(&target).state,
            McpCatalogCancelState::NotRunning
        );
        let settings = engine.mcp_settings.as_ref().unwrap();
        assert!(settings.store.load_catalog_index().unwrap().is_none());
        fixture.abort();
    }
}
