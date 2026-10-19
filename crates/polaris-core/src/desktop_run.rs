//! Owned real-core worker for the trusted desktop engine (macOS only).
//!
//! The engine retains this owner and drains its bounded events until `try_join`
//! returns a completion. That is not proof of remote inference termination or
//! service-owned tool cleanup/persistence; those remain engine responsibilities.
use futures_util::FutureExt;
use std::sync::Arc;
use std::thread::JoinHandle;

use polaris_desktop_protocol::request::RunTarget;
use polaris_provider::Provider;
use tokio::sync::Mutex;

use crate::agent::{AgentError, AgentOutcome, ToolContext};
use crate::audit::AuditLog;
use crate::desktop_events::{DesktopEventReceiver, DesktopEventSink, EventControl};
use crate::desktop_execution::ExecutionPort;
use crate::isolated_run::PreparedWorkspace;
use crate::session::Session;

mod context;
pub use context::{DesktopRequestContext, McpPreselection};

pub const MAX_TURNS: u32 = 1024;
pub const MAX_CONCURRENCY: usize = 32;

/// Trusted, already resolved inputs. No paths are opened for audit/history,
/// credentials are not resolved, and the supplied execution port is not replaced.
/// The parent must meter both providers outside this worker for error accounting.
/// `session` must be a working copy of the v3 owner's already published history,
/// including the accepted user turn. Retain that durable source: a validation or
/// thread-creation error consumes this input and does not return its session.
pub struct DesktopRunInput {
    pub target: RunTarget,
    pub prepared: Arc<PreparedWorkspace>,
    pub execution: ExecutionPort,
    pub provider: Arc<dyn Provider>,
    pub provider_pool: Arc<dyn Provider>,
    pub session: Session,
    pub request_context: DesktopRequestContext,
    pub always_on: crate::prompt::AlwaysOn,
    pub skills: Vec<polaris_skills::Skill>,
    pub agent_types: Vec<polaris_skills::AgentType>,
    pub max_turns: u32,
    pub spawn_concurrency: usize,
    pub spawn_write_concurrency: usize,
    pub audit: Arc<Mutex<AuditLog>>,
}

#[derive(Debug, thiserror::Error)]
pub enum DesktopRunError {
    #[error("desktop run execution binding mismatch")]
    ExecutionMismatch,
    #[error("desktop run requires in-memory history without v2 hooks")]
    LegacySession,
    #[error("desktop run limits are invalid")]
    InvalidLimits,
    #[error("desktop worker thread creation failed: {0}")]
    Spawn(std::io::Error),
    #[error("desktop worker runtime creation failed: {0}")]
    Runtime(std::io::Error),
    #[error("desktop agent failed: {0}")]
    Agent(#[from] AgentError),
    #[error("desktop MCP preparation failed: {0}")]
    Mcp(#[from] crate::mcp::runtime::Error),
    #[error("desktop MCP prompt is not confirmed")]
    PromptUnconfirmed,
    #[error("desktop MCP shutdown is unconfirmed")]
    McpShutdown,
    #[error("project activity cannot start: {0}")]
    ProjectActivity(#[from] crate::project_mutation::ProjectMutationError),
    #[error("management effect state cannot be recovered: {0}")]
    ManagementEffect(#[from] crate::management_effect::ManagementEffectStoreError),
    #[error("an unresolved management effect blocks this run")]
    UnresolvedManagementEffect,
    // Never copy an arbitrary panic payload into a user-visible error.
    #[error("desktop worker panicked; no automatic retry")]
    Panicked,
}

/// Available only after the OS thread has joined. Partial history survives an
/// agent error/cancellation. An unwinding agent also returns its partial session;
/// a panic outside that boundary is returned by `try_join` as `Panicked`.
pub struct DesktopRunCompletion {
    pub session: Session,
    /// Full unmodified assistant/tool output. Host prompt supplements belong
    /// only to raw history and must never be inferred by stripping text here.
    pub turn_suffix: Vec<polaris_provider::Message>,
    pub result: Result<AgentOutcome, DesktopRunError>,
}

#[must_use = "retain the owner until try_join returns the worker result"]
pub struct DesktopRun {
    thread: Option<JoinHandle<DesktopRunCompletion>>,
    control: EventControl,
}

impl DesktopRun {
    pub fn start(input: DesktopRunInput) -> Result<(Self, DesktopEventReceiver), DesktopRunError> {
        validate(&input)?;
        let (sink, receiver, _control) = DesktopEventSink::channel(input.target.run_id.clone());
        Self::start_with_events(input, sink, receiver)
    }

    /// Starts with the owner's already-created event channel. The desktop
    /// service uses this to let memory and MCP preselection report through the
    /// same run stream before the main agent begins.
    #[doc(hidden)]
    pub fn start_with_events(
        mut input: DesktopRunInput,
        sink: DesktopEventSink,
        receiver: DesktopEventReceiver,
    ) -> Result<(Self, DesktopEventReceiver), DesktopRunError> {
        validate(&input)?;
        let sink = sink
            .with_execution(input.execution.clone())
            .map_err(|_| DesktopRunError::ExecutionMismatch)?;
        let control = sink.control();
        if let Some(decider) = input.session.decider.take() {
            input.session.decider = Some(sink.observe_decider(decider));
        }
        let thread = std::thread::Builder::new()
            .name("polaris-desktop-run".into())
            .spawn(move || execute(input, sink))
            .map_err(DesktopRunError::Spawn)?;
        Ok((
            Self {
                thread: Some(thread),
                control,
            },
            receiver,
        ))
    }

    pub fn control(&self) -> EventControl {
        self.control.clone()
    }

    /// Requests cancellation only; never joins or reports successful completion.
    pub fn cancel(&self) {
        self.control.cancel();
    }

    /// Non-waiting poll. Returns a result once, and only after thread join.
    /// `None` means either still running or the result was already taken.
    pub fn try_join(&mut self) -> Option<Result<DesktopRunCompletion, DesktopRunError>> {
        if !self.thread.as_ref()?.is_finished() {
            return None;
        }
        Some(
            self.thread
                .take()
                .expect("checked worker")
                .join()
                .map_err(|_| DesktopRunError::Panicked),
        )
    }
}

impl Drop for DesktopRun {
    fn drop(&mut self) {
        if self.thread.is_some() {
            self.cancel();
        }
        // Drop cannot block the engine on uncooperative provider/tool code.
        // std threads cannot be forcibly stopped: dropping an unjoined owner
        // detaches it and loses completion/history, NOT proof of shutdown.ready.
        // The thread itself retains PreparedWorkspace until its future/runtime
        // are gone. No global registry or unbounded reaper threads are created.
        // Correct engine shutdown must retain this owner and poll try_join.
    }
}

fn validate(input: &DesktopRunInput) -> Result<(), DesktopRunError> {
    input.request_context.validate(&input.target)?;
    let session = &input.session;
    if session.persistence.is_some()
        || session.persistence_error.is_some()
        || session.strict_history.is_some()
        || session.before_compact.is_some()
        || session.desktop_transcript.is_some()
    {
        return Err(DesktopRunError::LegacySession);
    }
    if !(1..=MAX_TURNS).contains(&input.max_turns)
        || !(1..=MAX_CONCURRENCY).contains(&input.spawn_concurrency)
        || !(1..=input.spawn_concurrency).contains(&input.spawn_write_concurrency)
    {
        return Err(DesktopRunError::InvalidLimits);
    }
    let readonly = input
        .prepared
        .policy()
        .restrict(polaris_sandbox::SandboxMode::ReadOnly, &[])
        .map_err(|_| DesktopRunError::ExecutionMismatch)?;
    if input.execution.run_id() != &input.target.run_id
        || !input
            .execution
            .read_helper_matches(input.prepared.helper_path(), &readonly)
    {
        return Err(DesktopRunError::ExecutionMismatch);
    }
    Ok(())
}

struct DenyApproval;
impl crate::approval::Approver for DenyApproval {
    fn ask(&mut self, _: &str) -> crate::approval::Decision {
        crate::approval::Decision::Deny
    }
}

fn execute(mut input: DesktopRunInput, sink: DesktopEventSink) -> DesktopRunCompletion {
    let mut raw_history = input.session.messages.clone();
    // Catch inside the owned input scope, preserving partial in-memory history.
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        validate(&input)?;
        let source_project = &input.prepared.snapshot().source_path;
        let project_owner =
            crate::project_mutation::ProjectMutationOwnerV1::for_project(source_project)?;
        let management_effects = {
            let lease = project_owner.try_mutation()?;
            let store =
                crate::management_effect::ManagementEffectStoreV1::open(source_project, &lease)?;
            crate::sync::composite::C10CompositeBlockerV1::require_clear_with_mutation(
                source_project,
                store.state_dir(),
                &lease,
            )
            .map_err(|_| DesktopRunError::UnresolvedManagementEffect)?;
            store
        };
        let _project_activity = project_owner.try_activity()?;
        if crate::sync::composite::C10CompositeBlockerV1::inspect_with_activity(
            source_project,
            management_effects.state_dir(),
            &_project_activity,
        )
        .map_err(|_| DesktopRunError::UnresolvedManagementEffect)?
        .is_some()
        {
            return Err(DesktopRunError::UnresolvedManagementEffect);
        }
        // A desktop owner may serve several runs. Bind this run's copy rather
        // than the host cwd or the intentionally host-bound AGENTS capability.
        input.always_on = input
            .always_on
            .clone()
            .with_project_root(input.prepared.snapshot().path())
            .map_err(AgentError::Io)?;
        input.session.desktop_transcript = Some(Default::default());
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(DesktopRunError::Runtime)?;
        let mut gate = crate::approval::Gate::new(crate::approval::ApprovalPolicy::Always);
        let mut approver = DenyApproval;
        let mut stop = crate::stop::StopTracker::new(input.max_turns);
        let mut ctx = ToolContext {
            sandbox: input.prepared.policy(),
            helper: input.prepared.helper_path(),
            gate: &mut gate,
            approver: &mut approver,
        };
        // Do not start legacy background files.md regeneration from a v3 run.
        input.session.disable_files_md_auto_regenerate = true;
        // v3's owner publishes the complete raw transcript after join. Legacy
        // in-place compaction would replace that transcript without an archive.
        input.session.compaction_threshold = Some(usize::MAX);
        input.session.compaction_retry_tokens = None;
        runtime.block_on(async {
            // Connections are owned outside the cancellable agent future,
            // but constructed and destroyed on this same live Tokio runtime.
            let mut mcp = input.request_context.runtime()?;
            let control = sink.control();
            let result = std::panic::AssertUnwindSafe(async {
                let query = input
                    .request_context
                    .prepare(&mut input.session, &mut raw_history, mcp.as_mut(), &control)
                    .await?;
                if let Some(status) = mcp.as_ref().and_then(|m| m.preselection_status()) {
                    input
                        .audit
                        .lock()
                        .await
                        .record(&crate::audit::Record {
                            tool: "mcp-preselection",
                            detail: status.as_str(),
                            sandbox: None,
                            target: None,
                            result: status.as_str(),
                            caller: "root",
                        })
                        .map_err(|e| DesktopRunError::Agent(AgentError::Io(e)))?;
                }
                crate::agent::run_desktop(
                    input.provider.as_ref(),
                    &mut input.session,
                    &query,
                    input.audit.clone(),
                    &mut stop,
                    &input.always_on,
                    &input.skills,
                    &input.agent_types,
                    input.provider_pool.clone(),
                    input.spawn_concurrency,
                    input.spawn_write_concurrency,
                    sink,
                    &mut ctx,
                    mcp.as_mut(),
                )
                .await
                .map_err(DesktopRunError::Agent)
            })
            .catch_unwind()
            .await
            .unwrap_or(Err(DesktopRunError::Panicked));
            let cleanup = std::panic::AssertUnwindSafe(async {
                if let Some(mcp) = &mut mcp {
                    mcp.shutdown()
                        .await
                        .map_err(|_| DesktopRunError::McpShutdown)?;
                }
                Ok(())
            })
            .catch_unwind()
            .await
            .unwrap_or(Err(DesktopRunError::McpShutdown));
            // On an error, ManagedStdio parks pending cleanup for the existing
            // owner to drain after join; dropping here is not a successful stop.
            drop(mcp);
            after_cleanup(result, cleanup)
        })
    }))
    .unwrap_or(Err(DesktopRunError::Panicked));
    if let Err(DesktopRunError::Agent(AgentError::Provider(error))) = &result {
        // Diagnostics contain only a closed category, never an upstream error
        // body, URL, account identifier or credential. Preserve the original
        // failure even if the best-effort diagnostic append itself fails.
        let _ = input.audit.blocking_lock().record(&crate::audit::Record {
            tool: "provider",
            detail: provider_failure_category(error),
            sandbox: None,
            target: None,
            result: "failed",
            caller: "root",
        });
    }
    let turn_suffix = if let Some(transcript) = input.session.desktop_transcript.take() {
        let messages = transcript.into_messages();
        raw_history.extend_from_slice(&messages);
        input.session.messages = raw_history;
        input.session.input_source = None;
        messages
    } else {
        Vec::new()
    };
    DesktopRunCompletion {
        session: input.session,
        turn_suffix,
        result,
    }
}

fn after_cleanup(
    result: Result<AgentOutcome, DesktopRunError>,
    cleanup: Result<(), DesktopRunError>,
) -> Result<AgentOutcome, DesktopRunError> {
    // An unconfirmed stop must not disappear behind an earlier cancellation.
    cleanup.and(result)
}

fn provider_failure_category(error: &polaris_provider::ProviderError) -> &'static str {
    use polaris_provider::ProviderError;
    match error {
        ProviderError::ReceivedUsage { source, .. } => provider_failure_category(source),
        ProviderError::Auth(message) => match message.as_str() {
            "authentication_protection" => "authentication_protection",
            "authentication_store" => "authentication_store",
            "authentication_store_busy" => "authentication_store_busy",
            "authentication_recovery_required" => "authentication_recovery_required",
            "authentication_transport_or_status" => "authentication_transport_or_status",
            "authentication_http_400" => "authentication_http_400",
            "authentication_http_401" => "authentication_http_401",
            "authentication_http_403" => "authentication_http_403",
            "authentication_http_429" => "authentication_http_429",
            "authentication_response_format" => "authentication_response_format",
            "authentication_denied" => "authentication_denied",
            "authentication_refresh_policy" => "authentication_refresh_policy",
            "authentication_login_port" => "authentication_login_port",
            _ => "authentication",
        },
        ProviderError::Http(_) => "http",
        ProviderError::Decode(_) => "response_format",
        ProviderError::Budget(_) => "request_budget",
        ProviderError::Unsupported(_) => "unsupported_capability",
        ProviderError::Observation { .. } => "response_delivery",
    }
}

#[cfg(test)]
mod failure_category_tests {
    use super::*;
    #[test]
    fn unconfirmed_shutdown_overrides_cancel_or_success_but_clean_shutdown_preserves_result() {
        use crate::desktop_events::EventFailure;
        let cancelled = || Err(AgentError::Desktop(EventFailure::Cancelled).into());
        assert!(matches!(
            after_cleanup(cancelled(), Err(DesktopRunError::McpShutdown)),
            Err(DesktopRunError::McpShutdown)
        ));
        assert!(matches!(
            after_cleanup(cancelled(), Ok(())),
            Err(DesktopRunError::Agent(AgentError::Desktop(
                EventFailure::Cancelled
            )))
        ));
        let success = Ok(AgentOutcome {
            text: "done".into(),
            usage: Default::default(),
            usage_report: Default::default(),
        });
        assert!(matches!(
            after_cleanup(success, Err(DesktopRunError::McpShutdown)),
            Err(DesktopRunError::McpShutdown)
        ));
        assert!(matches!(
            after_cleanup(Err(DesktopRunError::Panicked), Ok(())),
            Err(DesktopRunError::Panicked)
        ));
    }

    #[test]
    fn provider_diagnostic_never_returns_upstream_text() {
        use polaris_provider::ProviderError;
        let private = "synthetic-token account@example.invalid upstream response";
        for (error, expected) in [
            (
                ProviderError::Auth("authentication_store_busy".into()),
                "authentication_store_busy",
            ),
            (
                ProviderError::Auth("authentication_recovery_required".into()),
                "authentication_recovery_required",
            ),
            (
                ProviderError::Auth(format!("authentication_recovery_required {private}")),
                "authentication",
            ),
            (ProviderError::Auth(private.into()), "authentication"),
            (
                ProviderError::Auth("authentication_store".into()),
                "authentication_store",
            ),
            (
                ProviderError::Auth(format!("authentication_store {private}")),
                "authentication",
            ),
            (ProviderError::Http(private.into()), "http"),
            (ProviderError::Decode(private.into()), "response_format"),
            (ProviderError::Budget(private.into()), "request_budget"),
            (
                ProviderError::Unsupported(private.into()),
                "unsupported_capability",
            ),
        ] {
            assert_eq!(provider_failure_category(&error), expected);
        }
    }
}

#[cfg(test)]
mod tests;
