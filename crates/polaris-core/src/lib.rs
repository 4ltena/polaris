//! Entry point for polaris-core. Ties together the budget, constitution, and prompt modules.

pub mod agent;
pub mod approval;
pub mod audit;
pub mod budget;
pub mod config;
pub mod constitution;
mod dir_watch;
mod gitignore;
pub mod project;
pub mod prompt;
pub mod secret_screen;
pub mod session;
pub mod spawn;
pub mod stop;
