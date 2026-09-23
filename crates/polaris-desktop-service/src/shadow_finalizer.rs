//! Compatibility surface for the shared answer-finalization owner.
//!
//! Desktop and terminal hosts use the same bounded worker and body-free
//! journal; only construction and capture of host evidence remain host-owned.

pub use polaris_core::answer_finalizer::shadow::*;
