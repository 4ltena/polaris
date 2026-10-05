//! The desktop execution owner's bounded MCP approval and publication relay.
//! Connections stay on the worker's retained Tokio runtime, outside this owner.

use super::{APPROVAL_TTL_MS, StartDeadline, now_ms};
use crate::{
    approval::{ApprovalPolicy, Gate},
    desktop_store::{
        IntentReceipt, MCP_TOOL_OPERATION, McpDisposition, McpPromptIntent, McpPromptPublication,
        McpPromptResult, McpPublishedResult, McpTerminalInput, McpTerminalPublication,
        OwnerRequestHistory, Published, StoreError, Writer, local_mcp_prompt_reply_budget,
        local_mcp_prompt_terminal, local_mcp_terminal, mcp_reply_budget, validate_turn_suffix,
    },
    mcp::{
        operation::{ExternalCeiling, HostOperationIdentity, McpOperation},
        results::{ReplyProjection, ResultStore, RunBinding, Source, SourceOrigin},
    },
};
use futures_util::FutureExt;
use polaris_desktop_protocol::{
    event::{ApprovalExpired, EventBody, ExpiryReason},
    ids::{ApprovalId, DecimalU64, OperationId, SessionId},
    request::RunTarget,
    run_state::Observation,
    snapshot::{ApprovalDecision, ApprovalDisplay, PendingApproval, PendingApprovalState},
};
use polaris_mcp::{
    BoundOperation, Connection, OperationBinding, OperationFailure, OperationProgress,
};
use polaris_provider::Message;
use std::{
    future::Future,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore, mpsc, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum McpCallError {
    #[error("mcp_owner_unavailable")]
    Unavailable,
    #[error("mcp_call_invalid")]
    Invalid,
    #[error("mcp_call_denied")]
    Denied,
    #[error("mcp_call_cancelled")]
    Cancelled,
    #[error("mcp_publication_unconfirmed")]
    Storage,
}

/// Host-prepared data, never a model-deserializable permission or retry token.
pub struct McpCall {
    operation: BoundOperation,
    model_call: String,
    target: String,
    suffix: Vec<Message>,
    meta_arguments_hash: Option<String>,
    app: Option<&'static crate::mcp::apps::App>,
}

impl McpCall {
    /// The built-in app definition the host matched for this server.
    pub(crate) fn with_app(mut self, app: Option<&'static crate::mcp::apps::App>) -> Self {
        self.app = app;
        self
    }

    pub fn new(
        operation: BoundOperation,
        model_call: String,
        target: String,
        suffix: Vec<Message>,
    ) -> Result<Self, McpCallError> {
        Self::new_checked(operation, model_call, target, suffix, None)
    }

    pub(crate) fn resource_read(
        operation: BoundOperation,
        model_call: String,
        target: String,
        suffix: Vec<Message>,
    ) -> Result<Self, McpCallError> {
        let original = suffix
            .iter()
            .flat_map(|message| &message.tool_calls)
            .find(|call| call.id == model_call)
            .ok_or(McpCallError::Invalid)?;
        crate::mcp::operation::read::ResourceRead::parse(original)
            .and_then(|read| read.check(operation.binding()))
            .map_err(|_| McpCallError::Invalid)?;
        let hash = polaris_mcp::registration::canonical_hash(&original.arguments)
            .map_err(|_| McpCallError::Invalid)?;
        Self::new_checked(operation, model_call, target, suffix, Some(hash))
    }

    fn new_checked(
        operation: BoundOperation,
        model_call: String,
        target: String,
        suffix: Vec<Message>,
        meta_arguments_hash: Option<String>,
    ) -> Result<Self, McpCallError> {
        if [&model_call, &target]
            .into_iter()
            .any(|s| s.is_empty() || s.len() > 256 || s.chars().any(char::is_control))
        {
            return Err(McpCallError::Invalid);
        }
        validate_turn_suffix(&suffix, Observation::Cancelled).map_err(|_| McpCallError::Invalid)?;
        let alias = if meta_arguments_hash.is_some() {
            "mcp_catalog".into()
        } else {
            polaris_mcp::catalog::tool_alias(&operation.binding().item)
                .map_err(|_| McpCallError::Invalid)?
        };
        let calls: Vec<_> = suffix
            .iter()
            .flat_map(|m| &m.tool_calls)
            .filter(|call| call.id == model_call)
            .collect();
        if calls.len() != 1
            || calls[0].name != alias
            || polaris_mcp::registration::canonical_hash(&calls[0].arguments)
                .map_err(|_| McpCallError::Invalid)?
                != meta_arguments_hash
                    .as_ref()
                    .unwrap_or(&operation.binding().arguments_hash)
                    .as_str()
            || suffix
                .iter()
                .any(|m| m.tool_call_id.as_deref() == Some(&model_call))
        {
            return Err(McpCallError::Invalid);
        }
        Ok(Self {
            app: None,
            operation,
            model_call,
            target,
            suffix,
            meta_arguments_hash,
        })
    }
}

#[derive(Default)]
struct Stop {
    value: AtomicBool,
    notify: Notify,
}
impl Stop {
    fn set(&self) {
        self.value.store(true, Ordering::Release);
        self.notify.notify_waiters();
    }
    fn is_set(&self) -> bool {
        self.value.load(Ordering::Acquire)
    }
    async fn cancelled(&self) {
        let notified = self.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !self.is_set() {
            notified.await;
        }
    }
}

enum Terminal {
    Reply(ReplyProjection),
    Failure(OperationFailure),
}
struct Shared {
    abandoned: AtomicBool,
    terminal: Mutex<Option<Terminal>>,
}
struct ClientGuard(Arc<Shared>);
impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.0.abandoned.store(true, Ordering::Release);
    }
}
enum RequestKind {
    Model {
        model_call: String,
        target: String,
        suffix: Vec<Message>,
        meta_arguments_hash: Option<String>,
        arguments: serde_json::Value,
        app: Option<&'static crate::mcp::apps::App>,
    },
    HostPrompt {
        project: String,
    },
}

enum PublishedResult {
    Model(McpPublishedResult),
    Prompt(McpPromptResult),
}
impl PublishedResult {
    fn disposition(&self) -> McpDisposition {
        match self {
            Self::Model(result) => result.disposition,
            Self::Prompt(result) => result.disposition,
        }
    }
    fn permits_continuation(&self) -> bool {
        match self {
            Self::Model(result) => result.permits_continuation(),
            Self::Prompt(result) => result.permits_continuation(),
        }
    }
}
struct Request {
    binding: OperationBinding,
    progress: OperationProgress,
    kind: RequestKind,
    shared: Arc<Shared>,
    grant: Option<oneshot::Sender<Result<Grant, McpCallError>>>,
    result: Option<oneshot::Sender<Result<PublishedResult, McpCallError>>>,
    _permit: OwnedSemaphorePermit,
}
struct Grant {
    binding: OperationBinding,
    operation: OperationId,
    deadline: StartDeadline,
    reply_budget: usize,
}

enum Queued {
    Operation(Box<Request>),
    History {
        reply: oneshot::Sender<Result<OwnerRequestHistory, McpCallError>>,
        _permit: OwnedSemaphorePermit,
    },
}

/// Only the trusted run setup obtains this port. A cloned port shares the same
/// one-operation capacity and cancellation boundary; it never clones a grant.
/// A local port has no desktop owner: the host's Gate approves each operation
/// and the session history records the result.
#[derive(Clone)]
pub struct McpPort {
    tx: Option<mpsc::Sender<Queued>>,
    capacity: Arc<Semaphore>,
    stop: Arc<Stop>,
    binding: RunBinding,
    local_ceiling: Option<ExternalCeiling>,
}

impl McpPort {
    /// CLI and TUI runs. The ceiling is the explicit external permission.
    pub fn local(session: &str, run: &str, attempt: &str, ceiling: ExternalCeiling) -> Self {
        Self {
            tx: None,
            capacity: Arc::new(Semaphore::new(1)),
            stop: Arc::new(Stop::default()),
            binding: RunBinding {
                session: session.into(),
                run: run.into(),
                attempt: attempt.into(),
            },
            local_ceiling: Some(ceiling),
        }
    }

    pub(crate) fn local_ceiling(&self) -> Option<ExternalCeiling> {
        self.local_ceiling
    }

    /// Stops new operations and interrupts pending ones on every clone.
    pub fn cancel(&self) {
        self.stop.set();
    }

    pub(crate) fn binding(&self) -> &RunBinding {
        &self.binding
    }

    pub(crate) fn stopped(&self) -> bool {
        self.stop.is_set() || self.tx.as_ref().is_some_and(mpsc::Sender::is_closed)
    }

    pub(crate) async fn cancelled(&self) {
        let closed = async {
            match &self.tx {
                Some(tx) => tx.closed().await,
                None => std::future::pending().await,
            }
        };
        tokio::select! {
            _ = self.stop.cancelled() => (),
            _ = closed => (),
        }
    }

    /// Catalog/connection work shares the tool lane without creating an
    /// operation grant. Holding the permit also keeps the owner nonquiescent.
    pub(crate) fn reserve_metadata(&self) -> Result<OwnedSemaphorePermit, McpCallError> {
        if self.stopped() {
            return Err(McpCallError::Cancelled);
        }
        self.capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| McpCallError::Unavailable)
    }

    /// Read the fixed run's owner-proven history after publication. A terminal
    /// ACK may arrive before its entry releases capacity, so wait cancellably.
    pub(crate) async fn history(
        &self,
        cancelled: impl Future<Output = ()>,
    ) -> Result<OwnerRequestHistory, McpCallError> {
        tokio::pin!(cancelled);
        let permit = tokio::select! {
            biased;
            _ = &mut cancelled => return Err(McpCallError::Cancelled),
            _ = self.cancelled() => return Err(McpCallError::Cancelled),
            permit = self.capacity.clone().acquire_owned() =>
                permit.map_err(|_| McpCallError::Unavailable)?,
        };
        if self.stopped() || cancelled.as_mut().now_or_never().is_some() {
            return Err(McpCallError::Cancelled);
        }
        let (reply, mut result) = oneshot::channel();
        self.tx
            .as_ref()
            .ok_or(McpCallError::Unavailable)?
            .try_send(Queued::History {
                reply,
                _permit: permit,
            })
            .map_err(|_| McpCallError::Unavailable)?;
        let history = tokio::select! {
            biased;
            _ = &mut cancelled => return Err(McpCallError::Cancelled),
            _ = self.cancelled() => {
                return Err(match result.try_recv() {
                    Ok(Err(reason)) => reason,
                    _ => McpCallError::Cancelled,
                });
            }
            result = &mut result => result.map_err(|_| McpCallError::Unavailable)??,
        };
        if self.stopped() || cancelled.as_mut().now_or_never().is_some() {
            return Err(McpCallError::Cancelled);
        }
        Ok(history)
    }

    /// Retain Connection and ResultStore outside the caller future and shut
    /// them down after it returns or is dropped. `authorized` rechecks the
    /// trusted registry/destination/secret identity; it is not model input.
    pub async fn call(
        &self,
        connection: &mut Connection,
        call: McpCall,
        results: &mut ResultStore,
        cancelled: impl Future<Output = ()>,
        authorized: impl FnMut(&OperationBinding) -> bool,
    ) -> Result<McpPublishedResult, McpCallError> {
        let McpCall {
            operation,
            model_call,
            target,
            suffix,
            meta_arguments_hash,
            app,
        } = call;
        let arguments = operation.arguments().clone();
        match self
            .perform(
                connection,
                operation,
                RequestKind::Model {
                    model_call,
                    target,
                    suffix,
                    meta_arguments_hash,
                    arguments,
                    app,
                },
                results,
                cancelled,
                authorized,
            )
            .await?
        {
            PublishedResult::Model(result) => Ok(result),
            PublishedResult::Prompt(_) => Err(McpCallError::Invalid),
        }
    }

    /// Trusted host entry only. The owner checks the immutable accepted choice
    /// and project before publishing the intent or releasing a transport grant.
    pub(crate) async fn host_prompt(
        &self,
        connection: &mut Connection,
        operation: BoundOperation,
        project: String,
        results: &mut ResultStore,
        cancelled: impl Future<Output = ()>,
        authorized: impl FnMut(&OperationBinding) -> bool,
    ) -> Result<McpPromptResult, McpCallError> {
        let binding = operation.binding();
        if binding.item.kind != polaris_mcp::catalog::Kind::Prompt
            || binding.selection_id.is_none()
            || binding.schema_hash.is_some()
            || binding.resource_uri.is_some()
            || project.is_empty()
            || project.len() > 128
            || project.contains('\0')
        {
            return Err(McpCallError::Invalid);
        }
        if self.local_ceiling.is_some() {
            return self
                .perform_local_prompt(
                    connection, operation, project, results, cancelled, authorized,
                )
                .await;
        }
        match self
            .perform(
                connection,
                operation,
                RequestKind::HostPrompt { project },
                results,
                cancelled,
                authorized,
            )
            .await?
        {
            PublishedResult::Prompt(result) => Ok(result),
            PublishedResult::Model(_) => Err(McpCallError::Invalid),
        }
    }

    async fn perform_local_prompt(
        &self,
        connection: &mut Connection,
        operation: BoundOperation,
        project: String,
        results: &mut ResultStore,
        cancelled: impl Future<Output = ()>,
        mut authorized: impl FnMut(&OperationBinding) -> bool,
    ) -> Result<McpPromptResult, McpCallError> {
        tokio::pin!(cancelled);
        if self.stop.is_set() || cancelled.as_mut().now_or_never().is_some() {
            return Err(McpCallError::Cancelled);
        }
        results
            .check_binding(&self.binding)
            .map_err(|_| McpCallError::Invalid)?;
        let _permit = self
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| McpCallError::Unavailable)?;
        let binding = operation.binding().clone();
        let selection_id = binding.selection_id.clone().ok_or(McpCallError::Invalid)?;
        let reply_budget = local_mcp_prompt_reply_budget(&self.binding, &project, &binding)
            .map_err(|_| McpCallError::Invalid)?;
        let response = connection
            .perform_once(
                operation,
                async {
                    tokio::select! {
                        biased;
                        _ = &mut cancelled => (),
                        _ = self.stop.cancelled() => (),
                    }
                },
                |seen| seen == &binding && !self.stop.is_set() && authorized(seen),
            )
            .await;
        let terminal = match response {
            Ok(reply) => match results.project_reply_bounded(
                &self.binding,
                Source {
                    server: binding.item.server_id.clone(),
                    origin: SourceOrigin::HostSelection {
                        selection_id: selection_id.clone(),
                    },
                    result_generation: selection_id,
                },
                reply,
                Instant::now(),
                reply_budget,
            ) {
                Ok(reply) => Terminal::Reply(reply),
                Err(_) => {
                    Terminal::Failure(OperationFailure::OutcomeUnknown(polaris_mcp::Error::Limit))
                }
            },
            Err(failure) => Terminal::Failure(failure),
        };
        let input = match &terminal {
            Terminal::Reply(reply) => McpTerminalInput::Reply(reply),
            Terminal::Failure(failure) => McpTerminalInput::Failure(*failure),
        };
        local_mcp_prompt_terminal(&self.binding, &project, &binding, input)
            .or_else(|_| {
                local_mcp_prompt_terminal(
                    &self.binding,
                    &project,
                    &binding,
                    McpTerminalInput::Failure(OperationFailure::OutcomeUnknown(
                        polaris_mcp::Error::Limit,
                    )),
                )
            })
            .map_err(|_| McpCallError::Storage)
    }

    async fn perform(
        &self,
        connection: &mut Connection,
        operation: BoundOperation,
        kind: RequestKind,
        results: &mut ResultStore,
        cancelled: impl Future<Output = ()>,
        mut authorized: impl FnMut(&OperationBinding) -> bool,
    ) -> Result<PublishedResult, McpCallError> {
        tokio::pin!(cancelled);
        if self.stop.is_set() || cancelled.as_mut().now_or_never().is_some() {
            return Err(McpCallError::Cancelled);
        }
        results
            .check_binding(&self.binding)
            .map_err(|_| McpCallError::Invalid)?;
        let permit = self
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| McpCallError::Unavailable)?;
        let app = match &kind {
            RequestKind::Model { app, .. } => *app,
            RequestKind::HostPrompt { .. } => None,
        };
        let origin = match &kind {
            RequestKind::Model { model_call, .. } => SourceOrigin::ModelCall {
                model_call: model_call.clone(),
            },
            RequestKind::HostPrompt { .. } => SourceOrigin::HostSelection {
                selection_id: operation
                    .binding()
                    .selection_id
                    .clone()
                    .ok_or(McpCallError::Invalid)?,
            },
        };
        let shared = Arc::new(Shared {
            abandoned: AtomicBool::new(false),
            terminal: Mutex::new(None),
        });
        let _guard = ClientGuard(shared.clone());
        let (grant_tx, mut grant_rx) = oneshot::channel();
        let (result_tx, result_rx) = oneshot::channel();
        let binding = operation.binding().clone();
        self.tx
            .as_ref()
            .ok_or(McpCallError::Unavailable)?
            .try_send(Queued::Operation(Box::new(Request {
                binding: binding.clone(),
                progress: operation.progress(),
                kind,
                shared: shared.clone(),
                grant: Some(grant_tx),
                result: Some(result_tx),
                _permit: permit,
            })))
            .map_err(|_| McpCallError::Unavailable)?;
        let grant = tokio::select! {
            biased;
            _ = &mut cancelled => return Err(McpCallError::Cancelled),
            _ = self.stop.cancelled() => {
                // Preserve an already published rejection. A queued successful
                // grant never overrides owner cancellation.
                return Err(match grant_rx.try_recv() {
                    Ok(Err(reason)) => reason,
                    _ => McpCallError::Cancelled,
                });
            }
            grant = &mut grant_rx => grant.map_err(|_| McpCallError::Unavailable)??,
        };
        let _turn = crate::mcp::apps::App::turn(app, async {
            tokio::select! {
                biased;
                _ = &mut cancelled => (),
                _ = self.stop.cancelled() => (),
            }
        })
        .await;
        let response = connection
            .perform_once(
                operation,
                async {
                    tokio::select! {
                        biased;
                        _ = &mut cancelled => (),
                        _ = self.stop.cancelled() => (),
                    }
                },
                |seen| {
                    seen == &grant.binding
                        && !self.stop.is_set()
                        && grant.deadline.valid()
                        && authorized(seen)
                },
            )
            .await;
        let terminal = match response {
            Ok(reply) => match results.project_reply_bounded(
                &self.binding,
                Source {
                    server: binding.item.server_id,
                    origin,
                    result_generation: grant.operation.as_str().into(),
                },
                reply,
                Instant::now(),
                grant.reply_budget,
            ) {
                Ok(reply) => Terminal::Reply(reply),
                Err(_) => {
                    Terminal::Failure(OperationFailure::OutcomeUnknown(polaris_mcp::Error::Limit))
                }
            },
            Err(failure) => Terminal::Failure(failure),
        };
        *shared.terminal.lock().map_err(|_| McpCallError::Storage)? = Some(terminal);
        // Once the response is ready, cancellation cannot fabricate an ACK or
        // erase it. The owner either confirms publication or reports failure.
        result_rx.await.map_err(|_| McpCallError::Storage)?
    }
}

impl McpPort {
    /// Local hosts only, after `LocalCall::approve`. Sends once and returns the
    /// canonical message; a dropped future leaves no result, which resume
    /// closes as unknown.
    pub(crate) async fn perform_local(
        &self,
        connection: &mut Connection,
        operation: BoundOperation,
        intent: &McpOperation,
        results: &mut ResultStore,
        cancelled: impl Future<Output = ()>,
        mut authorized: impl FnMut(&OperationBinding) -> bool,
    ) -> Result<McpPublishedResult, McpCallError> {
        tokio::pin!(cancelled);
        if self.local_ceiling.is_none() {
            return Err(McpCallError::Invalid);
        }
        if self.stop.is_set() || cancelled.as_mut().now_or_never().is_some() {
            return Err(McpCallError::Cancelled);
        }
        results
            .check_binding(&self.binding)
            .map_err(|_| McpCallError::Invalid)?;
        let _permit = self
            .capacity
            .clone()
            .try_acquire_owned()
            .map_err(|_| McpCallError::Unavailable)?;
        let binding = operation.binding().clone();
        if intent.binding() != &binding
            || intent.identity().expires_at_ms
                <= super::now_ms().map_err(|_| McpCallError::Invalid)?
        {
            return Err(McpCallError::Invalid);
        }
        let reply_budget = mcp_reply_budget(intent).map_err(|_| McpCallError::Invalid)?;
        let app = intent.app();
        let _turn = crate::mcp::apps::App::turn(app, async {
            tokio::select! {
                biased;
                _ = &mut cancelled => (),
                _ = self.stop.cancelled() => (),
            }
        })
        .await;
        let response = connection
            .perform_once(
                operation,
                async {
                    tokio::select! {
                        biased;
                        _ = &mut cancelled => (),
                        _ = self.stop.cancelled() => (),
                    }
                },
                |seen| seen == &binding && !self.stop.is_set() && authorized(seen),
            )
            .await;
        let terminal = match response {
            Ok(reply) => match results.project_reply_bounded(
                &self.binding,
                Source {
                    server: binding.item.server_id.clone(),
                    origin: SourceOrigin::ModelCall {
                        model_call: intent.identity().model_call_id.clone(),
                    },
                    result_generation: intent.identity().operation_id.clone(),
                },
                reply,
                Instant::now(),
                reply_budget,
            ) {
                Ok(reply) => Terminal::Reply(reply),
                Err(_) => {
                    Terminal::Failure(OperationFailure::OutcomeUnknown(polaris_mcp::Error::Limit))
                }
            },
            Err(failure) => Terminal::Failure(failure),
        };
        let input = match &terminal {
            Terminal::Reply(reply) => McpTerminalInput::Reply(reply),
            Terminal::Failure(failure) => McpTerminalInput::Failure(*failure),
        };
        local_mcp_terminal(intent, input)
            .or_else(|_| {
                local_mcp_terminal(
                    intent,
                    McpTerminalInput::Failure(OperationFailure::OutcomeUnknown(
                        polaris_mcp::Error::Limit,
                    )),
                )
            })
            .map_err(|_| McpCallError::Storage)
    }
}

enum Phase {
    Awaiting,
    Granted,
    Publication,
    Fault,
    Done,
}
enum Operation {
    Model(McpOperation),
    Prompt(McpPromptIntent),
}
enum Publication {
    Model(McpTerminalPublication),
    Prompt(McpPromptPublication),
}
struct Entry {
    request: Request,
    operation: Operation,
    policy_revision: DecimalU64,
    approval: Option<PendingApproval>,
    operation_id: OperationId,
    deadline: StartDeadline,
    reply_budget: usize,
    decision: Option<ApprovalDecision>,
    phase: Phase,
    publication: Option<Publication>,
}

pub(super) struct McpRelay {
    port: McpPort,
    incoming: mpsc::Receiver<Queued>,
    target: RunTarget,
    session: SessionId,
    gate: Gate,
    ceiling: ExternalCeiling,
    disconnected: Arc<AtomicBool>,
    entry: Option<Entry>,
    next_operation: u64,
    unknown: bool,
    interrupted: bool,
}

impl McpRelay {
    pub(super) fn new(
        target: &RunTarget,
        session: SessionId,
        ceiling: ExternalCeiling,
        policy: ApprovalPolicy,
        disconnected: Arc<AtomicBool>,
    ) -> (Self, McpPort) {
        let (tx, incoming) = mpsc::channel(1);
        let port = McpPort {
            tx: Some(tx),
            local_ceiling: None,
            capacity: Arc::new(Semaphore::new(1)),
            stop: Arc::new(Stop::default()),
            binding: RunBinding {
                session: session.as_str().into(),
                run: target.run_id.as_str().into(),
                attempt: target.attempt_id.as_str().into(),
            },
        };
        (
            Self {
                port: port.clone(),
                incoming,
                target: target.clone(),
                session,
                gate: Gate::new(policy),
                ceiling,
                disconnected,
                entry: None,
                next_operation: 0,
                unknown: false,
                interrupted: false,
            },
            port,
        )
    }

    pub(super) fn cancel(&mut self) {
        self.port.stop.set();
        self.incoming.close();
    }
    pub(super) fn approval_resolved(&mut self, id: &ApprovalId, decision: ApprovalDecision) {
        if let Some(entry) = &mut self.entry
            && entry
                .approval
                .as_ref()
                .is_some_and(|approval| approval.approval_id == *id)
            && matches!(entry.phase, Phase::Awaiting)
        {
            entry.decision = Some(decision);
        }
    }
    pub(super) fn awaiting_approval(&self) -> bool {
        self.entry
            .as_ref()
            .is_some_and(|entry| entry.approval.is_some() && matches!(entry.phase, Phase::Awaiting))
    }
    pub(super) fn quiescent(&self) -> bool {
        self.entry.is_none()
            && self.incoming.is_empty()
            && self.port.capacity.available_permits() == 1
    }
    pub(super) fn terminal_stop(&self) -> bool {
        self.unknown || self.interrupted
    }
    pub(super) fn observation(&self, requested: Observation) -> Observation {
        if self.unknown {
            Observation::OutcomeUnknown
        } else if self.interrupted && requested != Observation::OutcomeUnknown {
            Observation::Interrupted
        } else {
            requested
        }
    }

    pub(super) fn step(
        &mut self,
        store: &mut Writer,
        target: &RunTarget,
        published: &Published,
        events: &mut Vec<EventBody>,
    ) -> Result<(), StoreError> {
        if target != &self.target || published.marker.session_id != self.session {
            self.cancel();
            return Err(StoreError::TargetMismatch);
        }
        let current = published.state.policy_revision;
        let run_matches = published.state.runs.iter().any(|run| {
            run.run.run_id == target.run_id
                && run.run.attempt_id == target.attempt_id
                && !run.run.state.is_terminal()
                && run.policy_revision == current
        });
        if self.disconnected.load(Ordering::Acquire) || !run_matches {
            self.cancel();
        }
        if let Some(mut entry) = self.entry.take() {
            let outcome = self.advance(&mut entry, store, current, events);
            if outcome.is_err() {
                entry.phase = Phase::Fault;
                self.interrupted |= matches!(&entry.operation, Operation::Prompt(_));
                reject(&mut entry.request, McpCallError::Storage);
                self.cancel();
            }
            if !matches!(entry.phase, Phase::Done) {
                self.entry = Some(entry);
            }
            outcome?;
        }
        if self.entry.is_none()
            && let Ok(queued) = self.incoming.try_recv()
        {
            let mut request = match queued {
                Queued::Operation(request) => *request,
                Queued::History { reply, _permit } => {
                    if reply.is_closed() {
                        return Ok(());
                    }
                    if self.port.stopped() {
                        let _ = reply.send(Err(McpCallError::Cancelled));
                        return Ok(());
                    }
                    return match store.project_request_history(&self.target) {
                        Ok(history) => {
                            let _ = reply.send(Ok(history));
                            Ok(())
                        }
                        Err(error) => {
                            let _ = reply.send(Err(McpCallError::Storage));
                            Err(error)
                        }
                    };
                }
            };
            if self.port.stop.is_set() || request.shared.abandoned.load(Ordering::Acquire) {
                reject(&mut request, McpCallError::Cancelled);
                return Ok(());
            }
            self.next_operation = self
                .next_operation
                .checked_add(1)
                .ok_or(StoreError::Overflow)?;
            let key = crate::conversation_state::content_hash(&serde_json::to_vec(&(
                "mcp-operation-v1",
                &self.target,
                self.next_operation,
            ))?);
            let operation_id = OperationId::new(format!("mcp-{key}"))?;
            let deadline = StartDeadline {
                unix_ms: now_ms()?
                    .checked_add(APPROVAL_TTL_MS)
                    .ok_or(StoreError::Overflow)?,
                monotonic: Instant::now() + Duration::from_millis(APPROVAL_TTL_MS),
            };
            let (operation, approval, reply_budget) = match &request.kind {
                RequestKind::Model {
                    model_call,
                    target: project,
                    meta_arguments_hash,
                    arguments,
                    app,
                    ..
                } => {
                    let identity = HostOperationIdentity {
                        run_id: target.run_id.as_str().into(),
                        attempt_id: target.attempt_id.as_str().into(),
                        model_call_id: model_call.clone(),
                        operation_id: operation_id.as_str().into(),
                        policy_revision: current.get(),
                        expires_at_ms: deadline.unix_ms,
                    };
                    let operation = if let Some(hash) = meta_arguments_hash {
                        if project != published.marker.project_id.as_str() {
                            reject(&mut request, McpCallError::Invalid);
                            return Ok(());
                        }
                        McpOperation::from_resource_binding(
                            identity,
                            project.clone(),
                            request.binding.clone(),
                            hash.clone(),
                        )
                    } else {
                        match McpOperation::from_binding(
                            identity,
                            project.clone(),
                            request.binding.clone(),
                        )
                        .and_then(|operation| operation.with_arguments(arguments, *app))
                        {
                            Ok(operation) => Ok(operation),
                            Err(_) => {
                                reject(&mut request, McpCallError::Invalid);
                                return Ok(());
                            }
                        }
                    }
                    .map_err(|_| StoreError::RunConflict)?;
                    let reply_budget = mcp_reply_budget(&operation)?;
                    let approval = if operation.is_resource_read() {
                        None
                    } else {
                        let display = match self.gate.owned_mcp_approval(self.ceiling, &operation) {
                            Ok(Some(display)) => display,
                            _ => {
                                reject(&mut request, McpCallError::Denied);
                                return Ok(());
                            }
                        };
                        Some(PendingApproval {
                            approval_id: ApprovalId::new(format!(
                                "approval-{}",
                                operation_id.as_str()
                            ))?,
                            run_id: target.run_id.clone(),
                            attempt_id: target.attempt_id.clone(),
                            operation_id: operation_id.clone(),
                            operation: MCP_TOOL_OPERATION.into(),
                            scope: operation.target().into(),
                            payload_hash: operation.payload_hash().into(),
                            policy_revision: current,
                            expires_at_unix_ms: DecimalU64::new(deadline.unix_ms),
                            state: PendingApprovalState::Pending,
                            display: ApprovalDisplay {
                                title: display.title.into(),
                                description: display.description,
                                choices: vec![ApprovalDecision::Allow, ApprovalDecision::Deny],
                            },
                        })
                    };
                    (Operation::Model(operation), approval, reply_budget)
                }
                RequestKind::HostPrompt { project } => {
                    let intent = if project == published.marker.project_id.as_str() {
                        store.prepare_mcp_prompt_intent(
                            target,
                            operation_id.clone(),
                            &request.binding,
                            deadline.unix_ms,
                        )
                    } else {
                        Err(StoreError::TargetMismatch)
                    };
                    let intent = match intent {
                        Ok(intent) => intent,
                        Err(error) => {
                            self.interrupted = true;
                            reject(&mut request, McpCallError::Invalid);
                            self.cancel();
                            return Err(error);
                        }
                    };
                    let reply_budget = intent.reply_budget()?;
                    (Operation::Prompt(intent), None, reply_budget)
                }
            };
            // Keep the request through ambiguous publication. A fault can
            // neither grant a retry nor make the owner quiescent.
            self.entry = Some(Entry {
                request,
                operation,
                policy_revision: current,
                approval: approval.clone(),
                operation_id,
                deadline,
                reply_budget,
                decision: None,
                phase: if approval.is_some() {
                    Phase::Fault
                } else {
                    Phase::Awaiting
                },
                publication: None,
            });
            if let Some(approval) = approval {
                if let Err(error) = store.publish_approval(approval.clone(), now_ms()?) {
                    reject(
                        &mut self.entry.as_mut().unwrap().request,
                        McpCallError::Storage,
                    );
                    self.cancel();
                    return Err(error);
                }
                self.entry.as_mut().unwrap().phase = Phase::Awaiting;
                events.push(EventBody::ApprovalRequested(approval));
            }
        }
        Ok(())
    }

    fn advance(
        &mut self,
        entry: &mut Entry,
        store: &mut Writer,
        current: DecimalU64,
        events: &mut Vec<EventBody>,
    ) -> Result<(), StoreError> {
        if matches!(entry.phase, Phase::Fault) {
            return Err(StoreError::RecoveryRequired);
        }
        if entry.policy_revision != current {
            self.cancel();
        }
        if matches!(entry.phase, Phase::Awaiting) {
            let cancelled =
                self.port.stop.is_set() || entry.request.shared.abandoned.load(Ordering::Acquire);
            if cancelled || !entry.deadline.valid() {
                if let Some(approval) = &entry.approval {
                    store.invalidate_approval(&approval.approval_id)?;
                    events.push(EventBody::ApprovalExpired(ApprovalExpired {
                        approval_id: approval.approval_id.clone(),
                        reason: if entry.policy_revision != current {
                            ExpiryReason::PolicyRevoked
                        } else if cancelled {
                            ExpiryReason::Cancelled
                        } else {
                            ExpiryReason::Expired
                        },
                    }));
                }
                reject(&mut entry.request, McpCallError::Cancelled);
                entry.phase = Phase::Done;
                return Ok(());
            }
            if entry.approval.is_some() {
                let Some(decision) = entry.decision else {
                    return Ok(());
                };
                if decision == ApprovalDecision::Deny {
                    reject(&mut entry.request, McpCallError::Denied);
                    entry.phase = Phase::Done;
                    return Ok(());
                }
            }
            let receipt = match (&entry.operation, &entry.request.kind, &entry.approval) {
                (
                    Operation::Model(operation),
                    RequestKind::Model { suffix, .. },
                    Some(approval),
                ) => store.consume_mcp_approval_intent(
                    &self.target,
                    &approval.approval_id,
                    operation,
                    suffix.clone(),
                    now_ms()?,
                ),
                (Operation::Model(operation), RequestKind::Model { suffix, .. }, None) => {
                    store.record_mcp_read_intent(&self.target, operation, suffix.clone(), now_ms()?)
                }
                (Operation::Prompt(intent), RequestKind::HostPrompt { .. }, None) => {
                    store.begin_mcp_prompt(intent, now_ms()?)
                }
                _ => return Err(StoreError::RunConflict),
            };
            match receipt {
                Ok(IntentReceipt::NewlyPublished) => {
                    entry.phase = Phase::Granted;
                    if let Some(reply) = entry.request.grant.take() {
                        let _ = reply.send(Ok(Grant {
                            binding: entry.request.binding.clone(),
                            operation: entry.operation_id.clone(),
                            reply_budget: entry.reply_budget,
                            deadline: StartDeadline {
                                unix_ms: entry.deadline.unix_ms,
                                monotonic: entry.deadline.monotonic,
                            },
                        }));
                    }
                }
                Ok(IntentReceipt::AlreadyRecorded) => return Err(StoreError::RunConflict),
                Err(_) => {
                    // No grant has been issued. Confirm presence only; never
                    // repeat consume or dispatch after an ambiguous intent ACK.
                    if matches!(&entry.operation, Operation::Prompt(_)) {
                        // Stop before readback, including a missing publication.
                        self.interrupted = true;
                        if let Some(reply) = entry.request.grant.take() {
                            let _ = reply.send(Err(McpCallError::Storage));
                        }
                        self.cancel();
                    }
                    let confirmed = match (&entry.operation, &entry.request.kind, &entry.approval) {
                        (
                            Operation::Model(operation),
                            RequestKind::Model { suffix, .. },
                            Some(approval),
                        ) => store.confirm_mcp_intent(
                            &self.target,
                            &approval.approval_id,
                            operation,
                            suffix,
                        )?,
                        (Operation::Model(operation), RequestKind::Model { suffix, .. }, None) => {
                            store.confirm_mcp_read_intent(&self.target, operation, suffix)?
                        }
                        (Operation::Prompt(intent), RequestKind::HostPrompt { .. }, None) => {
                            store.confirm_mcp_prompt_intent(intent)?
                        }
                        _ => return Err(StoreError::RunConflict),
                    };
                    if !confirmed {
                        reject(&mut entry.request, McpCallError::Storage);
                        entry.phase = Phase::Done;
                        return Ok(());
                    }
                    *entry
                        .request
                        .shared
                        .terminal
                        .lock()
                        .map_err(|_| StoreError::RecoveryRequired)? = Some(Terminal::Failure(
                        OperationFailure::BeforeSend(polaris_mcp::Error::Storage),
                    ));
                    entry.phase = Phase::Granted;
                    // The client waits for the result without ever receiving a
                    // usable grant. A separate grant error releases that wait.
                    if let Some(reply) = entry.request.grant.take() {
                        let _ = reply.send(Err(McpCallError::Storage));
                    }
                }
            }
        }
        if matches!(entry.phase, Phase::Granted) {
            let mut terminal = entry
                .request
                .shared
                .terminal
                .lock()
                .map_err(|_| StoreError::RecoveryRequired)?
                .take();
            if terminal.is_none() && entry.request.shared.abandoned.load(Ordering::Acquire) {
                terminal = Some(Terminal::Failure(
                    if entry.request.progress.may_have_sent() {
                        OperationFailure::OutcomeUnknown(polaris_mcp::Error::Cancelled)
                    } else {
                        OperationFailure::BeforeSend(polaris_mcp::Error::Cancelled)
                    },
                ));
            }
            let Some(terminal) = terminal else {
                return Ok(());
            };
            let input = match &terminal {
                Terminal::Reply(reply) => McpTerminalInput::Reply(reply),
                Terminal::Failure(error) => McpTerminalInput::Failure(*error),
            };
            let prepare = |input| match &entry.operation {
                Operation::Model(_) => store
                    .prepare_mcp_terminal(&self.target, &entry.operation_id, input)
                    .map(Publication::Model),
                Operation::Prompt(intent) => store
                    .prepare_mcp_prompt_terminal(intent, input)
                    .map(Publication::Prompt),
            };
            let publication = prepare(input).or_else(|_| {
                prepare(McpTerminalInput::Failure(OperationFailure::OutcomeUnknown(
                    polaris_mcp::Error::Limit,
                )))
            })?;
            entry.publication = Some(publication);
            entry.phase = Phase::Publication;
        }
        if matches!(entry.phase, Phase::Publication) {
            let publication = entry
                .publication
                .as_ref()
                .ok_or(StoreError::RecoveryRequired)?;
            let result = match publication {
                Publication::Model(publication) => {
                    PublishedResult::Model(match store.publish_mcp_terminal(publication) {
                        Ok(result) => result,
                        Err(_) => store.confirm_mcp_terminal(publication)?,
                    })
                }
                Publication::Prompt(publication) => {
                    PublishedResult::Prompt(match store.publish_mcp_prompt_terminal(publication) {
                        Ok(result) => result,
                        Err(_) => store.confirm_mcp_prompt_terminal(publication)?,
                    })
                }
            };
            self.unknown |= result.disposition() == McpDisposition::OutcomeUnknown;
            self.interrupted |= !result.permits_continuation()
                && result.disposition() != McpDisposition::OutcomeUnknown;
            if !result.permits_continuation() {
                self.cancel();
            }
            if let Some(reply) = entry.request.result.take() {
                let _ = reply.send(Ok(result));
            }
            entry.phase = Phase::Done;
        }
        Ok(())
    }
}

fn reject(request: &mut Request, error: McpCallError) {
    if let Some(reply) = request.grant.take() {
        let _ = reply.send(Err(error));
    }
    if let Some(reply) = request.result.take() {
        let _ = reply.send(Err(error));
    }
}

#[cfg(test)]
mod tests;
