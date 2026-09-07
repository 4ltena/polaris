//! Entry point for polaris-core. Ties together the budget, constitution, and prompt modules.

pub mod agent;
pub mod approval;
pub mod audit;
pub mod budget;
pub mod compaction;
pub mod config;
pub mod constitution;
pub mod conversation_memory;
pub mod conversation_state;
mod dir_watch;
mod events;
mod files_md;
mod gitignore;
pub mod project;
pub mod prompt;
pub mod secret_screen;
pub mod session;
pub mod session_store;
pub mod spawn;
pub mod stop;
pub mod strict_provider;
pub mod tool_memory;
pub mod workflow;

pub use events::{AgentEvent, Diff, DiffHunk, DiffLine, compute_diff};
