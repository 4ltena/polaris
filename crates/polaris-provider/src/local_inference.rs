//! Inference through an explicitly selected loopback runtime.
//!
//! A selection fixes the observed model name and endpoint, not content identity
//! or on-device execution. Remote/Unknown location evidence remains unchanged.
//! Slot ownership, revalidation and runtime stop acknowledgement belong to the
//! caller; dropping a request future does not prove server-side generation stopped.

use crate::local::{Capability, ModelSelection};
use crate::openai::OpenAiProvider;
use crate::web_search::{WebSearchConfig, WebSearchPolicy};
use crate::{CompletionRequest, CompletionResponse, Provider, ProviderError};

/// One immutable selection per execution. The inner mutable cloud-compatible
/// transport is private and its setters are never forwarded. Construct a new
/// wrapper to change selection; the Provider setters retain their no-op defaults.
pub struct LocalInferenceProvider {
    selection: ModelSelection,
    transport: OpenAiProvider,
}

impl LocalInferenceProvider {
    pub fn new(selection: ModelSelection) -> Result<Self, ProviderError> {
        Self::with_web_search(selection, WebSearchConfig::disabled())
    }

    pub fn with_web_search(
        selection: ModelSelection,
        web_search: WebSearchConfig,
    ) -> Result<Self, ProviderError> {
        if web_search.policy != WebSearchPolicy::Disabled {
            return Err(ProviderError::Unsupported("local hosted web search".into()));
        }
        if selection.model().capabilities().completion == Capability::Unsupported {
            return Err(ProviderError::Unsupported("local completion".into()));
        }
        let transport = OpenAiProvider::for_local(&selection)?;
        Ok(Self {
            selection,
            transport,
        })
    }

    pub fn selection(&self) -> &ModelSelection {
        &self.selection
    }
}

#[async_trait::async_trait]
impl Provider for LocalInferenceProvider {
    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, ProviderError> {
        // Unknown completion may be attempted as an explicit user selection, but
        // is never upgraded to Supported. Function tools require positive evidence.
        if (!req.tools.is_empty()
            || req
                .messages
                .iter()
                .any(|m| !m.tool_calls.is_empty() || m.tool_call_id.is_some()))
            && self.selection.model().capabilities().tools != Capability::Supported
        {
            return Err(ProviderError::Unsupported(
                "local tools require confirmed support".into(),
            ));
        }
        if req.messages.iter().any(|m| !m.hosted_web_search.is_empty()) {
            return Err(ProviderError::Unsupported(
                "local hosted web search history".into(),
            ));
        }
        self.transport.complete(req).await
    }
}

#[cfg(test)]
#[path = "local_inference/tests.rs"]
mod tests;
