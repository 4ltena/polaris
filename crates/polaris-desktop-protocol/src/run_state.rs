//! 単一attemptの制御入力と観測終端を分離し、取消後の成功保持と再開禁止を純粋に判定する。

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    Queued,
    Loading,
    Running,
    AwaitingApproval,
    Cancelling,
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
    OutcomeUnknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Control {
    Load,
    Start,
    AwaitApproval,
    ApprovalResolved,
    Cancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Observation {
    Succeeded,
    Failed,
    Cancelled,
    Interrupted,
    OutcomeUnknown,
}

impl Observation {
    pub fn state(self) -> RunState {
        match self {
            Self::Succeeded => RunState::Succeeded,
            Self::Failed => RunState::Failed,
            Self::Cancelled => RunState::Cancelled,
            Self::Interrupted => RunState::Interrupted,
            Self::OutcomeUnknown => RunState::OutcomeUnknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunInput {
    Control(Control),
    Observed(Observation),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transition {
    Changed(RunState),
    Unchanged(RunState),
}

impl Transition {
    pub fn state(self) -> RunState {
        match self {
            Self::Changed(s) | Self::Unchanged(s) => s,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransitionError {
    InvalidTransition,
    TerminalConflict,
    TerminalAttempt,
}

impl RunState {
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Succeeded
                | Self::Failed
                | Self::Cancelled
                | Self::Interrupted
                | Self::OutcomeUnknown
        )
    }
    /// 取消による開始禁止だけを表す。承認消費・能力・保存の照合は呼出側の責務。
    pub fn permits_start(self) -> bool {
        matches!(
            self,
            Self::Queued | Self::Loading | Self::Running | Self::AwaitingApproval
        )
    }
    pub fn transition(self, input: RunInput) -> Result<Transition, TransitionError> {
        if self.is_terminal() {
            return match input {
                RunInput::Observed(observed) if observed.state() == self => {
                    Ok(Transition::Unchanged(self))
                }
                RunInput::Observed(_) => Err(TransitionError::TerminalConflict),
                RunInput::Control(Control::Cancel) => Ok(Transition::Unchanged(self)),
                RunInput::Control(_) => Err(TransitionError::TerminalAttempt),
            };
        }
        let next = match input {
            RunInput::Control(control) => match (self, control) {
                (Self::Queued, Control::Cancel) => Self::Cancelled,
                (_, Control::Cancel) => Self::Cancelling,
                (Self::Queued, Control::Load) => Self::Loading,
                (Self::Queued | Self::Loading | Self::AwaitingApproval, Control::Start) => {
                    Self::Running
                }
                (Self::Loading | Self::Running, Control::AwaitApproval) => Self::AwaitingApproval,
                (Self::AwaitingApproval, Control::ApprovalResolved) => Self::AwaitingApproval,
                _ => return Err(TransitionError::InvalidTransition),
            },
            RunInput::Observed(observed) => {
                if self == Self::Queued
                    && !matches!(observed, Observation::Interrupted | Observation::Cancelled)
                {
                    return Err(TransitionError::InvalidTransition);
                }
                observed.state()
            }
        };
        Ok(if self == next {
            Transition::Unchanged(self)
        } else {
            Transition::Changed(next)
        })
    }
}
