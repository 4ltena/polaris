//! Local hosts: the Gate approves the exact prepared operation, then one send.
use super::*;
use crate::{
    approval::{Approver, Gate},
    mcp::operation::{ExternalCeiling, HostOperationIdentity, McpOperation, read::ResourceRead},
};
use polaris_mcp::{BoundOperation, registration::canonical_hash};

const APPROVAL_TTL_MS: u64 = 60_000;

/// Prepared by the runtime from a frozen request; model input cannot build it.
pub struct LocalCall {
    operation: BoundOperation,
    intent: McpOperation,
    ceiling: ExternalCeiling,
    destination: String,
    schema_hash: Option<String>,
    secret_revision: Option<u64>,
}

pub struct ApprovedCall(LocalCall);

impl LocalCall {
    /// A tool call asks through the Gate; a resource read already passed its
    /// read capability. Either refusal is a code for a failed tool result:
    /// nothing was sent.
    pub async fn approve(
        self,
        gate: &mut Gate,
        approver: &mut dyn Approver,
    ) -> Result<ApprovedCall, &'static str> {
        if !self.intent.is_resource_read() {
            gate.check_mcp_operation_async(self.ceiling, &self.intent, approver)
                .await
                .map_err(|_| "mcp_call_denied")?;
        }
        if crate::execution_owner::now_ms()
            .map_or(true, |now| now >= self.intent.identity().expires_at_ms)
        {
            return Err("mcp_approval_expired");
        }
        Ok(ApprovedCall(self))
    }
}

impl RunRuntime {
    pub fn is_local(&self) -> bool {
        self.port.local_ceiling().is_some()
    }

    pub fn prepare_local(
        &mut self,
        snapshot: &RunRequest,
        call: &ToolCall,
    ) -> Result<LocalCall, Error> {
        let ceiling = self.port.local_ceiling().ok_or(McpCallError::Invalid)?;
        self.active()?;
        if snapshot.generation != self.request_generation
            || &snapshot.binding != self.port.binding()
        {
            return Err(crate::mcp::Error::SelectionStale.into());
        }
        let read = (call.name == "mcp_catalog")
            .then(|| ResourceRead::parse(call).map_err(|_| crate::mcp::Error::InvalidArguments))
            .transpose()?;
        let (item, generation, arguments, schema_hash) = match &read {
            Some(read) => {
                let id = read.id();
                if !self
                    .config
                    .server(&id.server_id)?
                    .capabilities
                    .resource_read
                {
                    return Err(polaris_mcp::Error::Auth.into());
                }
                (
                    id,
                    read.generation().to_owned(),
                    read.arguments().clone(),
                    None,
                )
            }
            None => {
                let identity = snapshot
                    .snapshot
                    .resolve(&self.view()?, &call.name, &call.arguments)?
                    .clone();
                (
                    identity.id,
                    identity.catalog_generation,
                    call.arguments.clone(),
                    Some(identity.schema_hash),
                )
            }
        };
        let server = self.config.server(&item.server_id)?;
        let destination = server.destination_hash()?;
        let app = crate::mcp::apps::find(server);
        let revision = self.config.registry.revision;
        let entry = self
            .connections
            .iter_mut()
            .find(|e| e.phase == Phase::Ready && e.connection.server_id() == item.server_id)
            .ok_or(polaris_mcp::Error::Protocol)?;
        let operation =
            entry
                .connection
                .prepare_operation(revision, &generation, &item, arguments)?;
        if let Some(read) = &read {
            read.check(operation.binding())
                .map_err(|_| McpCallError::Invalid)?;
        }
        self.local_operations += 1;
        let binding = self.port.binding();
        let identity = HostOperationIdentity {
            run_id: binding.run.clone(),
            attempt_id: binding.attempt.clone(),
            model_call_id: call.id.clone(),
            operation_id: format!("mcp-local-{}", self.local_operations),
            policy_revision: 0,
            expires_at_ms: crate::execution_owner::now_ms()
                .map_err(|_| McpCallError::Invalid)?
                .saturating_add(APPROVAL_TTL_MS),
        };
        let project = self.config.project.clone();
        let intent = match read {
            Some(_) => McpOperation::from_resource_binding(
                identity,
                project,
                operation.binding().clone(),
                canonical_hash(&call.arguments)?,
            ),
            None => McpOperation::from_binding(identity, project, operation.binding().clone())
                .and_then(|intent| intent.with_arguments(operation.arguments(), app)),
        }
        .map_err(|_| McpCallError::Invalid)?;
        Ok(LocalCall {
            operation,
            intent,
            ceiling,
            destination,
            schema_hash,
            secret_revision: self.config.secrets.as_ref().map(|s| s.revision()),
        })
    }

    /// Rechecks the registration and destination at send time.
    pub async fn perform_local(
        &mut self,
        call: ApprovedCall,
        cancelled: impl Future<Output = ()>,
    ) -> Result<McpPublishedResult, Error> {
        self.active()?;
        let LocalCall {
            operation,
            intent,
            destination,
            schema_hash,
            secret_revision,
            ..
        } = call.0;
        let item = intent.binding().item.clone();
        let generation = intent.binding().catalog_generation.clone();
        let resource = intent.is_resource_read();
        let config = &self.config;
        let entry = self
            .connections
            .iter_mut()
            .find(|e| e.phase == Phase::Ready && e.connection.server_id() == item.server_id)
            .ok_or(polaris_mcp::Error::Protocol)?;
        Ok(self
            .port
            .perform_local(
                &mut entry.connection,
                operation,
                &intent,
                &mut self.results,
                cancelled,
                |binding| {
                    config
                        .server(&item.server_id)
                        .is_ok_and(|server| !resource || server.capabilities.resource_read)
                        && config.secrets.as_ref().map(|s| s.revision()) == secret_revision
                        && binding.registry_revision == config.registry.revision
                        && binding.destination_hash == destination
                        && binding.item == item
                        && binding.catalog_generation == generation
                        && (schema_hash.is_none() || binding.schema_hash == schema_hash)
                },
            )
            .await?)
    }
}
