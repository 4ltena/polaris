//! Fixed observations only; nothing is installed.
use super::*;

fn observed() -> Observed {
    Observed {
        has_manifest: true,
        lockfiles: vec![],
        completed: vec![],
        install_confirmed: true,
    }
}

#[test]
fn a_project_without_its_manifest_or_with_two_lockfiles_is_left_alone() {
    assert_eq!(
        plan(
            &NEXT_JS,
            &Observed {
                has_manifest: false,
                ..observed()
            }
        ),
        Plan::NotApplicable {
            reason: Refusal::NoManifest
        }
    );
    assert_eq!(
        plan(
            &NEXT_JS,
            &Observed {
                lockfiles: vec![Manager::Npm, Manager::Pnpm],
                ..observed()
            }
        ),
        Plan::NotApplicable {
            reason: Refusal::MixedManagers
        }
    );
}

#[test]
fn the_projects_own_lockfile_decides_the_manager_and_finished_steps_are_not_redone() {
    assert_eq!(
        plan(
            &NEXT_JS,
            &Observed {
                lockfiles: vec![Manager::Pnpm],
                ..observed()
            }
        ),
        Plan::Run {
            manager: Manager::Pnpm,
            steps: vec!["detect", "install", "configure", "verify"],
        }
    );
    assert_eq!(
        plan(
            &NEXT_JS,
            &Observed {
                completed: vec!["detect".into(), "install".into()],
                ..observed()
            }
        ),
        Plan::Run {
            manager: Manager::Npm,
            steps: vec!["configure", "verify"],
        }
    );
    assert_eq!(
        plan(
            &NEXT_JS,
            &Observed {
                completed: NEXT_JS
                    .steps
                    .iter()
                    .map(|step| step.id.to_string())
                    .collect(),
                ..observed()
            }
        ),
        Plan::NotApplicable {
            reason: Refusal::AlreadyComplete
        }
    );
}

#[test]
fn without_a_confirmation_the_plan_stops_before_anything_is_installed() {
    let unconfirmed = Observed {
        install_confirmed: false,
        ..observed()
    };
    // Reading the project is allowed; installing is not.
    assert_eq!(
        plan(&NEXT_JS, &unconfirmed),
        Plan::Run {
            manager: Manager::Npm,
            steps: vec!["detect"],
        }
    );
    assert_eq!(
        plan(
            &NEXT_JS,
            &Observed {
                completed: vec!["detect".into()],
                ..unconfirmed
            }
        ),
        Plan::NotApplicable {
            reason: Refusal::InstallNotConfirmed
        }
    );
    assert_eq!(Manager::Pnpm.lockfile(), "pnpm-lock.yaml");
}

#[test]
fn the_project_is_read_for_its_manifest_and_lockfiles_only() {
    let temporary = tempfile::tempdir().unwrap();
    let root = temporary.path();
    let observed = observe(root, &NEXT_JS, vec![], false);
    assert!(!observed.has_manifest && observed.lockfiles.is_empty());
    assert_eq!(
        plan(&NEXT_JS, &observed),
        Plan::NotApplicable {
            reason: Refusal::NoManifest
        }
    );

    std::fs::write(root.join("package.json"), "{}").unwrap();
    std::fs::write(root.join("pnpm-lock.yaml"), "").unwrap();
    let observed = observe(root, &NEXT_JS, vec!["detect".into()], true);
    assert!(observed.has_manifest);
    assert_eq!(observed.lockfiles, vec![Manager::Pnpm]);
    assert_eq!(
        plan(&NEXT_JS, &observed),
        Plan::Run {
            manager: Manager::Pnpm,
            steps: vec!["install", "configure", "verify"],
        }
    );
}

#[test]
fn each_step_names_a_fixed_command_and_configuration_is_written_by_the_host() {
    assert_eq!(
        command(&NEXT_JS, "install", Manager::Pnpm),
        Some((
            "pnpm",
            vec![
                "add".to_string(),
                "next".into(),
                "react".into(),
                "react-dom".into()
            ]
        ))
    );
    assert_eq!(
        command(&NEXT_JS, "verify", Manager::Npm),
        Some(("npm", vec!["run".to_string(), "build".into()]))
    );
    // The host writes configuration; no package command does it.
    assert_eq!(command(&NEXT_JS, "configure", Manager::Npm), None);
    assert_eq!(command(&NEXT_JS, "unknown", Manager::Npm), None);
}
