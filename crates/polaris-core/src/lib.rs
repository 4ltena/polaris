//! Entry point for polaris-core. Ties together the budget, constitution, and prompt modules.

pub mod activity;
pub mod agent;
pub mod answer_finalizer;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod apply_intent;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod apply_owner;
pub mod approval;
pub mod audit;
pub mod budget;
pub mod build_cache;
#[cfg(target_os = "macos")]
pub mod clef;
pub mod common_tests;
pub mod compaction;
pub mod computer;
pub mod config;
pub mod constitution;
pub mod conversation_memory;
pub mod conversation_state;
pub mod decision_bands;
pub mod decision_capacity;
pub mod decision_diagnostics;
pub mod decision_policy;
pub mod decisions;
pub mod desktop_approval;
pub mod desktop_events;
pub mod desktop_execution;
pub mod desktop_memory;
pub mod desktop_response;
#[cfg(any(target_os = "macos", target_os = "linux"))]
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
pub mod managed_decision;
#[cfg(any(target_os = "macos", target_os = "linux", windows))]
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
pub mod project_mutation;
pub mod prompt;
pub mod recipe;
pub mod request_input;
pub mod resume;
#[cfg(unix)]
pub mod rewind_admission;
#[cfg(unix)]
pub mod rewind_candidate_capacity;
#[cfg(unix)]
pub mod rewind_workspace;
pub mod scratch_registry;
pub mod secret_screen;
pub mod session;
pub mod session_manager;
pub mod session_settings;
pub mod session_store;
#[cfg(unix)]
pub mod session_switch;
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
#[cfg(windows)]
mod windows_fs;
pub mod workflow;
pub mod workflow_artifact_store;

pub use events::{AgentEvent, Diff, DiffHunk, DiffLine, compute_diff};
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod handoff;
pub mod isolated_run;
#[cfg(any(unix, windows))]
pub mod isolated_workspace;
#[cfg(target_os = "linux")]
pub mod linux_l04_parent;
pub mod llama_server;
pub mod local_execution;
pub mod management_effect;
pub mod mcp;
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod workspace_apply;
#[cfg(windows)]
#[path = "workspace_apply/report.rs"]
pub mod workspace_apply;

#[cfg(unix)]
pub mod sandbox_command;
