//! Trusted initial request inputs; no mutable MCP runtime is stored in Session.
use super::*;
use crate::{
    desktop_store::OwnerRequestHistory,
    execution_owner::McpPort,
    mcp::runtime::{RunRuntime, RuntimeConfig},
};
use polaris_desktop_protocol::request::McpPromptChoice;
use polaris_provider::Message;

pub enum DesktopRequestContext {
    Prepared(OwnerRequestHistory),
    Mcp {
        config: Box<RuntimeConfig>,
        port: McpPort,
        prompt: Option<McpPromptChoice>,
        preselection: McpPreselection,
    },
}

pub use crate::mcp::preselection::Context as McpPreselection;

impl DesktopRequestContext {
    pub(super) fn validate(&self, target: &RunTarget) -> Result<(), DesktopRunError> {
        let matches = match self {
            Self::Prepared(history) => history.target() == target,
            Self::Mcp { port, .. } => {
                port.binding().run == target.run_id.as_str()
                    && port.binding().attempt == target.attempt_id.as_str()
            }
        };
        if !matches {
            return Err(DesktopRunError::ExecutionMismatch);
        }
        Ok(())
    }

    pub(super) fn runtime(&self) -> Result<Option<RunRuntime>, DesktopRunError> {
        match self {
            Self::Prepared(_) => Ok(None),
            Self::Mcp { config, port, .. } => {
                Ok(Some(RunRuntime::new((**config).clone(), port.clone())?))
            }
        }
    }

    pub(super) async fn prepare(
        &self,
        session: &mut Session,
        raw_history: &mut Vec<Message>,
        runtime: Option<&mut RunRuntime>,
        control: &EventControl,
    ) -> Result<String, DesktopRunError> {
        if let Some(failure) = control.failure() {
            return Err(AgentError::Desktop(failure).into());
        }
        match self {
            Self::Prepared(history) => Ok(install(session, history)),
            Self::Mcp {
                prompt,
                preselection,
                ..
            } => {
                let runtime = runtime.ok_or(DesktopRunError::ExecutionMismatch)?;
                if let Some(prompt) = prompt {
                    let response = runtime
                        .host_prompt(prompt, control.cancelled())
                        .await
                        .map_err(|error| preparation_error(error, control))?;
                    let confirmed = response.permits_continuation();
                    // ACKed external data is durable raw, not assistant/tool output.
                    raw_history.push(response.message);
                    if !confirmed {
                        return Err(DesktopRunError::PromptUnconfirmed);
                    }
                }
                let history = runtime
                    .request_history(control.cancelled())
                    .await
                    .map_err(|error| preparation_error(error, control))?;
                let phase = session
                    .workflow
                    .as_ref()
                    .map(|w| w.state().phase.as_str())
                    .unwrap_or("");
                preselection
                    .prepare(runtime, history.query(), phase, control.cancelled())
                    .await;
                Ok(install(session, &history))
            }
        }
    }
}

fn preparation_error(error: crate::mcp::runtime::Error, control: &EventControl) -> DesktopRunError {
    use crate::{desktop_events::EventFailure, execution_owner::McpCallError, mcp::runtime::Error};
    match error {
        Error::Owner(McpCallError::Cancelled) | Error::Transport(polaris_mcp::Error::Cancelled) => {
            AgentError::Desktop(control.failure().unwrap_or(EventFailure::Cancelled)).into()
        }
        // A simultaneous cancellation must not hide a saved owner failure.
        other => other.into(),
    }
}

fn install(session: &mut Session, history: &OwnerRequestHistory) -> String {
    session.messages = history.messages().to_vec();
    history.query().to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{desktop_events::EventFailure, execution_owner::McpCallError, mcp::runtime::Error};

    #[test]
    fn cancellation_mapping_keeps_owner_failures_and_receiver_loss_distinct() {
        let (_sink, receiver, control) = DesktopEventSink::channel(
            polaris_desktop_protocol::ids::RunId::new("error-mapping").unwrap(),
        );
        drop(receiver);
        assert!(matches!(
            preparation_error(Error::Owner(McpCallError::Cancelled), &control),
            DesktopRunError::Agent(AgentError::Desktop(EventFailure::ReceiverDropped))
        ));
        for error in [McpCallError::Storage, McpCallError::Invalid] {
            assert!(matches!(
                preparation_error(Error::Owner(error), &control),
                DesktopRunError::Mcp(Error::Owner(McpCallError::Storage | McpCallError::Invalid))
            ));
        }
        assert!(matches!(
            preparation_error(Error::Transport(polaris_mcp::Error::Transport), &control),
            DesktopRunError::Mcp(Error::Transport(polaris_mcp::Error::Transport))
        ));
    }
}
