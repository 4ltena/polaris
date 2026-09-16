//! Bounded MCP catalog discovery and generation tracking.

use super::*;
use crate::catalog::{Builder, Catalog, Kind};
use rmcp::{Peer, model::PaginatedRequestParams};
use serde_json::{Value, json};
use std::sync::atomic::Ordering;
use tokio::time::Instant;

pub(super) struct Snapshot {
    catalog: Catalog,
    epoch: u64,
    expires: Instant,
}

impl Connection {
    #[cfg(feature = "test-support")]
    pub fn expire_catalog_for_test(&mut self) {
        if let Some(snapshot) = &mut self.catalog_snapshot {
            snapshot.expires = Instant::now();
        }
    }

    /// Checks freshness at every use; holding an older clone is not authority.
    pub fn catalog(&self, registry_revision: u64) -> Result<&Catalog, Error> {
        let snapshot = self.catalog_snapshot.as_ref().ok_or(Error::Protocol)?;
        if !self.is_connected()
            || self.registry_revision != Some(registry_revision)
            || self.handler.epoch.load(Ordering::Acquire) != snapshot.epoch
            || Instant::now() >= snapshot.expires
        {
            return Err(Error::Protocol);
        }
        Ok(&snapshot.catalog)
    }

    /// Read-only catalog acquisition, not permission to call any listed tool.
    /// The run owner serializes requests and caps the sum of server snapshots.
    pub async fn refresh_catalog(
        &mut self,
        registry_revision: u64,
        cancelled: impl Future<Output = ()>,
        authorized: impl FnMut() -> bool,
    ) -> Result<&Catalog, Error> {
        self.refresh_catalog_with_budget(
            &mut crate::catalog::Budget::default(),
            registry_revision,
            cancelled,
            authorized,
        )
        .await
    }

    /// All acquisitions belonging to one run must share the retained budget.
    pub async fn refresh_catalog_with_budget(
        &mut self,
        budget: &mut crate::catalog::Budget,
        registry_revision: u64,
        cancelled: impl Future<Output = ()>,
        mut authorized: impl FnMut() -> bool,
    ) -> Result<&Catalog, Error> {
        self.catalog_snapshot = None;
        if registry_revision == 0
            || !self.server.capabilities.catalog_metadata
            || self
                .registry_revision
                .is_some_and(|revision| revision != registry_revision)
        {
            return Err(Error::Auth);
        }
        if !authorized() {
            return Err(Error::Cancelled);
        }
        if !self.is_connected() {
            return Err(self.failures.resolve(Error::Transport));
        }
        budget.check_available()?;
        self.registry_revision = Some(registry_revision);
        self.catalog_refresh = self.catalog_refresh.checked_add(1).ok_or(Error::Limit)?;
        let epoch = self.handler.epoch.load(Ordering::Acquire);
        if epoch == u64::MAX {
            return Err(Error::Limit);
        }
        let service = self.service.as_ref().ok_or(Error::Transport)?;
        let info = service.peer_info().ok_or(Error::Protocol)?;
        let mut kinds = Vec::new();
        if info.capabilities.tools.is_some() {
            kinds.push(Kind::Tool);
        }
        if info.capabilities.resources.is_some() {
            kinds.extend([Kind::Resource, Kind::ResourceTemplate]);
        }
        if info.capabilities.prompts.is_some() {
            kinds.push(Kind::Prompt);
        }
        let binding = json!({
            "registry_revision":registry_revision,
            "destination_hash":self.server.destination_hash()?,
            "connection_generation":self.connection_generation,
            "protocol":info.protocol_version,
            "capabilities":info.capabilities,
            "identity":info.server_info,
            "refresh":self.catalog_refresh,
            "notification_epoch":epoch,
        });
        crate::registration::bounded_json(&binding, 16 * 1024)?;
        let mut builder = Builder::new(&self.server.server_id, &binding, kinds.iter().copied())?;
        let peer = service.peer().clone();
        let timeout = Duration::from_secs(self.server.timeouts.catalog_seconds.into());
        tokio::pin!(cancelled);
        for kind in kinds {
            let mut cursor = None;
            loop {
                if !authorized() {
                    return Err(Error::Cancelled);
                }
                budget.check_available()?;
                let request = page(&peer, kind, cursor.clone());
                tokio::pin!(request);
                let deadline = tokio::time::sleep(timeout);
                tokio::pin!(deadline);
                let (items, next) = loop {
                    tokio::select! {
                        biased;
                        _ = &mut cancelled => return Err(Error::Cancelled),
                        _ = &mut deadline => return Err(self.failures.resolve(Error::Timeout)),
                        result = &mut request => break result.map_err(|error| self.failures.resolve(error))?,
                        result = self.wait_process() => { result.map_err(|error| self.failures.resolve(error))?; }
                    }
                };
                budget.receive_page(&items)?;
                builder.add_page(kind, cursor.as_deref(), items, next.clone())?;
                if self.handler.epoch.load(Ordering::Acquire) != epoch {
                    return Err(Error::Protocol);
                }
                cursor = next;
                if cursor.is_none() {
                    break;
                }
            }
        }
        if !authorized() || self.handler.epoch.load(Ordering::Acquire) != epoch {
            return Err(Error::Cancelled);
        }
        self.catalog_snapshot = Some(Snapshot {
            catalog: builder.finish()?,
            epoch,
            expires: Instant::now()
                + Duration::from_secs(self.server.timeouts.catalog_ttl_seconds.into()),
        });
        self.catalog(registry_revision)
    }
}

async fn page(
    peer: &Peer<RoleClient>,
    kind: Kind,
    cursor: Option<String>,
) -> Result<(Vec<Value>, Option<String>), Error> {
    let mut params = PaginatedRequestParams::default();
    params.cursor = cursor;
    let params = Some(params);
    // Do not use list_all_*: each page must pass host limits before requesting
    // another cursor. SDK response caching is disabled during initialize.
    let (items, next) = match kind {
        Kind::Tool => {
            let result = peer.list_tools(params).await.map_err(failure::service)?;
            (
                result
                    .tools
                    .into_iter()
                    .map(serde_json::to_value)
                    .collect::<Result<_, _>>(),
                result.next_cursor,
            )
        }
        Kind::Resource => {
            let result = peer
                .list_resources(params)
                .await
                .map_err(failure::service)?;
            (
                result
                    .resources
                    .into_iter()
                    .map(serde_json::to_value)
                    .collect::<Result<_, _>>(),
                result.next_cursor,
            )
        }
        Kind::ResourceTemplate => {
            let result = peer
                .list_resource_templates(params)
                .await
                .map_err(failure::service)?;
            (
                result
                    .resource_templates
                    .into_iter()
                    .map(serde_json::to_value)
                    .collect::<Result<_, _>>(),
                result.next_cursor,
            )
        }
        Kind::Prompt => {
            let result = peer.list_prompts(params).await.map_err(failure::service)?;
            (
                result
                    .prompts
                    .into_iter()
                    .map(serde_json::to_value)
                    .collect::<Result<_, _>>(),
                result.next_cursor,
            )
        }
    };
    Ok((items.map_err(|_| Error::Protocol)?, next))
}

#[cfg(test)]
mod tests {
    use super::super::tests::{fixture, secrets, server};
    use super::*;
    use std::collections::BTreeMap;

    #[tokio::test]
    async fn expired_notified_and_failed_refresh_never_returns_the_old_snapshot() {
        let (url, methods, task) = fixture(false).await;
        let registration = server(
            Destination::StreamableHttp {
                url,
                secret_headers: BTreeMap::new(),
            },
            ProtocolProfile::Discover2026,
        );
        let mut connection =
            Connection::prepare(&registration, "project", &secrets(), None, || true).unwrap();
        connection
            .initialize(std::future::pending(), || true)
            .await
            .unwrap();
        let first = connection
            .refresh_catalog(1, std::future::pending(), || true)
            .await
            .unwrap()
            .generation
            .clone();
        connection.handler.epoch.fetch_add(1, Ordering::AcqRel);
        assert!(connection.catalog(1).is_err());
        let second = connection
            .refresh_catalog(1, std::future::pending(), || true)
            .await
            .unwrap()
            .generation
            .clone();
        assert_ne!(first, second);
        connection.catalog_snapshot.as_mut().unwrap().expires = Instant::now();
        assert!(connection.catalog(1).is_err());
        connection
            .refresh_catalog(1, std::future::pending(), || true)
            .await
            .unwrap();
        let sent = methods.lock().unwrap().len();
        assert!(
            connection
                .refresh_catalog(1, std::future::ready(()), || true)
                .await
                .is_err()
        );
        assert!(connection.catalog(1).is_err());
        assert_eq!(sent, methods.lock().unwrap().len());
        assert!(
            connection
                .refresh_catalog(2, std::future::pending(), || true)
                .await
                .is_err()
        );
        assert_eq!(sent, methods.lock().unwrap().len());
        connection.shutdown().await.unwrap();
        task.abort();
        let _ = task.await;
    }

    #[tokio::test]
    async fn disabled_catalog_capability_does_not_list_anything() {
        let (url, methods, task) = fixture(false).await;
        let mut registration = server(
            Destination::StreamableHttp {
                url,
                secret_headers: BTreeMap::new(),
            },
            ProtocolProfile::Discover2026,
        );
        registration.capabilities.catalog_metadata = false;
        let mut connection =
            Connection::prepare(&registration, "project", &secrets(), None, || true).unwrap();
        connection
            .initialize(std::future::pending(), || true)
            .await
            .unwrap();
        assert!(matches!(
            connection
                .refresh_catalog(1, std::future::pending(), || true)
                .await,
            Err(Error::Auth)
        ));
        assert_eq!(*methods.lock().unwrap(), vec!["server/discover"]);
        connection.shutdown().await.unwrap();
        task.abort();
        let _ = task.await;
    }
}
