//! The agent types that ship inside the binary.
//!
//! `spawn` is advertised on every root request and costs tokens whether or
//! not it can be used. Until 2026-09-19 a type could only come from
//! `<project>/agents` or `~/.polaris/agents`, and outside this repository
//! both are empty — so in every other project the tool was paid for on
//! every request and could only fail. `files_md`'s regeneration, which
//! looks its worker up by the name `files-md-writer`, could never find one
//! either.
//!
//! The workflow skills already solve this shape with `include_str!`
//! (`workflow_profile.rs`), and this follows it. The one thing an agent
//! type has that a skill does not is `polaris-output`: a relative path to
//! a JSON Schema the *parent* reads back after the child answers. A type
//! that lives in the binary has no such file, so the schema is embedded
//! next to its manifest and `output_schema_text` hands it over in place of
//! a read. `ROOT` is what makes those paths recognisable, and it is not a
//! path anything will ever resolve: the containment check in
//! `agent_type::parse` still applies to it, and a real discovered type's
//! path is absolute and so can never collide.
//!
//! The manifests are included from the repository's own `agents/`, which
//! is also what the macOS app copies into its bundle. One copy, two
//! consumers — a second copy under this crate would be free to drift.

use std::path::{Path, PathBuf};

use crate::{AgentType, Skill};

/// The first component of a shipped item's path. Not a real directory.
pub const ROOT: &str = "<polaris-builtin>";

/// The optional skills that ship inside the binary.
///
/// Unlike the seven workflow skills (`workflow_profile.rs`), these are
/// **not** always-on: they sit in the optional catalogue, cost their one
/// description line per request, and are read only when the model asks for
/// them by name. Until 2026-09-19 there was no way to ship a skill on those
/// terms — the only bundled skills were the mandatory profile — so anything
/// optional had to be copied into `~/.polaris/skills` by hand on every
/// machine.
const SKILLS: [(&str, &str); 2] = [
    (
        "document-writing",
        include_str!("../resources/skills/document-writing/SKILL.md"),
    ),
    (
        "smallest-change",
        include_str!("../resources/skills/smallest-change/SKILL.md"),
    ),
];

/// The shipped skills, parsed.
///
/// A manifest that fails to parse is dropped rather than panicking, for the
/// same reason as `agent_types`: this is library code on a startup path.
/// `every_shipped_skill_parses` pins the count so a silent drop cannot
/// reach a release.
pub fn skills() -> Vec<Skill> {
    SKILLS
        .iter()
        .filter_map(|(name, manifest)| {
            let (name, description, body) = crate::frontmatter::parse(manifest, name).ok()?;
            Some(Skill {
                name,
                description,
                body,
                path: PathBuf::from(ROOT).join("skills").join("SKILL.md"),
            })
        })
        .collect()
}

struct Builtin {
    name: &'static str,
    manifest: &'static str,
    /// The manifest's own `polaris-output`, repeated here so the lookup
    /// can be an exact path comparison rather than a prefix match.
    /// `every_shipped_type_parses_and_its_schema_path_matches_its_manifest`
    /// holds the two in agreement.
    schema_path: &'static str,
    schema: &'static str,
}

const BUILTINS: [Builtin; 6] = [
    Builtin {
        name: "file-reader",
        manifest: include_str!("../../../agents/file-reader/SKILL.md"),
        schema_path: "references/result.schema.json",
        schema: include_str!("../../../agents/file-reader/references/result.schema.json"),
    },
    Builtin {
        name: "file-inspector",
        manifest: include_str!("../../../agents/file-inspector/SKILL.md"),
        schema_path: "references/result.schema.json",
        schema: include_str!("../../../agents/file-inspector/references/result.schema.json"),
    },
    Builtin {
        name: "files-md-writer",
        manifest: include_str!("../../../agents/files-md-writer/SKILL.md"),
        schema_path: "references/result.schema.json",
        schema: include_str!("../../../agents/files-md-writer/references/result.schema.json"),
    },
    Builtin {
        name: "implementer",
        manifest: include_str!("../../../agents/implementer/SKILL.md"),
        schema_path: "references/result.schema.json",
        schema: include_str!("../../../agents/implementer/references/result.schema.json"),
    },
    Builtin {
        name: "doc-writer",
        manifest: include_str!("../../../agents/doc-writer/SKILL.md"),
        schema_path: "references/result.schema.json",
        schema: include_str!("../../../agents/doc-writer/references/result.schema.json"),
    },
    Builtin {
        name: "reviewer",
        manifest: include_str!("../../../agents/reviewer/SKILL.md"),
        schema_path: "references/result.schema.json",
        schema: include_str!("../../../agents/reviewer/references/result.schema.json"),
    },
];

/// The shipped types, parsed.
///
/// A manifest that fails to parse is dropped rather than panicking, since
/// this is library code on a startup path. That can only happen if a
/// manifest in `agents/` is edited into an invalid state, which
/// `every_shipped_type_parses_and_its_schema_path_matches_its_manifest`
/// catches at test time — the count is pinned there, so a silent drop
/// cannot reach a release.
pub fn agent_types() -> Vec<AgentType> {
    BUILTINS
        .iter()
        .filter_map(|b| {
            crate::agent_type::parse(b.manifest, b.name, &PathBuf::from(ROOT).join(b.name)).ok()
        })
        .collect()
}

/// The embedded schema for a shipped type's `output_schema`.
///
/// `None` means the path names a real file and the caller reads it as
/// before. The comparison is against the whole path, not a prefix, so
/// nothing a project puts on disk can be mistaken for a shipped type.
pub fn output_schema_text(output_schema: &Path) -> Option<&'static str> {
    BUILTINS
        .iter()
        .find(|b| Path::new(ROOT).join(b.name).join(b.schema_path) == output_schema)
        .map(|b| b.schema)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_shipped_type_parses_and_its_schema_path_matches_its_manifest() {
        let types = agent_types();
        assert_eq!(
            types.len(),
            BUILTINS.len(),
            "a shipped manifest no longer parses; {} of {} survived",
            types.len(),
            BUILTINS.len()
        );
        for (builtin, parsed) in BUILTINS.iter().zip(&types) {
            assert_eq!(parsed.name, builtin.name);
            // `parse` derives `output_schema` from the manifest's own
            // `polaris-output`. If the table's copy drifts from it, the
            // lookup below silently stops matching and every spawn of this
            // type dies on "cannot read output schema".
            assert_eq!(
                parsed.output_schema,
                Path::new(ROOT).join(builtin.name).join(builtin.schema_path),
                "{}'s polaris-output does not match the table",
                builtin.name
            );
            assert_eq!(
                output_schema_text(&parsed.output_schema),
                Some(builtin.schema),
                "{} cannot find its own schema",
                builtin.name
            );
            serde_json::from_str::<serde_json::Value>(builtin.schema)
                .unwrap_or_else(|e| panic!("{}'s schema is not JSON: {e}", builtin.name));
        }
    }

    #[test]
    fn every_shipped_skill_parses_and_costs_a_bounded_catalogue_line() {
        let shipped = skills();
        assert_eq!(
            shipped.len(),
            SKILLS.len(),
            "a shipped skill no longer parses"
        );
        for skill in &shipped {
            assert!(!skill.body.trim().is_empty(), "{} has no body", skill.name);
            // The description rides in the optional catalogue on **every**
            // request, so it is the part with a running cost. The body is
            // read only when the model asks for the skill by name.
            let cost = crate::workflow_profile::count_tokens(&skill.description);
            assert!(
                (20..=80).contains(&cost),
                "{}'s description costs {cost} tokens; too short to route on, or too dear to carry",
                skill.name
            );
        }
        // The names must not collide with the mandatory workflow profile's,
        // which is keyed by the same spellings under `builtin:`.
        for skill in &shipped {
            assert!(
                ![
                    "workflow-core",
                    "brainstorm",
                    "specify",
                    "implement",
                    "review",
                    "verify",
                    "deliver"
                ]
                .contains(&skill.name.as_str()),
                "{} collides with a workflow profile skill",
                skill.name
            );
        }
    }

    #[test]
    fn a_path_on_disk_is_never_mistaken_for_a_shipped_schema() {
        for path in [
            "/w/project/agents/implementer/references/result.schema.json",
            "agents/implementer/references/result.schema.json",
            "<polaris-builtin>/implementer/references/other.json",
            "<polaris-builtin>/not-shipped/references/result.schema.json",
            "<polaris-builtin>",
        ] {
            assert_eq!(output_schema_text(Path::new(path)), None, "{path}");
        }
    }

    /// The whole point is that a project can replace one. `parse` is given
    /// the shipped name, so a type the project wrote under the same name
    /// has to be distinguishable — it carries a real directory as its path.
    #[test]
    fn a_shipped_type_is_marked_as_one_by_its_path() {
        for parsed in agent_types() {
            assert!(
                parsed.path.starts_with(ROOT),
                "{} does not carry the shipped root: {}",
                parsed.name,
                parsed.path.display()
            );
        }
    }
}
