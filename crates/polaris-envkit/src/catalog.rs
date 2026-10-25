//! Cheap catalog metadata. A name in this table is not an installation or
//! verification claim; exact resources are loaded only after profile selection.
use crate::version::Parser;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum ProfileId {
    Node,
    Nextjs,
    React,
    Vue,
    Svelte,
    Astro,
    Nuxt,
    Angular,
    Python,
    Rust,
    Go,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AdapterId {
    Npm,
    Python,
    Cargo,
    Go,
}

impl AdapterId {
    pub fn revision(self) -> u32 {
        1
    }
}

pub struct Entry {
    pub id: ProfileId,
    pub name: &'static str,
    pub description: &'static str,
    pub adapter: AdapterId,
    pub parser: Parser,
    pub platform: &'static str,
}

pub const ALL: [ProfileId; 11] = [
    ProfileId::Node,
    ProfileId::Nextjs,
    ProfileId::React,
    ProfileId::Vue,
    ProfileId::Svelte,
    ProfileId::Astro,
    ProfileId::Nuxt,
    ProfileId::Angular,
    ProfileId::Python,
    ProfileId::Rust,
    ProfileId::Go,
];

impl ProfileId {
    pub fn primary_package(self) -> Option<&'static str> {
        match self {
            Self::Nextjs => Some("next"),
            Self::React => Some("react"),
            Self::Vue => Some("vue"),
            Self::Svelte => Some("svelte"),
            Self::Astro => Some("astro"),
            Self::Nuxt => Some("nuxt"),
            Self::Angular => Some("@angular/core"),
            _ => None,
        }
    }

    pub fn entry(self) -> Entry {
        let (name, description, adapter, parser) = match self {
            Self::Node => ("node", "Node.js and npm", AdapterId::Npm, Parser::JsRelease),
            Self::Nextjs => (
                "nextjs",
                "Next.js application",
                AdapterId::Npm,
                Parser::JsRelease,
            ),
            Self::React => (
                "react",
                "React application with Vite",
                AdapterId::Npm,
                Parser::JsRelease,
            ),
            Self::Vue => (
                "vue",
                "Vue application with Vite",
                AdapterId::Npm,
                Parser::JsRelease,
            ),
            Self::Svelte => (
                "svelte",
                "Svelte application with SvelteKit",
                AdapterId::Npm,
                Parser::JsRelease,
            ),
            Self::Astro => (
                "astro",
                "Astro application",
                AdapterId::Npm,
                Parser::JsRelease,
            ),
            Self::Nuxt => (
                "nuxt",
                "Nuxt application",
                AdapterId::Npm,
                Parser::JsRelease,
            ),
            Self::Angular => (
                "angular",
                "Angular application",
                AdapterId::Npm,
                Parser::JsRelease,
            ),
            Self::Python => (
                "python",
                "Python and a project virtual environment",
                AdapterId::Python,
                Parser::PythonRelease,
            ),
            Self::Rust => (
                "rust",
                "Rust toolchain and Cargo project",
                AdapterId::Cargo,
                Parser::RustToolchain,
            ),
            Self::Go => (
                "go",
                "Go toolchain and module project",
                AdapterId::Go,
                Parser::GoRelease,
            ),
        };
        Entry {
            id: self,
            name,
            description,
            adapter,
            parser,
            platform: "darwin-arm64",
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum DefinitionState {
    Planned,
    Available,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum VerificationState {
    Unverified,
    Verified,
    Failed,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn catalog_contains_exactly_the_approved_profiles_without_alias_drift() {
        let names: std::collections::BTreeSet<_> = ALL.iter().map(|id| id.entry().name).collect();
        assert_eq!(names.len(), 11);
        for id in ALL {
            assert_eq!(
                serde_json::from_str::<ProfileId>(&format!("\"{}\"", id.entry().name)).unwrap(),
                id
            );
            assert_eq!(id.entry().platform, "darwin-arm64");
        }
        for excluded in ["java", "dotnet", "cpp", "custom"] {
            assert!(serde_json::from_str::<ProfileId>(&format!("\"{excluded}\"")).is_err());
        }
    }
}
