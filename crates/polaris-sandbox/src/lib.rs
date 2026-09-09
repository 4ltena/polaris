//! Sandbox policy definitions, and delegation to OS mechanisms.
//!
//! This crate does not implement the enforcement mechanism itself. It holds
//! the policy and the normalized writable roots, and merely hands them off
//! to platform-specific mechanisms.

mod broker;
mod child_environment;
mod child_fds;
pub mod confine;
mod controlled;
pub mod helper;
pub mod policy;
pub mod stage;

#[cfg(target_os = "macos")]
pub mod macos;

#[cfg(target_os = "linux")]
pub mod linux;

pub use confine::{Outcome, run_confined};
pub use controlled::{
    ControlledEnd, ControlledOutcome, PendingCleanup, run_confined_controlled,
    run_confined_controlled_authorized, take_pending_cleanups,
};
pub use helper::Mutation;
pub use policy::{SandboxMode, SandboxPolicy};

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("failed to apply the sandbox: {0}")]
    NotEnforced(String),
    #[error("denied: {path} (policy {policy})")]
    Denied { path: String, policy: String },
    #[error("I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("this platform has no enforcement delegate")]
    UnsupportedPlatform,
}
