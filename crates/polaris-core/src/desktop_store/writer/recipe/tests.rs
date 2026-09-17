//! Records only; nothing is executed and no file is read.
use super::*;

fn current() -> Current {
    Current {
        recipe_id: "nextjs".into(),
        recipe_version: 1,
        input_digest: "input-0001".into(),
        lockfile_digest: Some("lock-after".into()),
        environment: "macos/node-24/npm-11".into(),
    }
}

fn intent(step_id: &str, after: &[&str]) -> SavedRecipeIntent {
    SavedRecipeIntent {
        schema_version: SCHEMA_VERSION,
        recipe_id: "nextjs".into(),
        recipe_version: 1,
        step_id: step_id.into(),
        after: after.iter().map(|held| (*held).to_owned()).collect(),
        input_digest: "input-0001".into(),
        lockfile_digest_before: Some("lock-before".into()),
        source: Some("registry.npmjs.org".into()),
        requested_versions: vec!["next@15".into()],
        environment: "macos/node-24/npm-11".into(),
    }
}

fn finished(step_id: &str, after: &[&str]) -> SavedRecipeStep {
    SavedRecipeStep {
        intent: intent(step_id, after),
        terminal: Some(SavedRecipeTerminal {
            outcome: StepOutcome::Succeeded,
            exit_code: Some(0),
            signal: None,
            lockfile_digest_after: Some("lock-after".into()),
            resolved_versions: vec!["next@15.4.2".into()],
            evidence: Some("checks/nextjs/build.json".into()),
            log: None,
        }),
    }
}

#[test]
fn a_finished_step_is_reused_only_while_its_inputs_still_hold() {
    let step = finished("install", &[]);
    assert!(step.reusable(&current()));

    // Anything the step stood on having moved means it stands for nothing.
    for changed in [
        Current {
            input_digest: "input-0002".into(),
            ..current()
        },
        Current {
            lockfile_digest: Some("lock-edited".into()),
            ..current()
        },
        Current {
            environment: "macos/node-26/npm-11".into(),
            ..current()
        },
        Current {
            recipe_version: 2,
            ..current()
        },
        Current {
            recipe_id: "vite".into(),
            ..current()
        },
    ] {
        assert!(!step.reusable(&changed), "{changed:?}");
    }
}

#[test]
fn only_a_success_that_actually_ended_counts() {
    for outcome in [
        StepOutcome::Failed,
        StepOutcome::Cancelled,
        // The host could not read back what happened. Re-running is the
        // caller's decision, but it is never skipped as done.
        StepOutcome::Unknown,
    ] {
        let mut step = finished("install", &[]);
        step.terminal.as_mut().expect("a terminal").outcome = outcome;
        assert!(!step.reusable(&current()), "{outcome:?}");
    }
    // A step interrupted before it settled has no terminal at all.
    let mut unsettled = finished("install", &[]);
    unsettled.terminal = None;
    assert!(!unsettled.reusable(&current()));
}

#[test]
fn a_step_is_not_reused_on_top_of_one_that_was_not() {
    let install = finished("install", &[]);
    let configure = finished("configure", &["install"]);
    let verify = finished("verify", &["configure"]);
    let saved = vec![install.clone(), configure.clone(), verify.clone()];
    assert_eq!(
        reusable_steps(&saved, &current()),
        vec!["install".to_string(), "configure".into(), "verify".into()]
    );

    // The middle step failed, so what came after it is planned again even
    // though its own record looks finished.
    let mut broken = configure;
    broken.terminal.as_mut().expect("a terminal").outcome = StepOutcome::Failed;
    assert_eq!(
        reusable_steps(&[install, broken, verify], &current()),
        vec!["install".to_string()]
    );
}

#[test]
fn a_step_saved_under_an_older_shape_is_not_read_under_this_one() {
    let mut old = finished("install", &[]);
    old.intent.schema_version = SCHEMA_VERSION - 1;
    assert!(!old.reusable(&current()));
}

#[test]
fn a_project_that_gained_a_lockfile_does_not_reuse_the_step_that_had_none() {
    let mut fresh = finished("install", &[]);
    fresh.intent.lockfile_digest_before = None;
    fresh
        .terminal
        .as_mut()
        .expect("a terminal")
        .lockfile_digest_after = None;
    // The step ran against a project with no lockfile; there is one now.
    assert!(!fresh.reusable(&current()));
    assert!(fresh.reusable(&Current {
        lockfile_digest: None,
        ..current()
    }));
}

/// The ledger on a real writer: one session, one run, steps saved as
/// operations and read back after reopening.
mod saved {
    use super::super::*;
    use crate::desktop_store::{InitialState, PrototypeRoot};
    use polaris_desktop_protocol::{ids::*, request::*, snapshot::*};

    fn session() -> SessionId {
        SessionId::new("setting-session").expect("a session id")
    }
    fn project() -> ProjectId {
        ProjectId::new("project").expect("a project id")
    }
    fn target() -> RunTarget {
        RunTarget {
            run_id: RunId::new("run").expect("a run id"),
            attempt_id: AttemptId::new("attempt").expect("an attempt id"),
        }
    }
    fn op(id: &str) -> OperationId {
        OperationId::new(id).expect("an operation id")
    }

    fn started() -> (PrototypeRoot, crate::desktop_store::Writer) {
        let root = PrototypeRoot::new().expect("a store");
        let mut writer = root
            .create(
                project(),
                session(),
                InitialState {
                    draft: Draft {
                        draft_revision: DecimalU64::new(0),
                        text: "setting".into(),
                        attachment_ids: Vec::new(),
                    },
                    configuration: Configuration {
                        history_mode: Default::default(),
                        configuration_revision: DecimalU64::new(0),
                        provider: "fake".into(),
                        model: "model".into(),
                        effort: "medium".into(),
                    },
                    policy_revision: DecimalU64::new(0),
                },
            )
            .expect("the session is created");
        writer
            .apply_bound(
                &Request {
                    protocol_version: Default::default(),
                    client_id: ClientId::new("cli").expect("a client id"),
                    request_id: RequestId::new("r-1").expect("a request id"),
                    body: RequestBody::RunStart(
                        session(),
                        RunStart {
                            mcp_prompt: None,
                            expected_draft_revision: DecimalU64::new(0),
                            expected_configuration_revision: DecimalU64::new(0),
                            expected_policy_revision: DecimalU64::new(0),
                        },
                    ),
                },
                Some(target()),
                None,
            )
            .expect("the run is accepted");
        (root, writer)
    }

    fn intent(step_id: &str) -> SavedRecipeIntent {
        SavedRecipeIntent {
            schema_version: SCHEMA_VERSION,
            recipe_id: "nextjs".into(),
            recipe_version: 1,
            step_id: step_id.into(),
            after: Vec::new(),
            input_digest: "input-0001".into(),
            lockfile_digest_before: None,
            source: None,
            requested_versions: Vec::new(),
            environment: "macos/node-24/npm-11".into(),
        }
    }

    fn terminal() -> SavedRecipeTerminal {
        SavedRecipeTerminal {
            outcome: StepOutcome::Succeeded,
            exit_code: Some(0),
            signal: None,
            lockfile_digest_after: None,
            resolved_versions: Vec::new(),
            evidence: Some("checks/detect.json".into()),
            log: None,
        }
    }

    #[test]
    fn a_step_is_saved_before_it_runs_and_settles_once() {
        let (root, mut writer) = started();
        assert!(writer.recipe_steps().is_empty());
        assert_eq!(
            writer
                .record_recipe_intent(&target(), op("detect"), intent("detect"))
                .expect("the intent is saved"),
            IntentReceipt::NewlyPublished
        );
        // Saved before the step ran, so it is there with no terminal.
        let saved = writer.recipe_steps();
        assert_eq!(saved.len(), 1);
        assert_eq!(saved[0].terminal, None);
        assert!(!saved[0].reusable(&Current {
            recipe_id: "nextjs".into(),
            recipe_version: 1,
            input_digest: "input-0001".into(),
            lockfile_digest: None,
            environment: "macos/node-24/npm-11".into(),
        }));

        writer
            .record_recipe_terminal(&target(), &op("detect"), terminal())
            .expect("the terminal is saved");
        // A second terminal would let a re-run rewrite how a step ended.
        assert!(
            writer
                .record_recipe_terminal(&target(), &op("detect"), terminal())
                .is_err()
        );

        // Reopening the session reads the same ledger back.
        drop(writer);
        let reopened = root
            .open(&project(), &session())
            .expect("the session opens");
        let saved = reopened.recipe_steps();
        assert_eq!(saved.len(), 1);
        assert_eq!(
            saved[0].terminal.as_ref().map(|t| t.outcome),
            Some(StepOutcome::Succeeded)
        );
        assert!(saved[0].reusable(&Current {
            recipe_id: "nextjs".into(),
            recipe_version: 1,
            input_digest: "input-0001".into(),
            lockfile_digest: None,
            environment: "macos/node-24/npm-11".into(),
        }));
    }

    #[test]
    fn the_same_request_twice_is_the_same_record_and_a_different_one_is_refused() {
        let (_root, mut writer) = started();
        writer
            .record_recipe_intent(&target(), op("detect"), intent("detect"))
            .expect("the intent is saved");
        assert_eq!(
            writer
                .record_recipe_intent(&target(), op("detect"), intent("detect"))
                .expect("the same request again"),
            IntentReceipt::AlreadyRecorded
        );
        let mut other = intent("detect");
        other.input_digest = "input-0002".into();
        assert!(
            writer
                .record_recipe_intent(&target(), op("detect"), other)
                .is_err(),
            "the same operation id must not stand for a different request"
        );
        assert_eq!(writer.recipe_steps().len(), 1);
    }

    #[test]
    fn a_step_saved_under_another_shape_is_not_accepted() {
        let (_root, mut writer) = started();
        let mut old = intent("detect");
        old.schema_version = SCHEMA_VERSION + 1;
        assert!(
            writer
                .record_recipe_intent(&target(), op("detect"), old)
                .is_err()
        );
        assert!(writer.recipe_steps().is_empty());
    }

    #[test]
    fn a_terminal_without_a_saved_intent_has_nowhere_to_go() {
        let (_root, mut writer) = started();
        assert!(
            writer
                .record_recipe_terminal(&target(), &op("detect"), terminal())
                .is_err()
        );
    }
}
