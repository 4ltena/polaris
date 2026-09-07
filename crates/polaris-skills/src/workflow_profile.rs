//! Deterministic resolution of the optional shipped workflow skill profile.
//!
//! This module deliberately knows nothing about workflow state, configuration,
//! or prompt assembly.  Its caller supplies the mandatory IDs for one turn and
//! the skills visible to that turn; the result is a frozen, budgeted snapshot.

use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::Skill;

/// The maximum number of `o200k_base` tokens injected by this resolver.
pub const WORKFLOW_PROFILE_TOKEN_LIMIT: usize = 384;

const BUILTIN_NAMESPACE: &str = "builtin:";
const USER_NAMESPACE: &str = "user:";
const WRAPPER_HEADING: &str = "## Required workflow instructions\n";

const BUILTINS: [(&str, &str); 7] = [
    (
        "workflow-core",
        include_str!("../resources/workflow/workflow-core/SKILL.md"),
    ),
    (
        "brainstorm",
        include_str!("../resources/workflow/brainstorm/SKILL.md"),
    ),
    (
        "specify",
        include_str!("../resources/workflow/specify/SKILL.md"),
    ),
    (
        "implement",
        include_str!("../resources/workflow/implement/SKILL.md"),
    ),
    (
        "review",
        include_str!("../resources/workflow/review/SKILL.md"),
    ),
    (
        "verify",
        include_str!("../resources/workflow/verify/SKILL.md"),
    ),
    (
        "deliver",
        include_str!("../resources/workflow/deliver/SKILL.md"),
    ),
];

/// A fully resolved mandatory profile for one frozen workflow turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedProfile {
    /// Complete wrapped instructions, without truncation.
    pub body: String,
    /// SHA-256 of complete resolved identity, source identity, and body.
    pub manifest_hash: String,
    /// Namespaced IDs, in the configured mandatory order after de-duplication.
    pub ids: Vec<String>,
    /// Resolved identities and diagnostics in the same order as [`Self::ids`].
    pub entries: Vec<ResolvedSkill>,
    /// `o200k_base` token count for [`Self::body`].
    pub tokens: usize,
}

/// One selected skill's stable identity and change diagnostics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedSkill {
    pub id: String,
    pub source: String,
    pub body_hash: String,
}

/// A deterministic resolver failure that must stop the turn before a model request.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ProfileError {
    #[error("workflow skill reference is malformed: {reference}")]
    MalformedReference { reference: String },
    #[error("workflow skill namespace is unknown: {reference}")]
    UnknownNamespace { reference: String },
    #[error("workflow skill is missing: {id}")]
    Missing { id: String },
    #[error("workflow skill is ambiguous: {id} matched {matches} skills")]
    Ambiguous { id: String, matches: usize },
    #[error("workflow skill has an empty body: {id}")]
    Empty { id: String },
    #[error("workflow skill is malformed: {id}")]
    MalformedSkill { id: String },
    #[error("workflow profile is {tokens} tokens, over the {limit}-token limit")]
    OverBudget { tokens: usize, limit: usize },
}

/// Returns the seven built-in workflow skills for an explicit profile binding.
///
/// This only constructs in-memory values. It neither discovers nor installs
/// skills, so callers can opt into it without changing global skill defaults.
pub fn recommended_bindings() -> Vec<Skill> {
    BUILTINS
        .iter()
        .map(|(name, source)| {
            let (parsed_name, description, body) = crate::frontmatter::parse(source, name)
                .expect("shipped workflow skill must have valid frontmatter");
            Skill {
                name: parsed_name,
                description,
                body,
                path: PathBuf::from(format!("{BUILTIN_NAMESPACE}{name}")),
            }
        })
        .collect()
}

/// Resolves explicit mandatory workflow skills in `always` order followed by
/// `phase_skills` order. This never invokes search or BM25 ranking.
pub fn resolve(
    always: &[String],
    phase_skills: &[String],
    skills: &[Skill],
) -> Result<ResolvedProfile, ProfileError> {
    let mut selected = Vec::new();
    for reference in always.iter().chain(phase_skills) {
        let (namespace, name) = parse_reference(reference)?;
        let matches: Vec<&Skill> = skills
            .iter()
            .filter(|skill| matches_reference(namespace, name, skill))
            .collect();
        let id = format!("{namespace}{name}");
        let skill = match matches.len() {
            0 => return Err(ProfileError::Missing { id }),
            1 => matches[0],
            matches => return Err(ProfileError::Ambiguous { id, matches }),
        };
        validate_skill(&id, skill)?;
        let content_hash = hash(&skill.body);
        if selected.iter().any(
            |(seen_id, seen_hash, _, _): &(String, String, String, &Skill)| {
                seen_id == &id && seen_hash == &content_hash
            },
        ) {
            continue;
        }
        selected.push((id, content_hash, source_path(skill), skill));
    }

    let mut body = String::from(WRAPPER_HEADING);
    for (id, _, _, skill) in &selected {
        body.push_str("\n### ");
        body.push_str(id);
        body.push_str("\n\n");
        body.push_str(skill.body.trim());
        body.push('\n');
    }
    let tokens = count_tokens(&body);
    if tokens > WORKFLOW_PROFILE_TOKEN_LIMIT {
        return Err(ProfileError::OverBudget {
            tokens,
            limit: WORKFLOW_PROFILE_TOKEN_LIMIT,
        });
    }

    let ids = selected.iter().map(|(id, _, _, _)| id.clone()).collect();
    let entries = selected
        .iter()
        .map(|(id, body_hash, source, _)| ResolvedSkill {
            id: id.clone(),
            source: source.clone(),
            body_hash: body_hash.clone(),
        })
        .collect();
    let manifest_hash = manifest_hash(&selected, &body);
    Ok(ResolvedProfile {
        body,
        manifest_hash,
        ids,
        entries,
        tokens,
    })
}

fn parse_reference(reference: &str) -> Result<(&str, &str), ProfileError> {
    let (namespace, name) = if let Some(name) = reference.strip_prefix(BUILTIN_NAMESPACE) {
        (BUILTIN_NAMESPACE, name)
    } else if let Some(name) = reference.strip_prefix(USER_NAMESPACE) {
        (USER_NAMESPACE, name)
    } else if reference.contains(':') {
        return Err(ProfileError::UnknownNamespace {
            reference: reference.into(),
        });
    } else {
        return Err(ProfileError::MalformedReference {
            reference: reference.into(),
        });
    };
    if !valid_name(name) {
        return Err(ProfileError::MalformedReference {
            reference: reference.into(),
        });
    }
    Ok((namespace, name))
}

fn matches_reference(namespace: &str, name: &str, skill: &Skill) -> bool {
    match namespace {
        BUILTIN_NAMESPACE => {
            skill.name == name
                && skill
                    .path
                    .to_str()
                    .and_then(|path| path.strip_prefix(BUILTIN_NAMESPACE))
                    == Some(name)
        }
        USER_NAMESPACE => skill.name == name && !is_builtin(skill),
        _ => false,
    }
}

fn is_builtin(skill: &Skill) -> bool {
    skill.path.to_string_lossy().starts_with(BUILTIN_NAMESPACE)
}

fn validate_skill(id: &str, skill: &Skill) -> Result<(), ProfileError> {
    if !valid_name(&skill.name) || skill.description.trim().is_empty() {
        return Err(ProfileError::MalformedSkill { id: id.into() });
    }
    if skill.body.trim().is_empty() {
        return Err(ProfileError::Empty { id: id.into() });
    }
    Ok(())
}

fn valid_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && !name.starts_with('-')
        && !name.ends_with('-')
        && !name.contains("--")
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

fn count_tokens(text: &str) -> usize {
    tiktoken_rs::o200k_base_singleton()
        .encode_with_special_tokens(text)
        .len()
}

fn hash(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

fn source_path(skill: &Skill) -> String {
    skill.path.to_string_lossy().into_owned()
}

fn manifest_hash(selected: &[(String, String, String, &Skill)], body: &str) -> String {
    let mut digest = Sha256::new();
    for (id, content_hash, source, _) in selected {
        digest.update(id.as_bytes());
        digest.update([0]);
        digest.update(content_hash.as_bytes());
        digest.update([0]);
        digest.update(hash(source).as_bytes());
        digest.update([0]);
    }
    digest.update(body.as_bytes());
    format!("{:x}", digest.finalize())
}
