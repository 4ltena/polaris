//! Host environment setup. Source admission uses the same protected,
//! descriptor-based walker as isolated workspaces and rewind.
pub mod capture;
pub mod native;
pub mod owner;
pub mod recovery;
pub mod tool;
