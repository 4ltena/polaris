//! Trusted OS account lookup shared with the MCP migration owner.
//! The public core API remains unchanged; no HOME fallback is introduced.
pub use polaris_mcp::account_home::{TrustedHomeError, trusted_user_home};
