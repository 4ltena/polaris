//! Agent Skills 仕様に準拠した skill の読み込み。独自のフロントマターは足さない。

pub mod discovery;
pub mod frontmatter;

use std::path::PathBuf;

pub use discovery::{Discovered, SkipCause, Skipped, discover, discover_in};
pub use frontmatter::SkillError;

/// 読み込み済みの skill 1 件。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub body: String,
    pub path: PathBuf,
}
