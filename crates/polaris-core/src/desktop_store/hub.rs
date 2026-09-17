//! M6-1: the one state window an agent reads, and the difference since what
//! it last received. Pure projection: this module reads the published state
//! and never writes, never resolves a credential, and never keeps history.
//! Everything here is already durable somewhere else, so the window adds no
//! second source of truth.
use super::model::Sidecar;
use polaris_desktop_protocol::run_state::RunState;
use polaris_desktop_protocol::snapshot::{AcceptanceState, PendingApprovalState, TaskState};
use serde::Serialize;

/// Bounds a single answer. The saved state is already bounded; this only
/// keeps one window small enough to hand to a model.
const MAX_ITEMS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct HubView {
    pub revision: u64,
    /// What the owner saw the run doing. Absent when no run is active.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<crate::activity::Activity>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<Verification>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<GoalSummary>,
    /// Received but not yet applied. An applied instruction is already in the
    /// run's own history, so it is not repeated here.
    pub instructions: Vec<InstructionSummary>,
    pub tasks: Vec<TaskSummary>,
    pub runs: Vec<RunSummary>,
    pub approvals: Vec<ApprovalSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct GoalSummary {
    pub text: String,
    pub revision: u64,
    pub active: bool,
    /// Stopped by the person; only an explicit resume clears it.
    pub stopped: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InstructionSummary {
    pub instruction_id: String,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Verification {
    pub passed: bool,
    pub stale: bool,
    /// Evidence reference, never the evidence itself.
    pub artifact_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct TaskSummary {
    pub task_id: String,
    pub title: String,
    pub state: TaskState,
    pub unmet_acceptance: usize,
    pub blockers: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RunSummary {
    pub run_id: String,
    pub attempt_id: String,
    pub state: RunState,
    pub task_ids: Vec<String>,
    /// The registration this run was fixed to at acceptance (M5-3).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ApprovalSummary {
    pub approval_id: String,
    pub run_id: String,
    pub operation: String,
    pub scope: String,
    pub expires_at_unix_ms: u64,
}

/// What changed since the view the caller last received. `Resync` carries the
/// whole window because the caller's base is unknown or too old.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Delta {
    Resync(HubView),
    Changes(Changes),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Changes {
    pub base: u64,
    pub revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub activity: Option<Option<crate::activity::Activity>>,
    /// Present only when it changed; `Some(None)` means it was cleared.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub phase: Option<Option<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification: Option<Option<Verification>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goal: Option<Option<GoalSummary>>,
    pub instructions: ListChange<InstructionSummary>,
    pub tasks: ListChange<TaskSummary>,
    pub runs: ListChange<RunSummary>,
    pub approvals: ListChange<ApprovalSummary>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Default)]
pub struct ListChange<T> {
    pub changed: Vec<T>,
    pub removed: Vec<String>,
}

impl<T> ListChange<T> {
    pub fn is_empty(&self) -> bool {
        self.changed.is_empty() && self.removed.is_empty()
    }
}

impl Changes {
    /// Nothing to report: the caller keeps what it had, and no summary is
    /// generated just to have something to say.
    pub fn is_empty(&self) -> bool {
        self.activity.is_none()
            && self.goal.is_none()
            && self.instructions.is_empty()
            && self.phase.is_none()
            && self.verification.is_none()
            && self.tasks.is_empty()
            && self.runs.is_empty()
            && self.approvals.is_empty()
    }
}

pub fn view(
    state: &Sidecar,
    revision: u64,
    activity: Option<crate::activity::Activity>,
) -> HubView {
    HubView {
        revision,
        activity,
        goal: state.goal.as_ref().map(|goal| GoalSummary {
            text: goal.text.clone(),
            revision: goal.revision.get(),
            active: goal.active,
            stopped: goal.stopped,
        }),
        instructions: state
            .instructions
            .iter()
            .filter(|instruction| instruction.applied_to.is_none())
            .take(MAX_ITEMS)
            .map(|instruction| InstructionSummary {
                instruction_id: instruction.instruction_id.clone(),
                text: instruction.text.clone(),
            })
            .collect(),
        phase: state
            .workflow
            .as_ref()
            .map(|workflow| format!("{:?}", workflow.state.phase).to_lowercase()),
        verification: state.workflow.as_ref().and_then(|workflow| {
            workflow
                .gates
                .verification
                .as_ref()
                .map(|evidence| Verification {
                    passed: evidence.passed,
                    stale: evidence.stale,
                    artifact_hash: evidence.artifact_hash.clone(),
                })
        }),
        tasks: state
            .tasks
            .iter()
            .take(MAX_ITEMS)
            .map(|task| TaskSummary {
                task_id: task.task_id.as_str().to_owned(),
                title: task.title.clone(),
                state: task.state,
                unmet_acceptance: task
                    .acceptance
                    .iter()
                    .filter(|criterion| criterion.state != AcceptanceState::Passed)
                    .count(),
                blockers: task
                    .blockers
                    .iter()
                    .map(|blocker| format!("{:?}", blocker.kind).to_lowercase())
                    .collect(),
            })
            .collect(),
        runs: state
            .runs
            .iter()
            .rev()
            .take(MAX_ITEMS)
            .map(|record| RunSummary {
                run_id: record.run.run_id.as_str().to_owned(),
                attempt_id: record.run.attempt_id.as_str().to_owned(),
                state: record.run.state,
                task_ids: record
                    .run
                    .task_ids
                    .iter()
                    .map(|id| id.as_str().to_owned())
                    .collect(),
                account: record
                    .account
                    .as_ref()
                    .map(|account| account.registration_id.clone()),
            })
            .collect(),
        approvals: state
            .unresolved_approvals
            .iter()
            .filter(|approval| approval.state == PendingApprovalState::Pending)
            .take(MAX_ITEMS)
            .map(|approval| ApprovalSummary {
                approval_id: approval.approval_id.as_str().to_owned(),
                run_id: approval.run_id.as_str().to_owned(),
                operation: approval.operation.clone(),
                scope: approval.scope.clone(),
                expires_at_unix_ms: approval.expires_at_unix_ms.get(),
            })
            .collect(),
    }
}

/// The caller's `since` must name the revision of the view it holds; anything
/// else is a gap and resynchronizes.
pub fn delta(previous: Option<&HubView>, current: &HubView, since: Option<u64>) -> Delta {
    let Some(previous) = previous.filter(|held| Some(held.revision) == since) else {
        return Delta::Resync(current.clone());
    };
    Delta::Changes(Changes {
        base: previous.revision,
        revision: current.revision,
        activity: (previous.activity != current.activity).then_some(current.activity),
        goal: (previous.goal != current.goal).then(|| current.goal.clone()),
        instructions: list(
            &previous.instructions,
            &current.instructions,
            |instruction| instruction.instruction_id.clone(),
        ),
        phase: (previous.phase != current.phase).then(|| current.phase.clone()),
        verification: (previous.verification != current.verification)
            .then(|| current.verification.clone()),
        tasks: list(&previous.tasks, &current.tasks, |task| task.task_id.clone()),
        runs: list(&previous.runs, &current.runs, |run| {
            format!("{}:{}", run.run_id, run.attempt_id)
        }),
        approvals: list(&previous.approvals, &current.approvals, |approval| {
            approval.approval_id.clone()
        }),
    })
}

fn list<T: Clone + PartialEq>(
    previous: &[T],
    current: &[T],
    key: impl Fn(&T) -> String,
) -> ListChange<T> {
    let held: Vec<(String, &T)> = previous.iter().map(|item| (key(item), item)).collect();
    let now: Vec<(String, &T)> = current.iter().map(|item| (key(item), item)).collect();
    ListChange {
        changed: now
            .iter()
            .filter(|(id, item)| {
                !held
                    .iter()
                    .any(|(held_id, held_item)| held_id == id && held_item == item)
            })
            .map(|(_, item)| (*item).clone())
            .collect(),
        removed: held
            .iter()
            .filter(|(id, _)| !now.iter().any(|(current_id, _)| current_id == id))
            .map(|(id, _)| id.clone())
            .collect(),
    }
}

#[cfg(test)]
mod tests;
