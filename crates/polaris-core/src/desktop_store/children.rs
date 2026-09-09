//! Bounded child ownership ledger. Child completion never accepts a task.
use super::{Sidecar, StoreError, StoreResult};
use polaris_desktop_protocol::{
    request::RunTarget,
    run_state::{Observation, RunState},
    snapshot::Child,
};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedChild {
    pub root: RunTarget,
    pub child: Child,
    pub agent_type: String,
    pub task: String,
}

pub(super) fn validate(state: &Sidecar) -> StoreResult<()> {
    let invalid = || StoreError::Corrupt("child ownership ledger");
    if state.children.len() > 128 {
        return Err(invalid());
    }
    for (index, saved) in state.children.iter().enumerate() {
        let child = &saved.child;
        let root = state
            .runs
            .iter()
            .find(|r| {
                r.run.run_id == saved.root.run_id && r.run.attempt_id == saved.root.attempt_id
            })
            .ok_or_else(invalid)?;
        if saved.agent_type.is_empty()
            || saved.agent_type.len() > 128
            || saved.task.is_empty()
            || saved.task.len() > 4096
            || child.task_ids.len() > 128
            || state
                .children
                .iter()
                .filter(|c| c.root == saved.root)
                .count()
                > 32
            || state
                .runs
                .iter()
                .any(|r| r.run.run_id == child.run_id || r.run.attempt_id == child.attempt_id)
            || state.children[..index]
                .iter()
                .any(|c| c.child.run_id == child.run_id || c.child.attempt_id == child.attempt_id)
            || child
                .task_ids
                .iter()
                .enumerate()
                .any(|(i, t)| child.task_ids[..i].contains(t) || !root.run.task_ids.contains(t))
            || (root.run.state.is_terminal() && !child.state.is_terminal())
        {
            return Err(invalid());
        }
        if child.parent_run_id != root.run.run_id {
            let parent = state.children[..index]
                .iter()
                .find(|c| c.root == saved.root && c.child.run_id == child.parent_run_id)
                .ok_or_else(invalid)?;
            if child
                .task_ids
                .iter()
                .any(|t| !parent.child.task_ids.contains(t))
            {
                return Err(invalid());
            }
        }
        if child.state != RunState::Running && !child.state.is_terminal() {
            return Err(invalid());
        }
    }
    Ok(())
}

/// The owner must join first. Success cannot conceal an unobserved child result.
pub(super) fn seal(
    state: &mut Sidecar,
    target: &RunTarget,
    outcome: Observation,
) -> StoreResult<()> {
    for saved in state
        .children
        .iter_mut()
        .filter(|c| c.root == *target && !c.child.state.is_terminal())
    {
        if outcome == Observation::Succeeded {
            return Err(StoreError::RunConflict);
        }
        saved.child.state = RunState::OutcomeUnknown;
    }
    Ok(())
}
