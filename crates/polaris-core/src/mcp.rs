//! Run-owned MCP connections, request context and local result retention.
//! Registration and durable operation authority remain with the trusted host.

pub mod apps;
pub mod credentials;
pub mod lease;
pub mod local;
pub mod operation;
pub mod preselection;
pub mod results;
pub mod runtime;
mod selection;
pub mod setup;

pub use selection::{Candidate, CatalogView, PendingSelection, RequestSnapshot, SelectedIdentity};

#[derive(Debug, Clone, Copy, Eq, PartialEq, thiserror::Error)]
pub enum Error {
    #[error("mcp_catalog_unavailable")]
    CatalogUnavailable,
    #[error("mcp_selection_stale")]
    SelectionStale,
    #[error("mcp_invalid_selection")]
    InvalidSelection,
    #[error("mcp_context_limit")]
    ContextLimit,
    #[error("mcp_invalid_schema")]
    InvalidSchema,
    #[error("mcp_invalid_arguments")]
    InvalidArguments,
    #[error("result_unavailable")]
    ResultUnavailable,
}
