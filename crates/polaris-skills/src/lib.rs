//! Loading of skills that conform to the Agent Skills specification. Adds no frontmatter fields of its own.

pub mod agent_type;
pub mod discovery;
pub mod frontmatter;

use std::path::PathBuf;

pub use agent_type::{
    AgentAccess, AgentType, AgentTypeError, DiscoveredAgentTypes, SkipCauseAgentType,
    SkippedAgentType, discover_agent_types, discover_agent_types_in,
};
pub use discovery::{Discovered, SkipCause, Skipped, discover, discover_in};
pub use frontmatter::SkillError;

/// One loaded skill.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub path: PathBuf,
}
