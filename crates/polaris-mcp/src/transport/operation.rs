//! One-shot MCP operations with catalog validation and outcome classification.

use super::*;
use crate::{
    catalog::{ItemId, Kind},
    content::{Part, Reply, Status},
    registration::{MAX_JSON_BYTES, bounded_json, canonical_hash},
};
use futures_util::FutureExt;
use serde::Serialize;
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};

/// Retained, read-only evidence of SDK request polling, including after the
/// operation future is dropped. True means bytes may have been sent, not that
/// a response arrived or the remote effect completed. This is not a grant.
#[derive(Clone)]
pub struct OperationProgress(Arc<AtomicBool>);

impl OperationProgress {
    pub fn may_have_sent(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }

    async fn observe_request<F: Future>(&self, request: F) -> F::Output {
        self.0.store(true, Ordering::Release);
        request.await
    }
}

/// Protocol identity for the existing host Gate/intent owner. This value is
/// evidence, not a permission. No secret or full destination URL is included.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct OperationBinding {
    pub item: ItemId,
    pub registry_revision: u64,
    pub destination_hash: String,
    pub connection_generation: String,
    pub catalog_generation: String,
    pub schema_hash: Option<String>,
    pub arguments_hash: String,
    pub selection_id: Option<String>,
    pub resource_uri: Option<String>,
}

/// Owned, single-use preparation. It cannot be deserialized from model input,
/// cloned for retries, or reused on another connection. The host must still
/// authorize/persist its concrete operation before `perform_once`.
pub struct BoundOperation {
    binding: OperationBinding,
    arguments: Value,
    progress: OperationProgress,
}
impl BoundOperation {
    pub fn binding(&self) -> &OperationBinding {
        &self.binding
    }
    pub fn arguments(&self) -> &Value {
        &self.arguments
    }

    pub fn progress(&self) -> OperationProgress {
        self.progress.clone()
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum OperationFailure {
    BeforeSend(Error),
    /// Includes cancellation, timeout or invalid response after request polling
    /// may have sent bytes. Never proof that the remote operation was undone.
    OutcomeUnknown(Error),
}

impl Connection {
    /// For tools the host must have already validated the selected schema.
    /// Resource reads use an exact catalog URI or bounded expansion of the
    /// selected catalog template, never a caller-supplied replacement URI.
    pub fn prepare_operation(
        &self,
        revision: u64,
        generation: &str,
        id: &ItemId,
        arguments: Value,
    ) -> Result<BoundOperation, Error> {
        if id.kind == Kind::Prompt {
            return Err(Error::Auth);
        }
        self.bind_operation(revision, generation, id, arguments, None)
    }

    /// Only a host CLI/TUI/GUI user-selection action may call this method. It is
    /// deliberately absent from the model-facing meta tool operations.
    pub fn host_select_prompt(
        &self,
        revision: u64,
        generation: &str,
        name: &str,
        arguments: Value,
    ) -> Result<BoundOperation, Error> {
        let id = ItemId {
            server_id: self.server.server_id.clone(),
            kind: Kind::Prompt,
            key: name.into(),
        };
        let selection = polaris_auth::pkce::random_urlsafe(24).map_err(|_| Error::Protection)?;
        self.bind_operation(revision, generation, &id, arguments, Some(selection))
    }

    fn bind_operation(
        &self,
        revision: u64,
        generation: &str,
        id: &ItemId,
        arguments: Value,
        selection_id: Option<String>,
    ) -> Result<BoundOperation, Error> {
        let catalog = self.catalog(revision)?;
        if id.server_id != self.server.server_id || generation != catalog.generation {
            return Err(Error::RevisionConflict);
        }
        let item = catalog
            .items
            .iter()
            .find(|item| &item.id == id)
            .ok_or(Error::Protocol)?;
        let allowed = match id.kind {
            Kind::Tool => self.server.capabilities.tool_call,
            Kind::Resource | Kind::ResourceTemplate => self.server.capabilities.resource_read,
            Kind::Prompt => self.server.capabilities.prompt_selection && selection_id.is_some(),
        };
        if !allowed {
            return Err(Error::Auth);
        }
        bounded_json(&arguments, MAX_JSON_BYTES)?;
        if !arguments.is_object()
            || (id.kind == Kind::Resource
                && arguments.as_object().is_none_or(|args| !args.is_empty()))
        {
            return Err(Error::Protocol);
        }
        let resource_uri = match id.kind {
            Kind::Resource => Some(id.key.clone()),
            Kind::ResourceTemplate => Some(crate::template::expand(&id.key, &arguments)?),
            _ => None,
        };
        Ok(BoundOperation {
            binding: OperationBinding {
                item: id.clone(),
                registry_revision: revision,
                destination_hash: self.server.destination_hash()?,
                connection_generation: self.connection_generation.clone(),
                catalog_generation: generation.into(),
                schema_hash: item.schema_hash.clone(),
                arguments_hash: canonical_hash(&arguments)?,
                selection_id,
                resource_uri,
            },
            arguments,
            progress: OperationProgress(Arc::new(AtomicBool::new(false))),
        })
    }

    /// One ordinary SDK request, no MRTR continuation, reconnect, polling or
    /// replay. `authorized` must recheck the existing owner's durable intent,
    /// grant, policy revision, destination and secret identity, not annotations.
    pub async fn perform_once(
        &mut self,
        operation: BoundOperation,
        cancelled: impl Future<Output = ()>,
        mut authorized: impl FnMut(&OperationBinding) -> bool,
    ) -> Result<Reply, OperationFailure> {
        tokio::pin!(cancelled);
        if cancelled.as_mut().now_or_never().is_some() {
            return Err(OperationFailure::BeforeSend(Error::Cancelled));
        }
        let rebound = self
            .bind_operation(
                operation.binding.registry_revision,
                &operation.binding.catalog_generation,
                &operation.binding.item,
                operation.arguments.clone(),
                operation.binding.selection_id.clone(),
            )
            .map_err(OperationFailure::BeforeSend)?;
        if rebound.binding != operation.binding || !authorized(&operation.binding) {
            return Err(OperationFailure::BeforeSend(Error::Auth));
        }
        let peer = self
            .service
            .as_ref()
            .ok_or(OperationFailure::BeforeSend(Error::Transport))?
            .peer()
            .clone();
        let duration = Duration::from_secs(self.server.timeouts.operation_seconds.into());
        let sensitive = self.sensitive.clone();
        let kind = operation.binding.item.kind;
        let request = async {
            use rmcp::model::{
                CallToolRequestParams, CallToolResponse, GetPromptRequestParams, GetPromptResponse,
                ReadResourceRequestParams, ReadResourceResponse,
            };
            let name = operation.binding.item.key.clone();
            let arguments = operation
                .arguments
                .as_object()
                .cloned()
                .ok_or(Error::Protocol)?;
            let wire =
                match kind {
                    Kind::Tool => match operation
                        .progress
                        .observe_request(peer.call_tool_once(
                            CallToolRequestParams::new(name).with_arguments(arguments),
                        ))
                        .await
                        .map_err(|_| Error::Transport)?
                    {
                        CallToolResponse::Complete(value) => serde_json::to_value(value),
                        CallToolResponse::InputRequired(value) => serde_json::to_value(value),
                        CallToolResponse::Task(value) => serde_json::to_value(value),
                        _ => return Err(Error::Protocol),
                    },
                    Kind::Resource | Kind::ResourceTemplate => match operation
                        .progress
                        .observe_request(
                            peer.read_resource_once(ReadResourceRequestParams::new(
                                operation
                                    .binding
                                    .resource_uri
                                    .as_ref()
                                    .ok_or(Error::Protocol)?
                                    .clone(),
                            )),
                        )
                        .await
                        .map_err(|_| Error::Transport)?
                    {
                        ReadResourceResponse::Complete(value) => serde_json::to_value(value),
                        ReadResourceResponse::InputRequired(value) => serde_json::to_value(value),
                        _ => return Err(Error::Protocol),
                    },
                    Kind::Prompt => match operation
                        .progress
                        .observe_request(peer.get_prompt_once(
                            GetPromptRequestParams::new(name).with_arguments(arguments),
                        ))
                        .await
                        .map_err(|_| Error::Transport)?
                    {
                        GetPromptResponse::Complete(value) => serde_json::to_value(value),
                        GetPromptResponse::InputRequired(value) => serde_json::to_value(value),
                        _ => return Err(Error::Protocol),
                    },
                }
                .map_err(|_| Error::Protocol)?;
            crate::content::decode(wire, kind)
        };
        tokio::pin!(request);
        let timeout = tokio::time::sleep(duration);
        tokio::pin!(timeout);
        let mut owner = OperationOwner {
            connection: self,
            complete: false,
        };
        let reply = loop {
            tokio::select! {
                biased;
                _ = &mut cancelled => return Err(OperationFailure::OutcomeUnknown(Error::Cancelled)),
                _ = &mut timeout => return Err(OperationFailure::OutcomeUnknown(Error::Timeout)),
                response = &mut request => break response.map_err(OperationFailure::OutcomeUnknown)?,
                result = owner.connection.wait_process() => {
                    if !authorized(&operation.binding) { return Err(OperationFailure::OutcomeUnknown(Error::Auth)); }
                    result.map_err(OperationFailure::OutcomeUnknown)?;
                }
            }
        };
        // A binary may encode an injected secret that the JSON scanner could
        // not see before base64 decode. Never return those bytes to the host.
        for part in &reply.parts {
            if let Part::Binary { bytes, .. } = part {
                sensitive
                    .bytes(bytes)
                    .map_err(OperationFailure::OutcomeUnknown)?;
            }
        }
        owner.complete = matches!(reply.status, Status::Complete | Status::ToolError);
        Ok(reply)
    }
}

struct OperationOwner<'a> {
    connection: &'a mut Connection,
    complete: bool,
}
impl Drop for OperationOwner<'_> {
    fn drop(&mut self) {
        if !self.complete {
            self.connection.request_stop();
        }
    }
}

#[cfg(test)]
mod tests;
