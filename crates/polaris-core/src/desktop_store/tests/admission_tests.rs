//! Admission append is one durable, replay-safe publication and isolates a
//! running request from user inputs queued behind it.

use super::*;
use crate::session_manager::{
    AdmissionId as ManagerAdmissionId, AdmissionState, AdmissionV1, DestinationMode, RouteReason,
    SessionId as ManagerSessionId, SubmissionPayloadV1,
};

fn admission_session() -> SessionId {
    SessionId::new("admission-session").unwrap()
}

fn admission_setup() -> (PrototypeRoot, Writer) {
    let root = PrototypeRoot::new().unwrap();
    let writer = root
        .create(project(), admission_session(), initial())
        .unwrap();
    (root, writer)
}

fn admission(id: &str, text: &str) -> AdmissionV1 {
    let payload = SubmissionPayloadV1 {
        text: text.into(),
        attachment_ids: Vec::new(),
        route: DestinationMode::Current,
        expected_session_revision: 0,
        expected_goal_revision: 0,
    };
    AdmissionV1 {
        id: ManagerAdmissionId::new(id).unwrap(),
        revision: 1,
        payload_hash: payload.payload_hash().unwrap(),
        parent_session: ManagerSessionId::new(admission_session().as_str()).unwrap(),
        payload,
        route_reason: Some(RouteReason::ExplicitCurrent),
        state: AdmissionState::DestinationReady {
            session_id: ManagerSessionId::new(admission_session().as_str()).unwrap(),
            reason: RouteReason::ExplicitCurrent,
        },
    }
}

fn admitted_target(name: &str) -> RunTarget {
    RunTarget {
        run_id: RunId::new(format!("{name}-run")).unwrap(),
        attempt_id: AttemptId::new(format!("{name}-attempt")).unwrap(),
    }
}

#[test]
fn admitted_append_is_atomic_idempotent_and_collision_safe() {
    let (_root, mut writer) = admission_setup();
    let saved = admission("admission-one", "first input");
    let target = admitted_target("first");
    let first = writer
        .append_admitted_run(&saved, target.clone(), None)
        .unwrap();
    assert!(first.newly_appended);
    let revision = writer.snapshot().unwrap().marker.session_revision;

    let replay = writer
        .append_admitted_run(&saved, admitted_target("ignored"), None)
        .unwrap();
    assert!(!replay.newly_appended);
    assert_eq!(replay.run.run, first.run.run);
    assert_eq!(writer.snapshot().unwrap().marker.session_revision, revision);

    let mut collision = admission("admission-one", "different input");
    collision.payload_hash = collision.payload.payload_hash().unwrap();
    assert!(matches!(
        writer.append_admitted_run(&collision, admitted_target("collision"), None),
        Err(StoreError::RequestConflict)
    ));
}

#[test]
fn later_queued_input_is_excluded_from_the_earlier_request_prefix() {
    let (_root, mut writer) = admission_setup();
    let first = admitted_target("first");
    writer
        .append_admitted_run(
            &admission("admission-one", "first input"),
            first.clone(),
            None,
        )
        .unwrap();
    writer
        .append_admitted_run(
            &admission("admission-two", "future queued input"),
            admitted_target("second"),
            None,
        )
        .unwrap();

    assert_eq!(
        writer
            .raw_request_prefix(&first)
            .unwrap()
            .iter()
            .map(|message| message.content.as_str())
            .collect::<Vec<_>>(),
        vec!["first input"]
    );
    let projected = writer.project_request_history(&first).unwrap();
    assert_eq!(projected.query(), "first input");
    assert_eq!(projected.messages().len(), 1);
    assert_eq!(projected.messages()[0].content, "first input");
}

#[test]
fn unopened_admitted_runs_remain_queued_after_reopen() {
    let (root, mut writer) = admission_setup();
    for (id, text) in [("admission-one", "one"), ("admission-two", "two")] {
        writer
            .append_admitted_run(&admission(id, text), admitted_target(id), None)
            .unwrap();
    }
    drop(writer);

    let reopened = root.open(&project(), &admission_session()).unwrap();
    assert!(
        reopened
            .snapshot()
            .unwrap()
            .state
            .runs
            .iter()
            .all(|record| record.run.state == RunState::Queued)
    );
}
