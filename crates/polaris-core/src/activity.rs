//! M6-2: what the host knows about a run's liveness, decided from events and
//! per-kind deadlines rather than from silence. Pure: no clock is read here,
//! no process is inspected, and nothing is restarted. A long model request or
//! a long tool is ordinary work, so only a deadline for that specific kind of
//! work can raise a suspicion, and a suspicion is never a stop.
use serde::Serialize;
use std::time::Duration;

/// Deadlines are per kind of work because their normal durations differ by
/// orders of magnitude. An approval waits on a person, so it has none here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadlines {
    pub model_request: Duration,
    pub tool: Duration,
    pub child: Duration,
    pub preparation: Duration,
}

impl Default for Deadlines {
    fn default() -> Self {
        Self {
            model_request: Duration::from_secs(20 * 60),
            tool: Duration::from_secs(30 * 60),
            child: Duration::from_secs(60 * 60),
            preparation: Duration::from_secs(10 * 60),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Work {
    ModelRequest,
    Tool,
    Child,
    Preparation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WaitReason {
    /// A person must answer; no deadline of ours applies.
    Approval,
    Input,
    Dependency,
}

/// The run's liveness as the host sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Activity {
    Running {
        work: Work,
    },
    Waiting {
        reason: WaitReason,
    },
    /// Nothing is running and nothing is waited on. Not a failure and not a
    /// reason to restart: the run may simply be between steps.
    Idle,
    /// One kind of work passed its own deadline. The host asks the run for
    /// its state next; it does not conclude that the run is gone.
    Suspect {
        work: Work,
    },
    Finished {
        ok: bool,
    },
}

/// What the owner observed, with elapsed times it measured itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Observed {
    pub finished: Option<bool>,
    pub awaiting_approval: bool,
    pub awaiting_input: bool,
    pub awaiting_dependency: bool,
    /// The work in flight and how long it has been in flight.
    pub work: Option<(Work, Duration)>,
}

pub fn assess(observed: &Observed, deadlines: &Deadlines) -> Activity {
    if let Some(ok) = observed.finished {
        return Activity::Finished { ok };
    }
    // A person or another run is the reason for the pause, so no deadline of
    // ours applies and the work is not late.
    if observed.awaiting_approval {
        return Activity::Waiting {
            reason: WaitReason::Approval,
        };
    }
    if observed.awaiting_input {
        return Activity::Waiting {
            reason: WaitReason::Input,
        };
    }
    if observed.awaiting_dependency {
        return Activity::Waiting {
            reason: WaitReason::Dependency,
        };
    }
    let Some((work, elapsed)) = observed.work else {
        // Silence with no work in flight is idle, never a stop.
        return Activity::Idle;
    };
    let deadline = match work {
        Work::ModelRequest => deadlines.model_request,
        Work::Tool => deadlines.tool,
        Work::Child => deadlines.child,
        Work::Preparation => deadlines.preparation,
    };
    if elapsed > deadline {
        Activity::Suspect { work }
    } else {
        Activity::Running { work }
    }
}

#[cfg(test)]
mod tests;
