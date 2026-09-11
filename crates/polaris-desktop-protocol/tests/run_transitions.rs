//! 制御×状態の全表と取消・成功の両順序を検査し、同attemptの終端を保持する。

use polaris_desktop_protocol::run_state::{
    Control, Observation, RunInput, RunState, Transition, TransitionError,
};

const STATES: [RunState; 10] = [
    RunState::Queued,
    RunState::Loading,
    RunState::Running,
    RunState::AwaitingApproval,
    RunState::Cancelling,
    RunState::Succeeded,
    RunState::Failed,
    RunState::Cancelled,
    RunState::Interrupted,
    RunState::OutcomeUnknown,
];

#[test]
fn complete_control_transition_table() {
    use RunState::*;
    let controls = [
        Control::Load,
        Control::Start,
        Control::AwaitApproval,
        Control::ApprovalResolved,
        Control::Cancel,
    ];
    // 行はSTATES、列はcontrols。Noneは明示的な拒否。
    let expected = [
        [Some(Loading), Some(Running), None, None, Some(Cancelled)],
        [
            None,
            Some(Running),
            Some(AwaitingApproval),
            None,
            Some(Cancelling),
        ],
        [None, None, Some(AwaitingApproval), None, Some(Cancelling)],
        [
            None,
            Some(Running),
            None,
            Some(AwaitingApproval),
            Some(Cancelling),
        ],
        [None, None, None, None, Some(Cancelling)],
        [None, None, None, None, Some(Succeeded)],
        [None, None, None, None, Some(Failed)],
        [None, None, None, None, Some(Cancelled)],
        [None, None, None, None, Some(Interrupted)],
        [None, None, None, None, Some(OutcomeUnknown)],
    ];
    for (row, state) in STATES.iter().copied().enumerate() {
        for (column, control) in controls.iter().copied().enumerate() {
            let actual = state.transition(RunInput::Control(control));
            match expected[row][column] {
                Some(next) if next == state => assert_eq!(actual, Ok(Transition::Unchanged(state))),
                Some(next) => assert_eq!(actual, Ok(Transition::Changed(next))),
                None => assert_eq!(
                    actual,
                    Err(if row >= 5 {
                        TransitionError::TerminalAttempt
                    } else {
                        TransitionError::InvalidTransition
                    })
                ),
            }
        }
        assert_eq!(state.permits_start(), row < 4);
        assert_eq!(state.is_terminal(), row >= 5);
    }
}

#[test]
fn every_observation_preserves_terminal_identity_or_reports_conflict() {
    let observations = [
        Observation::Succeeded,
        Observation::Failed,
        Observation::Cancelled,
        Observation::Interrupted,
        Observation::OutcomeUnknown,
    ];
    let terminals = &STATES[5..];
    for (row, state) in STATES.iter().copied().enumerate() {
        for (column, observed) in observations.iter().copied().enumerate() {
            let actual = state.transition(RunInput::Observed(observed));
            let terminal = terminals[column];
            let expected = if row >= 5 {
                if state == terminal {
                    Ok(Transition::Unchanged(state))
                } else {
                    Err(TransitionError::TerminalConflict)
                }
            } else if row == 0 && ![2, 3].contains(&column) {
                Err(TransitionError::InvalidTransition)
            } else {
                Ok(Transition::Changed(terminal))
            };
            assert_eq!(actual, expected, "{state:?} + {observed:?}");
        }
    }
}

#[test]
fn cancel_and_confirmed_success_in_both_orders_converge_to_success() {
    let cancelling = RunState::Running
        .transition(RunInput::Control(Control::Cancel))
        .unwrap()
        .state();
    assert_eq!(cancelling, RunState::Cancelling);
    assert!(!cancelling.permits_start());
    assert_eq!(
        cancelling.transition(RunInput::Observed(Observation::Succeeded)),
        Ok(Transition::Changed(RunState::Succeeded))
    );
    let succeeded = RunState::Running
        .transition(RunInput::Observed(Observation::Succeeded))
        .unwrap()
        .state();
    assert_eq!(
        succeeded.transition(RunInput::Control(Control::Cancel)),
        Ok(Transition::Unchanged(RunState::Succeeded))
    );
}

#[test]
fn queued_cancel_prevents_start_and_approval_resolution_is_not_start() {
    let cancelled = RunState::Queued
        .transition(RunInput::Control(Control::Cancel))
        .unwrap()
        .state();
    assert_eq!(cancelled, RunState::Cancelled);
    assert_eq!(
        cancelled.transition(RunInput::Control(Control::Start)),
        Err(TransitionError::TerminalAttempt)
    );
    assert_eq!(
        RunState::AwaitingApproval.transition(RunInput::Control(Control::ApprovalResolved)),
        Ok(Transition::Unchanged(RunState::AwaitingApproval))
    );
    let cancelling = RunState::AwaitingApproval
        .transition(RunInput::Control(Control::Cancel))
        .unwrap()
        .state();
    assert_eq!(
        cancelling.transition(RunInput::Control(Control::ApprovalResolved)),
        Err(TransitionError::InvalidTransition)
    );
    assert_eq!(
        cancelling.transition(RunInput::Control(Control::Start)),
        Err(TransitionError::InvalidTransition)
    );
}
