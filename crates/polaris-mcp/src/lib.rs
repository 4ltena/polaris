//! Host-owned MCP registration and bounded client data.
//!
//! SDK types remain private to this crate. A registration permits a connection,
//! not an operation; callers must obtain a separate host operation grant.

#[cfg(unix)]
pub mod account_home;
#[cfg(unix)]
pub mod admission;
pub mod catalog;
pub mod content;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod entry_gate;
#[cfg(any(target_os = "macos", target_os = "linux"))]
mod native_mount;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub use native_mount::NativeMountIdentityV2;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod migration_observation;
pub mod registration;
pub mod secret_input;
pub mod store;
mod template;
#[cfg(feature = "test-support")]
pub mod test_support;
mod transport;
pub use transport::{
    BoundOperation, Connection, OperationBinding, OperationFailure, OperationProgress,
};

/// Closed error classes; upstream data, paths and credentials are never reflected.
#[derive(Debug, Clone, Copy, Eq, PartialEq, thiserror::Error)]
pub enum Error {
    #[error("mcp_invalid_registration")]
    InvalidRegistration,
    #[error("mcp_limit")]
    Limit,
    #[error("mcp_storage")]
    Storage,
    #[error("mcp_protection")]
    Protection,
    #[error("mcp_revision_conflict")]
    RevisionConflict,
    #[error("mcp_recovery_required")]
    RecoveryRequired,
    #[error("mcp_auth")]
    Auth,
    #[error("mcp_transport")]
    Transport,
    #[error("mcp_protocol")]
    Protocol,
    #[error("mcp_cancelled")]
    Cancelled,
    #[error("mcp_timeout")]
    Timeout,
    #[error("mcp_stop_unconfirmed")]
    StopUnconfirmed,
}
