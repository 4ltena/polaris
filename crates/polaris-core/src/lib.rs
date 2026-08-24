//! Entry point for polaris-core. Ties together the budget, constitution, and prompt modules.

pub mod agent;
pub mod approval;
pub mod audit;
pub mod budget;
pub mod config;
pub mod constitution;
mod dir_watch;
// Not yet called from outside the module — a later task wires it into the
// harness integration. Its own unit tests are the only caller today, and
// `cargo clippy --all-targets` still compiles the plain `lib` target
// (without `cfg(test)`) where none of that applies, so without this the
// module reads as entirely dead code.
#[allow(dead_code)]
mod files_md;
#[allow(dead_code)]
mod gitignore;
pub mod project;
pub mod prompt;
pub mod secret_screen;
pub mod session;
pub mod spawn;
pub mod stop;
