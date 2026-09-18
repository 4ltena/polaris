//! Serialized metadata acquisition with bounded retries and retained cleanup.
use super::*;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

fn retry_delay(retry: u8) -> Duration {
    let jitter = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |time| time.subsec_nanos() % 51);
    delay_with_jitter(retry, Duration::from_millis(jitter.into()))
}

pub(super) fn delay_with_jitter(retry: u8, jitter: Duration) -> Duration {
    let millis = match retry {
        0 => 250,
        1 => 1000,
        2 => 2000,
        _ => 4000,
    };
    (Duration::from_millis(millis) + jitter.min(Duration::from_millis(50)))
        .min(Duration::from_secs(4))
}

impl RunRuntime {
    /// Only connection/catalog traffic can be retried. An abandoned future or
    /// an exhausted/terminal failure retains its entry for outer shutdown.
    pub async fn ensure_catalog(
        &mut self,
        server_id: &str,
        cancelled: impl Future<Output = ()>,
    ) -> Result<(), Error> {
        self.ensure_catalog_with_delay(server_id, cancelled, retry_delay)
            .await
    }

    pub(super) async fn ensure_catalog_with_delay(
        &mut self,
        server_id: &str,
        cancelled: impl Future<Output = ()>,
        mut delay: impl FnMut(u8) -> Duration,
    ) -> Result<(), Error> {
        self.active()?;
        let server = self.config.server(server_id)?.clone();
        tokio::pin!(cancelled);
        if cancelled.as_mut().now_or_never().is_some() {
            return Err(McpCallError::Cancelled.into());
        }
        // Hold the same ordinary request lane through cleanup and backoff.
        let _permit = self.port.reserve_metadata()?;
        let mut existing = self
            .connections
            .iter()
            .position(|e| e.connection.server_id() == server_id);
        if let Some(i) = existing {
            if self.connections[i].phase != Phase::Ready {
                return Err(polaris_mcp::Error::Protocol.into());
            }
            if self.connections[i]
                .connection
                .catalog(self.config.registry.revision)
                .is_ok()
            {
                return Ok(());
            }
        }
        self.catalog_budget.check_available()?;
        if existing.is_none() && self.connections.len() == 4 {
            let i = self
                .connections
                .iter()
                .position(|e| {
                    e.phase == Phase::Ready
                        && !self.request_servers.contains(e.connection.server_id())
                        && !self.pending.references_server(e.connection.server_id())
                })
                .ok_or(polaris_mcp::Error::Limit)?;
            self.close_metadata_connection(i).await?;
            self.connections.remove(i);
        }

        for attempt in 0..=server.timeouts.connect_retries {
            self.active()?;
            self.catalog_budget.check_available()?;
            if cancelled.as_mut().now_or_never().is_some() {
                return Err(McpCallError::Cancelled.into());
            }
            let (index, fresh) = if let Some(i) = existing.take() {
                (i, false)
            } else {
                let mut resources_changed = false;
                let connection = Connection::prepare(
                    &server,
                    &self.config.project,
                    self.config.secrets.as_deref(),
                    self.config.policies.get(server_id),
                    || {
                        if self.port.stopped() {
                            return false;
                        }
                        resources_changed = self
                            .config
                            .stdio_resources
                            .get(server_id)
                            .is_some_and(|resources| resources.revalidate().is_err());
                        !resources_changed
                    },
                );
                if resources_changed {
                    return Err(polaris_mcp::Error::Protection.into());
                }
                // Preparation errors are not retried: a native setup failure
                // may already have transferred its child to the cleanup owner.
                let connection = connection?;
                self.connections.push(Entry {
                    connection,
                    phase: Phase::Loading,
                });
                (self.connections.len() - 1, true)
            };
            self.connections[index].phase = Phase::Loading;
            let initialized = if fresh {
                self.connections[index].connection.initialize(
                    async { tokio::select! { _ = &mut cancelled => (), _ = self.port.cancelled() => () } },
                    || !self.port.stopped(),
                ).await
            } else {
                Ok(())
            };
            let result = match initialized {
                Ok(()) => self.connections[index].connection.refresh_catalog_with_budget(
                    &mut self.catalog_budget,
                    self.config.registry.revision,
                    async { tokio::select! { _ = &mut cancelled => (), _ = self.port.cancelled() => () } },
                    || !self.port.stopped(),
                ).await.map(|_| ()),
                Err(error) => Err(error),
            };
            match result {
                Ok(()) => {
                    self.connections[index].phase = Phase::Ready;
                    self.view()?;
                    return Ok(());
                }
                Err(error) => {
                    self.connections[index].phase = Phase::Failed;
                    if attempt == server.timeouts.connect_retries
                        || !matches!(
                            error,
                            polaris_mcp::Error::Transport | polaris_mcp::Error::Timeout
                        )
                    {
                        return Err(error.into());
                    }
                    self.close_metadata_connection(index).await?;
                    // Keep Closing in the collection during the cancellable
                    // wait, so abandoning/re-entering cannot reset retries.
                    tokio::select! {
                        biased;
                        _ = &mut cancelled => return Err(McpCallError::Cancelled.into()),
                        _ = self.port.cancelled() => return Err(McpCallError::Cancelled.into()),
                        _ = tokio::time::sleep(delay(attempt).min(Duration::from_secs(4))) => (),
                    }
                    self.active()?;
                    if cancelled.as_mut().now_or_never().is_some() {
                        return Err(McpCallError::Cancelled.into());
                    }
                    self.connections.remove(index);
                }
            }
        }
        unreachable!("bounded attempts always return on the last failure")
    }

    /// Dedicated catalog check: tool summaries only, never a selection or a
    /// grant. The caller still owns shutdown.
    pub async fn index_entry(
        &mut self,
        server_id: &str,
        cancelled: impl Future<Output = ()>,
    ) -> Result<polaris_mcp::catalog::index::ServerIndex, Error> {
        self.ensure_catalog(server_id, cancelled).await?;
        let server = self.config.server(server_id)?;
        let entry = self
            .connections
            .iter()
            .find(|e| e.phase == Phase::Ready && e.connection.server_id() == server_id)
            .ok_or(polaris_mcp::Error::Protocol)?;
        Ok(polaris_mcp::catalog::index::ServerIndex::from_catalog(
            server,
            entry.connection.catalog(self.config.registry.revision)?,
        )?)
    }

    async fn close_metadata_connection(&mut self, index: usize) -> Result<(), Error> {
        self.connections[index].phase = Phase::Closing;
        if let Err(error) = self.connections[index].connection.shutdown().await {
            self.closed = true;
            return Err(error.into());
        }
        Ok(())
    }
}
