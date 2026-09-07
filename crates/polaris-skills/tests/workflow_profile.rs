//! Mandatory workflow skill resolution, source identity, and token budgets.

use std::path::PathBuf;

use polaris_skills::Skill;
use polaris_skills::workflow_profile::{
    ProfileError, ResolvedSkill, WORKFLOW_PROFILE_TOKEN_LIMIT, recommended_bindings, resolve,
};
use sha2::{Digest, Sha256};

fn user(name: &str, body: &str) -> Skill {
    user_at(name, body, &format!("/users/{name}/SKILL.md"))
}

fn user_at(name: &str, body: &str, path: &str) -> Skill {
    Skill {
        name: name.into(),
        description: format!("{name} description"),
        body: body.into(),
        path: PathBuf::from(path),
    }
}

fn refs(values: &[&str]) -> Vec<String> {
    values.iter().map(|value| (*value).into()).collect()
}

#[test]
fn mandatory_skills_keep_always_then_phase_order_and_dedupe() {
    let mut skills = recommended_bindings();
    skills.push(user("project-rule", "Keep the project rule."));
    let profile = resolve(
        &refs(&[
            "builtin:workflow-core",
            "user:project-rule",
            "builtin:workflow-core",
        ]),
        &refs(&["builtin:review", "user:project-rule"]),
        &skills,
    )
    .expect("profile resolves");

    assert_eq!(
        profile.ids,
        refs(&[
            "builtin:workflow-core",
            "user:project-rule",
            "builtin:review"
        ])
    );
    assert!(profile.body.contains("### builtin:workflow-core"));
    assert!(
        profile.body.find("### builtin:workflow-core") < profile.body.find("### user:project-rule")
    );
    assert!(profile.body.find("### user:project-rule") < profile.body.find("### builtin:review"));
}

#[test]
fn missing_and_ambiguous_skills_stop_resolution() {
    let missing = resolve(&refs(&["user:absent"]), &[], &[]).unwrap_err();
    assert_eq!(
        missing,
        ProfileError::Missing {
            id: "user:absent".into()
        }
    );

    let ambiguous = resolve(
        &refs(&["user:duplicate"]),
        &[],
        &[user("duplicate", "one"), user("duplicate", "two")],
    )
    .unwrap_err();
    assert_eq!(
        ambiguous,
        ProfileError::Ambiguous {
            id: "user:duplicate".into(),
            matches: 2
        }
    );
}

#[test]
fn changed_body_changes_manifest_hash() {
    let first = resolve(&refs(&["user:rule"]), &[], &[user("rule", "first body")]).unwrap();
    let second = resolve(&refs(&["user:rule"]), &[], &[user("rule", "second body")]).unwrap();
    assert_ne!(first.manifest_hash, second.manifest_hash);
    assert_ne!(first.entries[0].body_hash, second.entries[0].body_hash);
    assert_eq!(first.entries[0].source, second.entries[0].source);
}

#[test]
fn changed_source_path_changes_manifest_hash_and_is_reported() {
    let first = resolve(
        &refs(&["user:rule"]),
        &[],
        &[user_at("rule", "same body", "/first/rule/SKILL.md")],
    )
    .unwrap();
    let second = resolve(
        &refs(&["user:rule"]),
        &[],
        &[user_at("rule", "same body", "/second/rule/SKILL.md")],
    )
    .unwrap();
    assert_ne!(first.manifest_hash, second.manifest_hash);
    let body_hash = format!("{:x}", Sha256::digest(b"same body"));
    assert_eq!(
        first.entries,
        vec![ResolvedSkill {
            id: "user:rule".into(),
            source: "/first/rule/SKILL.md".into(),
            body_hash: body_hash.clone(),
        }]
    );
    assert_eq!(
        second.entries,
        vec![ResolvedSkill {
            id: "user:rule".into(),
            source: "/second/rule/SKILL.md".into(),
            body_hash,
        }]
    );
}

#[test]
fn irrelevant_skills_do_not_change_tokens_or_manifest() {
    let required = user("required", "Required body remains stable.");
    let baseline = resolve(
        &refs(&["user:required"]),
        &[],
        std::slice::from_ref(&required),
    )
    .unwrap();
    let mut skills = vec![required];
    for index in 0..30 {
        skills.push(user(
            &format!("irrelevant-{index}"),
            "This skill must not be selected.",
        ));
    }
    let with_irrelevant = resolve(&refs(&["user:required"]), &[], &skills).unwrap();
    assert_eq!(with_irrelevant.tokens, baseline.tokens);
    assert_eq!(with_irrelevant.manifest_hash, baseline.manifest_hash);
}

#[test]
fn oversize_profiles_reject_without_truncation() {
    let oversized = "word ".repeat(2_000);
    let error = resolve(&refs(&["user:large"]), &[], &[user("large", &oversized)]).unwrap_err();
    assert!(
        matches!(error, ProfileError::OverBudget { tokens, limit } if tokens > limit && limit == WORKFLOW_PROFILE_TOKEN_LIMIT)
    );
}

#[test]
fn malformed_empty_and_unknown_namespaces_reject() {
    assert!(matches!(
        resolve(&refs(&["plain-name"]), &[], &[]),
        Err(ProfileError::MalformedReference { .. })
    ));
    assert!(matches!(
        resolve(&refs(&["other:name"]), &[], &[]),
        Err(ProfileError::UnknownNamespace { .. })
    ));
    assert!(matches!(
        resolve(&refs(&["user:empty"]), &[], &[user("empty", " \n")]),
        Err(ProfileError::Empty { .. })
    ));
    let malformed = Skill {
        description: String::new(),
        ..user("valid", "body")
    };
    assert!(matches!(
        resolve(&refs(&["user:valid"]), &[], &[malformed]),
        Err(ProfileError::MalformedSkill { .. })
    ));
}

#[test]
fn shipped_builtins_are_valid_and_each_phase_profile_fits_budget() {
    let skills = recommended_bindings();
    assert_eq!(skills.len(), 7);
    for skill in &skills {
        assert!(!skill.name.is_empty());
        assert!(!skill.description.is_empty());
        assert!(!skill.body.trim().is_empty());
    }
    for phase in [
        "brainstorm",
        "specify",
        "implement",
        "review",
        "verify",
        "deliver",
    ] {
        let profile = resolve(
            &refs(&["builtin:workflow-core"]),
            &refs(&[&format!("builtin:{phase}")]),
            &skills,
        )
        .expect("shipped phase profile resolves");
        assert!(
            profile.tokens <= WORKFLOW_PROFILE_TOKEN_LIMIT,
            "{phase}: {}",
            profile.tokens
        );
    }
}
