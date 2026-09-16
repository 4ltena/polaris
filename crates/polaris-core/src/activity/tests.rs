//! Fixed durations only; no clock is read.
use super::*;

fn seconds(work: Work, seconds: u64) -> Observed {
    Observed {
        work: Some((work, Duration::from_secs(seconds))),
        ..Default::default()
    }
}

#[test]
fn a_long_silent_model_request_or_tool_is_work_not_a_stop() {
    let deadlines = Deadlines::default();
    for (work, seconds_elapsed) in [
        (Work::ModelRequest, 19 * 60),
        (Work::Tool, 29 * 60),
        (Work::Child, 59 * 60),
        (Work::Preparation, 9 * 60),
    ] {
        assert_eq!(
            assess(&seconds(work, seconds_elapsed), &deadlines),
            Activity::Running { work }
        );
    }
}

#[test]
fn each_kind_of_work_passes_its_own_deadline_and_only_raises_a_suspicion() {
    let deadlines = Deadlines::default();
    // The tool deadline is longer than the model one, so a request that is
    // late is not judged by the tool's limit.
    assert_eq!(
        assess(&seconds(Work::ModelRequest, 21 * 60), &deadlines),
        Activity::Suspect {
            work: Work::ModelRequest
        }
    );
    assert_eq!(
        assess(&seconds(Work::Tool, 21 * 60), &deadlines),
        Activity::Running { work: Work::Tool }
    );
}

#[test]
fn waiting_on_a_person_never_becomes_late_and_silence_alone_is_idle() {
    let deadlines = Deadlines::default();
    let waiting = Observed {
        awaiting_approval: true,
        ..seconds(Work::ModelRequest, 10 * 60 * 60)
    };
    assert_eq!(
        assess(&waiting, &deadlines),
        Activity::Waiting {
            reason: WaitReason::Approval
        }
    );
    assert_eq!(assess(&Observed::default(), &deadlines), Activity::Idle);
    assert_eq!(
        assess(
            &Observed {
                finished: Some(false),
                ..Default::default()
            },
            &deadlines
        ),
        Activity::Finished { ok: false }
    );
}
