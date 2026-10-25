//! Reads a fixture project. No command is run and nothing is installed.
use super::*;
use crate::recipe::NEXT_JS;

fn project() -> tempfile::TempDir {
    tempfile::tempdir().expect("a temporary project")
}

#[test]
fn every_step_names_something_that_has_to_be_read_back() {
    for step in NEXT_JS.steps {
        assert!(
            postcondition(&NEXT_JS, step.id).is_some(),
            "{} has no postcondition, so exit 0 would be the whole check",
            step.id
        );
    }
    assert_eq!(postcondition(&NEXT_JS, "not-a-step"), None);
}

#[test]
fn an_install_that_left_the_manifest_alone_is_not_complete() {
    let dir = project();
    std::fs::write(dir.path().join("package.json"), r#"{"name":"p"}"#).expect("a manifest");
    let condition = postcondition(&NEXT_JS, "install").expect("the install condition");
    // The process could have exited 0 and changed nothing.
    assert_eq!(
        confirm(dir.path(), &condition, ""),
        Verdict::NotMet {
            missing: "Dependencies missing from package.json: next, react, react-dom".into()
        }
    );

    std::fs::write(
        dir.path().join("package.json"),
        r#"{"dependencies":{"next":"15.4.2","react":"19.0.0"},
            "devDependencies":{"react-dom":"19.0.0"}}"#,
    )
    .expect("a manifest");
    assert!(matches!(
        confirm(dir.path(), &condition, ""),
        Verdict::Met { .. }
    ));
}

#[test]
fn a_name_without_a_version_is_not_a_dependency() {
    let dir = project();
    // A script or a comment naming the package is not the package being there.
    std::fs::write(
        dir.path().join("package.json"),
        r#"{"scripts":{"next":"next build"},
            "dependencies":{"next":null,"react":"19.0.0","react-dom":"19.0.0"}}"#,
    )
    .expect("a manifest");
    let condition = postcondition(&NEXT_JS, "install").expect("the install condition");
    assert_eq!(
        confirm(dir.path(), &condition, ""),
        Verdict::NotMet {
            missing: "Dependencies missing from package.json: next".into()
        }
    );
}

#[test]
fn a_manifest_that_cannot_be_read_is_not_the_same_as_a_failed_step() {
    let dir = project();
    let condition = postcondition(&NEXT_JS, "install").expect("the install condition");
    // Absent entirely.
    assert!(matches!(
        confirm(dir.path(), &condition, ""),
        Verdict::Unknown { .. }
    ));
    // Present but not JSON.
    std::fs::write(dir.path().join("package.json"), "{ broken").expect("a manifest");
    assert!(matches!(
        confirm(dir.path(), &condition, ""),
        Verdict::Unknown { .. }
    ));
}

#[test]
fn the_detect_step_is_judged_by_what_the_manager_answered() {
    let dir = project();
    let condition = postcondition(&NEXT_JS, "detect").expect("the detect condition");
    assert_eq!(
        confirm(dir.path(), &condition, "11.4.0\n"),
        Verdict::Met {
            evidence: "manager=11.4.0".into()
        }
    );
    assert!(matches!(
        confirm(dir.path(), &condition, "  \n"),
        Verdict::NotMet { .. }
    ));
}

#[test]
fn a_configuration_file_and_a_build_directory_are_told_apart() {
    let dir = project();
    let configure = postcondition(&NEXT_JS, "configure").expect("the configure condition");
    let verify = postcondition(&NEXT_JS, "verify").expect("the verify condition");
    assert!(matches!(
        confirm(dir.path(), &configure, ""),
        Verdict::NotMet { .. }
    ));

    std::fs::write(dir.path().join("next.config.mjs"), "export default {}").expect("a config");
    assert!(matches!(
        confirm(dir.path(), &configure, ""),
        Verdict::Met { .. }
    ));
    // A build directory that is a plain file is not a build.
    std::fs::write(dir.path().join(".next"), "").expect("a file");
    assert!(matches!(
        confirm(dir.path(), &verify, ""),
        Verdict::NotMet { .. }
    ));
    std::fs::remove_file(dir.path().join(".next")).expect("removed");
    std::fs::create_dir(dir.path().join(".next")).expect("a directory");
    assert!(matches!(
        confirm(dir.path(), &verify, ""),
        Verdict::Met { .. }
    ));
}

#[test]
#[cfg(unix)]
fn a_symlink_standing_in_for_what_the_step_makes_is_not_accepted() {
    let dir = project();
    let elsewhere = tempfile::tempdir().expect("another directory");
    std::os::unix::fs::symlink(elsewhere.path(), dir.path().join(".next")).expect("a symlink");
    let verify = postcondition(&NEXT_JS, "verify").expect("the verify condition");
    // Following it would judge the step by a directory the step did not make.
    assert!(matches!(
        confirm(dir.path(), &verify, ""),
        Verdict::NotMet { .. }
    ));

    // The same for the configuration file. The host writes a real file; a
    // link pointing somewhere else was put there by something else.
    let target = elsewhere.path().join("elsewhere.mjs");
    std::fs::write(&target, "export default {}").expect("a file");
    std::os::unix::fs::symlink(&target, dir.path().join("next.config.mjs")).expect("a symlink");
    let configure = postcondition(&NEXT_JS, "configure").expect("the configure condition");
    assert!(matches!(
        confirm(dir.path(), &configure, ""),
        Verdict::NotMet { .. }
    ));
}

mod settling {
    use super::*;
    use crate::desktop_store::StepOutcome;

    fn ended(exit_code: Option<i32>) -> Ended {
        Ended {
            exit_code,
            signal: None,
            cancelled: false,
        }
    }

    fn met() -> Verdict {
        Verdict::Met {
            evidence: "package.json: next".into(),
        }
    }

    #[test]
    fn exit_zero_with_the_work_undone_is_a_failure_not_a_success() {
        let held = terminal(
            ended(Some(0)),
            Verdict::NotMet {
                missing: "Dependencies missing from package.json: next".into(),
            },
            None,
        );
        assert_eq!(held.outcome, StepOutcome::Failed);
        assert_eq!(held.exit_code, Some(0), "how it ended is still recorded");
        assert_eq!(
            held.evidence.as_deref(),
            Some("Dependencies missing from package.json: next"),
            "what was read is kept, so a person can see why"
        );
    }

    #[test]
    fn a_step_that_reported_trouble_is_never_read_as_done() {
        // Even where the project happens to look right: a step that said it
        // went wrong has not been shown to be safe to skip next time.
        let held = terminal(ended(Some(1)), met(), None);
        assert_eq!(held.outcome, StepOutcome::Failed);
    }

    #[test]
    fn a_postcondition_that_could_not_be_read_settles_as_unknown() {
        let held = terminal(
            ended(Some(0)),
            Verdict::Unknown {
                reason: "Could not read package.json".into(),
            },
            None,
        );
        // Unknown is never retried on its own; the reason says why.
        assert_eq!(held.outcome, StepOutcome::Unknown);
        assert_eq!(
            held.evidence.as_deref(),
            Some("Could not read package.json")
        );
    }

    #[test]
    fn a_process_with_no_exit_code_is_unknown_rather_than_failed() {
        let held = terminal(
            Ended {
                exit_code: None,
                signal: Some(9),
                cancelled: false,
            },
            met(),
            None,
        );
        assert_eq!(held.outcome, StepOutcome::Unknown);
        assert_eq!(held.signal, Some(9));
    }

    #[test]
    fn a_step_the_person_stopped_is_recorded_as_cancelled() {
        let held = terminal(
            Ended {
                exit_code: Some(0),
                signal: None,
                cancelled: true,
            },
            met(),
            None,
        );
        assert_eq!(held.outcome, StepOutcome::Cancelled);
    }

    #[test]
    fn only_a_confirmed_success_settles_as_succeeded() {
        let held = terminal(ended(Some(0)), met(), Some("lock-after".into()));
        assert_eq!(held.outcome, StepOutcome::Succeeded);
        assert_eq!(held.lockfile_digest_after.as_deref(), Some("lock-after"));
        assert_eq!(held.evidence.as_deref(), Some("package.json: next"));
    }
}
