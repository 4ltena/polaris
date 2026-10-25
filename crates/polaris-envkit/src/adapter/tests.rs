//! Verify fixed plans, profile bindings and compatibility-only fallback.
use super::*;
use crate::{bundled, recipes::Argument};

fn snapshot() -> Snapshot {
    Snapshot {
        runner_binary_sha256: "a".repeat(64),
        host_os_revision: "d".repeat(64),
        target: TargetIdentity {
            canonical_path: "/tmp/envkit-project".into(),
            device: 1,
            inode: 2,
        },
        private_root: TargetIdentity {
            canonical_path: "/tmp/envkit-private".into(),
            device: 1,
            inode: 3,
        },
        platform: "darwin-arm64".into(),
        existing_project: false,
        inputs: BTreeMap::new(),
        installations: BTreeMap::new(),
        host_prerequisites: BTreeMap::new(),
    }
}

#[test]
fn one_owner_plan_binds_commands_environment_paths_and_binary_identity() {
    let definition = bundled::load(ProfileId::Nextjs).unwrap();
    let constraint = definition.profile.entry().parser.parse(None).unwrap();
    let plan = resolve(&definition, &constraint, &snapshot()).unwrap();
    let hash = plan.digest(&definition).unwrap();
    let command = plan.candidates[0].commands.values().next().unwrap();
    let layout = crate::invocation::Layout::for_plan(&plan, 0).unwrap();
    let invocation = crate::invocation::render(
        command,
        &definition.candidates[0],
        &layout,
        &snapshot(),
        &std::collections::BTreeSet::new(),
    )
    .unwrap();
    assert_eq!(
        invocation.environment["NEXT_PRIVATE_OUTPUT_TRACE_ROOT"],
        layout.stage().to_str().unwrap()
    );
    let mut changed = plan.clone();
    changed.candidates[0]
        .commands
        .values_mut()
        .next()
        .unwrap()
        .environment
        .insert(
            "NEXT_PRIVATE_OUTPUT_TRACE_ROOT".into(),
            Argument::Literal("/outside-stage".into()),
        );
    assert_eq!(
        revalidate(&definition, &changed, &snapshot()),
        Err(Error::StalePlan)
    );
    changed = plan.clone();
    changed.candidates[0]
        .commands
        .values_mut()
        .next()
        .unwrap()
        .arguments
        .push(Argument::Literal("--online".into()));
    assert_eq!(
        revalidate(&definition, &changed, &snapshot()),
        Err(Error::StalePlan)
    );
    changed = plan.clone();
    changed.candidates[0]
        .commands
        .values_mut()
        .next()
        .unwrap()
        .environment
        .insert(
            "HTTP_PROXY".into(),
            Argument::Literal("http://example.test".into()),
        );
    assert_eq!(
        revalidate(&definition, &changed, &snapshot()),
        Err(Error::StalePlan)
    );
    changed = plan.clone();
    changed.candidates[0]
        .paths
        .publish_target
        .values_mut()
        .next()
        .unwrap()
        .relative = "../escape".into();
    assert_eq!(
        revalidate(&definition, &changed, &snapshot()),
        Err(Error::StalePlan)
    );
    changed = plan;
    changed.snapshot.runner_binary_sha256 = "b".repeat(64);
    assert_ne!(changed.digest(&definition).unwrap(), hash);
}

#[test]
fn live_target_and_input_changes_invalidate_an_approved_plan() {
    let definition = bundled::load(ProfileId::Node).unwrap();
    let constraint = definition.profile.entry().parser.parse(None).unwrap();
    let captured = snapshot();
    let plan = resolve(&definition, &constraint, &captured).unwrap();
    let mut fresh = captured.clone();
    fresh.target.inode += 1;
    assert_eq!(
        revalidate(&definition, &plan, &fresh),
        Err(Error::StalePlan)
    );
    fresh = captured.clone();
    fresh.private_root.device += 1;
    assert_eq!(
        revalidate(&definition, &plan, &fresh),
        Err(Error::StalePlan)
    );
    fresh = captured.clone();
    fresh.runner_binary_sha256 = "b".repeat(64);
    assert_eq!(
        revalidate(&definition, &plan, &fresh),
        Err(Error::StalePlan)
    );
    fresh = captured;
    fresh.target.device = 0;
    assert_eq!(
        resolve(&definition, &constraint, &fresh),
        Err(Error::StalePlan)
    );
}

#[test]
fn native_linker_and_sdk_must_be_captured_and_remain_unchanged() {
    let definition = bundled::load(ProfileId::Rust).unwrap();
    let constraint = definition.profile.entry().parser.parse(None).unwrap();
    assert_eq!(
        resolve(&definition, &constraint, &snapshot()),
        Err(Error::Unsupported)
    );
    let mut captured = snapshot();
    for (index, id) in definition.required_host.iter().enumerate() {
        captured.host_prerequisites.insert(
            *id,
            HostPrerequisite {
                identity: TargetIdentity {
                    canonical_path: format!("/tmp/native-tools/item-{index}"),
                    device: 1,
                    inode: index as u64 + 10,
                },
                sha256: "c".repeat(64),
                version: "synthetic-toolchain".into(),
            },
        );
    }
    let plan = resolve(&definition, &constraint, &captured).unwrap();
    for command in plan.candidates[0].commands.values() {
        assert_eq!(command.host_inputs, captured.host_prerequisites);
        assert_eq!(
            command.environment.get("SDKROOT"),
            Some(&Argument::HostPath(HostPrerequisiteId::MacSdk))
        );
    }
    let build = plan.candidates[0]
        .operations
        .iter()
        .position(|operation| {
            matches!(
                operation,
                Operation::InstallOffline {
                    recipe: RecipeId::CargoBuild,
                    ..
                }
            )
        })
        .unwrap();
    let flags = plan.candidates[0].commands[&build]
        .environment
        .get("CARGO_ENCODED_RUSTFLAGS")
        .unwrap();
    assert_eq!(
        flags,
        &Argument::EncodedRustflags(vec![
            Argument::Literal("-C".into()),
            Argument::Prefixed {
                prefix: "link-arg=".into(),
                value: Box::new(Argument::Prefixed {
                    prefix: "-fuse-ld=".into(),
                    value: Box::new(Argument::HostPath(HostPrerequisiteId::Linker))
                })
            },
            Argument::Literal("-C".into()),
            Argument::Prefixed {
                prefix: "link-arg=".into(),
                value: Box::new(Argument::Prefixed {
                    prefix: "-resource-dir=".into(),
                    value: Box::new(Argument::HostPath(HostPrerequisiteId::ClangResources))
                })
            },
        ])
    );
    assert_eq!(
        plan.candidates[0]
            .commands
            .values()
            .filter(|command| matches!(
                command.executable,
                crate::recipes::Executable::Host {
                    id: HostPrerequisiteId::CompilerDriver,
                    ..
                }
            ))
            .count(),
        2
    );
    let mut fresh = captured;
    fresh
        .host_prerequisites
        .get_mut(&HostPrerequisiteId::MacSdk)
        .unwrap()
        .sha256 = "d".repeat(64);
    assert_eq!(
        revalidate(&definition, &plan, &fresh),
        Err(Error::StalePlan)
    );
}

#[test]
fn disk_budget_is_profile_specific_and_bounded_by_the_compiled_ceiling() {
    let mut definition = bundled::load(ProfileId::Node).unwrap();
    definition.limits.disk_bytes = 2 * 1024 * 1024 * 1024;
    assert!(definition.validate().is_ok());
    definition.limits.disk_bytes = Limits::default().disk_bytes + 1;
    assert_eq!(definition.validate(), Err(Error::InvalidProfile));
}

#[test]
fn profile_specific_version_parser_cannot_be_swapped() {
    let definition = bundled::load(ProfileId::Python).unwrap();
    let wrong = crate::version::Parser::JsRelease.parse(None).unwrap();
    assert_eq!(
        resolve(&definition, &wrong, &snapshot()),
        Err(Error::Unsupported)
    );
    let actual = definition
        .profile
        .entry()
        .parser
        .parse(Some("3.12"))
        .unwrap();
    let plan = resolve(&definition, &actual, &snapshot()).unwrap();
    assert_eq!(plan.candidates.len(), 1);
    assert_eq!(
        definition.candidates[plan.candidates[0].index].version,
        "3.12.15"
    );
    assert!(
        plan.candidates[0]
            .commands
            .values()
            .all(|command| command.environment.get("PIP_NO_INDEX")
                == Some(&Argument::Literal("1".into())))
    );
}

#[test]
fn catalog_can_hold_more_versions_than_a_single_execution_attempts() {
    let mut definition = bundled::load(ProfileId::Node).unwrap();
    let mut candidates = Vec::new();
    for patch in 0..6 {
        let mut tuple = definition.candidates[0].clone();
        tuple.version = format!("24.22.{patch}");
        tuple.programs.get_mut(&ProgramId::Node).unwrap().version = tuple.version.clone();
        for artifact in tuple.artifacts.values_mut() {
            artifact.version = tuple.version.clone();
        }
        candidates.push(tuple);
    }
    definition.candidates = candidates;
    let constraint = definition.profile.entry().parser.parse(Some("24")).unwrap();
    let plan = resolve(&definition, &constraint, &snapshot()).unwrap();
    assert_eq!(plan.candidates.len(), 3);
}

#[test]
fn existing_project_preimages_are_preserved_and_secret_config_is_not_admitted() {
    let definition = bundled::load(ProfileId::Nextjs).unwrap();
    let mut before = snapshot();
    before.existing_project = true;
    for template in definition.candidates[0].templates.values() {
        if let Some(input) = &template.preserved_input {
            before.inputs.insert(
                input.clone(),
                InputImage {
                    relative: template.relative.clone(),
                    sha256: format!("{:x}", Sha256::digest(template.content.as_bytes())),
                },
            );
        }
    }
    let constraint = definition.profile.entry().parser.parse(None).unwrap();
    let plan = resolve(&definition, &constraint, &before).unwrap();
    assert!(plan.candidates[0].paths.create_only.is_empty());
    assert_eq!(plan.candidates[0].paths.publish_target.len(), 1);
    before.inputs.insert(
        ReadInputId("npm_config".into()),
        InputImage {
            relative: ".npmrc".into(),
            sha256: "a".repeat(64),
        },
    );
    assert_eq!(
        resolve(&definition, &constraint, &before),
        Err(Error::StalePlan)
    );
}
