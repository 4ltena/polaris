//! Private SDK boundary. Neither upstream errors nor credential-bearing clients
//! implement a host-visible Debug interface.

mod catalog;
mod failure;
mod handler;
mod http;
mod line_reader;
mod operation;
mod sensitive;

pub use operation::{BoundOperation, OperationBinding, OperationFailure, OperationProgress};

const MAX_MESSAGE_BYTES: usize = 4 * 1024 * 1024;

use crate::{
    Error,
    registration::{Destination, ProtocolProfile, Server},
    store::Secrets,
};
use rmcp::{
    ClientCacheConfig, ClientLifecycleMode, ClientServiceExt, RoleClient,
    model::ProtocolVersion,
    service::RunningService,
    transport::{
        common::client_side_sse::NeverRetry,
        streamable_http_client::{
            StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
        },
    },
};
use std::{future::Future, sync::Arc, time::Duration};

enum Prepared {
    Http(StreamableHttpClientTransport<http::HttpClient>),
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    Stdio(
        line_reader::LineReader<tokio::process::ChildStdout>,
        tokio::process::ChildStdin,
    ),
}

/// A single run owns this connection, including while initialize is pending.
/// Preparation is not an operation grant. Retain the owner outside model
/// Futures and explicitly complete shutdown before reporting run readiness.
pub struct Connection {
    prepared: Option<Prepared>,
    service: Option<RunningService<RoleClient, handler::Handler>>,
    handler: handler::Handler,
    server: Server,
    connection_generation: String,
    registry_revision: Option<u64>,
    catalog_refresh: u64,
    catalog_snapshot: Option<catalog::Snapshot>,
    closing: Option<tokio::task::JoinHandle<Result<(), Error>>>,
    profile: ProtocolProfile,
    connect_timeout: Duration,
    stopped: bool,
    shutdown_failed: bool,
    sensitive: sensitive::SensitiveValues,
    failures: failure::Failures,
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    process: Option<polaris_sandbox::ManagedStdio>,
}

impl Connection {
    pub fn server_id(&self) -> &str {
        &self.server.server_id
    }

    /// `authorized` rechecks the registration, destination and run policy.
    /// For stdio the sandbox profile must already have been resolved by the host.
    pub fn prepare<'a>(
        server: &Server,
        project: &str,
        secrets: impl Into<Option<&'a Secrets>>,
        policy: Option<&polaris_sandbox::SandboxPolicy>,
        mut authorized: impl FnMut() -> bool,
    ) -> Result<Self, Error> {
        let secrets = secrets.into();
        server.validate()?;
        if !server.enabled_projects.contains(project) {
            return Err(Error::Auth);
        }
        let connect_timeout = Duration::from_secs(server.timeouts.connect_seconds.into());
        let connection_generation =
            polaris_auth::pkce::random_urlsafe(24).map_err(|_| Error::Protection)?;
        let references = match &server.transport {
            Destination::StreamableHttp { secret_headers, .. } => secret_headers,
            Destination::Stdio { secret_env, .. } => secret_env,
        };
        let sensitive = sensitive::SensitiveValues::new(
            references
                .values()
                .map(|reference| {
                    secrets
                        .ok_or(Error::Auth)?
                        .resolve(reference)
                        .map(str::to_owned)
                })
                .collect::<Result<Vec<_>, _>>()?,
        );
        let failures = failure::Failures::default();
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        let mut process = None;
        let prepared = match &server.transport {
            Destination::StreamableHttp {
                url,
                secret_headers,
            } => {
                let client = http::HttpClient::new(url, secret_headers, secrets, connect_timeout)?
                    .with_failures(failures.clone())
                    .with_request_timeout(Duration::from_secs(
                        server.timeouts.operation_seconds.into(),
                    ));
                let config = http_config(url);
                if !authorized() {
                    return Err(Error::Cancelled);
                }
                Prepared::Http(StreamableHttpClientTransport::with_client(client, config))
            }
            Destination::Stdio {
                command,
                args,
                cwd,
                secret_env,
                sandbox_profile,
                ..
            } => {
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                {
                    let policy = policy.ok_or(Error::Protection)?;
                    let registered = policy.isolated_boundary().map(|boundary| {
                        if boundary.loopback_network {
                            "loopback"
                        } else {
                            policy.mode().as_str()
                        }
                    });
                    if registered != Some(sandbox_profile.as_str())
                        || policy
                            .isolated_boundary()
                            .is_none_or(|boundary| boundary.environment.is_none())
                    {
                        return Err(Error::Protection);
                    }
                    let environment = secret_env
                        .iter()
                        .map(|(name, reference)| {
                            Ok((
                                name.clone(),
                                secrets.ok_or(Error::Auth)?.resolve(reference)?.to_owned(),
                            ))
                        })
                        .collect::<Result<std::collections::BTreeMap<_, _>, Error>>()?;
                    let sensitive = sensitive::SensitiveValues::new(environment.values().cloned());
                    let (owner, pipes) = polaris_sandbox::ManagedStdio::spawn_authorized(
                        policy,
                        command,
                        args,
                        cwd,
                        &environment,
                        &mut authorized,
                    )
                    .map_err(|_| Error::Protection)?
                    .ok_or(Error::Cancelled)?;
                    process = Some(owner);
                    // If pipe conversion fails, the local owner kills/parks the
                    // child instead of leaking an unowned server.
                    let stdout = tokio::process::ChildStdout::from_std(pipes.stdout)
                        .map_err(|_| Error::Transport)?;
                    let stdin = tokio::process::ChildStdin::from_std(pipes.stdin)
                        .map_err(|_| Error::Transport)?;
                    Prepared::Stdio(
                        line_reader::LineReader::new(stdout)
                            .with_secrets(sensitive)
                            .with_failures(failures.clone()),
                        stdin,
                    )
                }
                #[cfg(not(any(target_os = "macos", target_os = "linux")))]
                {
                    let _ = (policy, command, args, cwd, secret_env);
                    return Err(Error::Protection);
                }
            }
        };
        Ok(Self {
            prepared: Some(prepared),
            service: None,
            handler: handler::Handler::default(),
            server: server.clone(),
            connection_generation,
            registry_revision: None,
            catalog_refresh: 0,
            catalog_snapshot: None,
            closing: None,
            profile: server.protocol_profile,
            connect_timeout,
            stopped: false,
            shutdown_failed: false,
            sensitive,
            failures,
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            process,
        })
    }

    /// Call once. Failure never creates a replacement connection or replays a
    /// request. The caller may start a separately authorized new connection.
    pub async fn initialize(
        &mut self,
        cancelled: impl Future<Output = ()>,
        mut authorized: impl FnMut() -> bool,
    ) -> Result<(), Error> {
        if self.stopped || !authorized() {
            self.request_stop();
            return Err(Error::Cancelled);
        }
        let prepared = self.prepared.take().ok_or(Error::Protocol)?;
        let lifecycle = lifecycle(self.profile);
        let handler = self.handler.clone();
        let connect = async move {
            match prepared {
                Prepared::Http(transport) => {
                    handler.serve_with_lifecycle(transport, lifecycle).await
                }
                #[cfg(any(target_os = "macos", target_os = "linux"))]
                Prepared::Stdio(stdout, stdin) => {
                    handler
                        .serve_with_lifecycle((stdout, stdin), lifecycle)
                        .await
                }
            }
        };
        tokio::pin!(connect, cancelled);
        let deadline = tokio::time::sleep(self.connect_timeout);
        tokio::pin!(deadline);
        let mut owner_guard = InitializingOwner {
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            process: &mut self.process,
            _lifetime: std::marker::PhantomData,
            armed: true,
        };
        let result = loop {
            tokio::select! {
                biased;
                _ = &mut cancelled => break Err(Error::Cancelled),
                _ = &mut deadline => break Err(Error::Timeout),
                result = &mut connect => break result.map_err(failure::initialize),
                failed = owner_guard.wait() => {
                    if failed { break Err(Error::Transport); }
                }
            }
        };
        owner_guard.armed = false;
        drop(owner_guard);
        match result {
            Ok(service) => {
                let valid = service.peer_info().is_some_and(|info| {
                    let protocol = &info.protocol_version;
                    *protocol == ProtocolVersion::V_2026_07_28
                        || (self.profile != ProtocolProfile::Discover2026
                            && *protocol == ProtocolVersion::V_2025_11_25)
                        || (self.profile == ProtocolProfile::Auto2024Compat
                            && *protocol == ProtocolVersion::V_2024_11_05)
                });
                self.service = Some(service);
                if !valid {
                    self.request_stop();
                    return Err(Error::Protocol);
                }
                self.service
                    .as_ref()
                    .expect("connected")
                    .set_response_cache_config(ClientCacheConfig::disabled())
                    .await;
                Ok(())
            }
            Err(error) => {
                let error = self.failures.resolve(error);
                self.request_stop();
                Err(error)
            }
        }
    }

    pub fn is_connected(&self) -> bool {
        !self.stopped && self.service.as_ref().is_some_and(|s| !s.is_closed())
    }

    /// One nonblocking lifecycle check for synchronous run owners. Async
    /// owners should await wait_process so small pipe buffers cannot stall RPCs.
    pub fn poll_process(&mut self) -> Result<(), Error> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(process) = &mut self.process
            && (process.poll().is_some() || process.has_io_problem())
        {
            self.request_stop();
            return Err(Error::Transport);
        }
        Ok(())
    }

    /// Drive stderr readiness and lifecycle checks while an RPC is pending.
    /// Cancellation of the wait leaves the process owned by this connection.
    pub async fn wait_process(&mut self) -> Result<(), Error> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(process) = &mut self.process {
            if process.wait_and_poll().await.is_some() || process.has_io_problem() {
                self.request_stop();
                return Err(Error::Transport);
            }
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        Ok(())
    }

    #[cfg(any(target_os = "macos", target_os = "linux"))]
    pub fn take_pending_cleanup(&mut self) -> Option<polaris_sandbox::PendingCleanup> {
        self.process.as_mut().and_then(|p| p.take_pending())
    }

    /// Cancellation is latched and cleanup remains owned even if shutdown's
    /// caller cancels its wait. This method requires the connection's runtime.
    pub fn request_stop(&mut self) {
        self.stopped = true;
        self.catalog_snapshot = None;
        self.prepared = None;
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(process) = &mut self.process {
            process.request_stop();
        }
        if let Some(service) = self.service.take() {
            self.closing = Some(tokio::spawn(async move {
                service
                    .cancel()
                    .await
                    .map(|_| ())
                    .map_err(|_| Error::StopUnconfirmed)
            }));
        }
    }

    pub async fn shutdown(&mut self) -> Result<(), Error> {
        self.request_stop();
        if self.shutdown_failed {
            return Err(Error::StopUnconfirmed);
        }
        let deadline = tokio::time::Instant::now() + Duration::from_secs(3);
        loop {
            let mut process_done = true;
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            if let Some(process) = &mut self.process {
                match process.wait_and_poll().await {
                    Some(polaris_sandbox::ControlledEnd::StopUnconfirmed) => {
                        return Err(Error::StopUnconfirmed);
                    }
                    None => process_done = false,
                    Some(_) => {}
                }
            }
            if self.closing.as_ref().is_some_and(|task| task.is_finished())
                && !matches!(self.closing.take().expect("closing").await, Ok(Ok(())))
            {
                self.shutdown_failed = true;
                return Err(Error::StopUnconfirmed);
            }
            if process_done && self.closing.is_none() {
                return Ok(());
            }
            if tokio::time::Instant::now() >= deadline {
                return Err(Error::StopUnconfirmed);
            }
            // Active process waits already include a lifecycle tick. Once it
            // is reaped, only the independent SDK closing task can remain.
            if process_done {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        }
    }
}

struct InitializingOwner<'a> {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    process: &'a mut Option<polaris_sandbox::ManagedStdio>,
    _lifetime: std::marker::PhantomData<&'a ()>,
    armed: bool,
}
impl InitializingOwner<'_> {
    async fn wait(&mut self) -> bool {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(process) = self.process {
            return process.wait_and_poll().await.is_some() || process.has_io_problem();
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
        false
    }
}
impl Drop for InitializingOwner<'_> {
    fn drop(&mut self) {
        if self.armed {
            #[cfg(any(target_os = "macos", target_os = "linux"))]
            if let Some(process) = self.process {
                process.request_stop();
            }
        }
    }
}

fn lifecycle(profile: ProtocolProfile) -> ClientLifecycleMode {
    let preferred_versions = vec![ProtocolVersion::V_2026_07_28];
    match profile {
        ProtocolProfile::Discover2026 => ClientLifecycleMode::Discover { preferred_versions },
        ProtocolProfile::Auto2025Compat => ClientLifecycleMode::Auto {
            preferred_versions,
            legacy_version: Some(ProtocolVersion::V_2025_11_25),
        },
        ProtocolProfile::Auto2024Compat => ClientLifecycleMode::Auto {
            preferred_versions,
            legacy_version: Some(ProtocolVersion::V_2024_11_05),
        },
    }
}

fn http_config(url: &str) -> StreamableHttpClientTransportConfig {
    let mut config = StreamableHttpClientTransportConfig::with_uri(url);
    config.retry_config = Arc::new(NeverRetry::default());
    config.max_concurrent_requests = 1;
    config.channel_buffer_capacity = 1;
    config.reinit_on_expired_session = false;
    config.max_sse_event_size = MAX_MESSAGE_BYTES;
    config
}

#[cfg(test)]
mod tests;
