//! Entry point for polaris-core. Ties together the budget, constitution, and prompt modules.

pub mod activity;
pub mod agent;
pub mod answer_finalizer;
pub mod approval;
pub mod audit;
pub mod budget;
pub mod build_cache;
pub mod common_tests;
pub mod compaction;
pub mod computer;
pub mod config;
pub mod constitution;
pub mod conversation_memory;
pub mod conversation_state;
pub mod decision_bands;
pub mod decisions;
pub mod desktop_approval;
pub mod desktop_events;
pub mod desktop_execution;
pub mod desktop_memory;
pub mod desktop_response;
#[cfg(target_os = "macos")]
pub mod desktop_run;
pub mod desktop_store;
pub mod desktop_workflow;
mod dir_watch;
mod error_region;
mod events;
pub mod execution_owner;
pub mod filemap;
mod files_md;
mod gitignore;
pub mod input_preparation;
#[cfg(target_os = "macos")]
pub mod managed_runtime;
pub mod measurement;
#[cfg(unix)]
pub mod os_home;
pub mod preselect;
pub mod preselection_corpus;
pub mod project;
pub mod project_analysis;
/// Unix only: the call binds to an open directory FD and spawns a child.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod project_git;
#[cfg(unix)]
pub mod project_mutation;
pub mod prompt;
pub mod recipe;
pub mod request_input;
pub mod resume;
pub mod scratch_registry;
pub mod secret_screen;
pub mod session;
pub mod session_manager;
pub mod session_store;
pub mod similarity;
pub mod spare;
pub mod spawn;
pub mod stop;
pub mod strict_provider;
pub mod subagent_control;
pub mod sync;
pub mod tier;
pub mod tool_memory;
pub mod translation;
pub mod turn_intent;
pub mod verification;
pub mod workflow;
pub mod workflow_artifact_store;

pub use events::{AgentEvent, Diff, DiffHunk, DiffLine, compute_diff};
#[cfg(target_os = "macos")]
pub mod handoff;
pub mod isolated_run;
#[cfg(unix)]
pub mod isolated_workspace;
pub mod llama_server;
pub mod local_execution;
#[cfg(unix)]
pub mod management_effect;
pub mod mcp;
#[cfg(target_os = "macos")]
pub mod workspace_apply;
