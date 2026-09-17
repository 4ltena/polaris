//! M7c-3a: what a bundled setting command has already done. One saved step
//! per recipe step, holding what it was asked to do and how it ended. A step
//! is reused only when the inputs it ran against are still the inputs today,
//! so a finished step never carries an old pass over a changed project.
use serde::{Deserialize, Serialize};

/// Bumped when the shape changes, so a saved step is never reinterpreted
/// under a different meaning.
pub const SCHEMA_VERSION: u32 = 1;

/// What the step was asked to do, fixed before it started.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedRecipeIntent {
    pub schema_version: u32,
    pub recipe_id: String,
    pub recipe_version: u32,
    pub step_id: String,
    /// The steps this one needed, so a reused step is not read out of order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub after: Vec<String>,
    /// A digest of the normalized arguments: the same request twice gives the
    /// same digest, and a changed request gives another.
    pub input_digest: String,
    /// The lockfile as it stood when the step began. `None` when the project
    /// had none, which is different from an empty one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lockfile_digest_before: Option<String>,
    /// Where the step would fetch from, when it fetches.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The versions asked for, before anything was resolved.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requested_versions: Vec<String>,
    /// The machine as it was: operating system, runtime, package manager.
    pub environment: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    Succeeded,
    Failed,
    Cancelled,
    /// The command ended without saying what it did, or the host could not
    /// read the result back. Never retried on its own.
    Unknown,
}

/// How the step ended, written once the step is over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedRecipeTerminal {
    pub outcome: StepOutcome,
    /// The process result. `None` for a step that ran no process.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signal: Option<i32>,
    /// The lockfile after the step. Compared against the next run's reading.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lockfile_digest_after: Option<String>,
    /// What actually went in, which is not always what was asked for.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub resolved_versions: Vec<String>,
    /// What was read back to confirm the step did what it claimed. An exit
    /// code alone is never that evidence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedRecipeStep {
    pub intent: SavedRecipeIntent,
    /// Absent while the step is still going, or after a crash that left it
    /// unsettled. Either way the step is not finished.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal: Option<SavedRecipeTerminal>,
}

/// What the project looks like now, for deciding what may be reused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Current {
    pub recipe_id: String,
    pub recipe_version: u32,
    pub input_digest: String,
    pub lockfile_digest: Option<String>,
    pub environment: String,
}

impl SavedRecipeStep {
    /// Whether this finished step still stands for the project as it is now.
    /// Only a success counts, and only when nothing it depended on moved:
    /// the same recipe at the same version, the same request, the same
    /// machine, and a lockfile that is still what the step left behind.
    pub fn reusable(&self, current: &Current) -> bool {
        let Some(terminal) = &self.terminal else {
            return false;
        };
        if terminal.outcome != StepOutcome::Succeeded
            || self.intent.schema_version != SCHEMA_VERSION
            || self.intent.recipe_id != current.recipe_id
            || self.intent.recipe_version != current.recipe_version
            || self.intent.input_digest != current.input_digest
            || self.intent.environment != current.environment
        {
            return false;
        }
        // The step's own writes are the baseline for the next run. When the
        // step did not touch the lockfile, what it read is the baseline.
        let expected = terminal
            .lockfile_digest_after
            .as_ref()
            .or(self.intent.lockfile_digest_before.as_ref());
        expected == current.lockfile_digest.as_ref()
    }
}

/// The steps that may be skipped, out of everything saved for this command.
/// A step whose predecessor cannot be reused is not reused either, so a
/// later step never stands on a stale earlier one.
pub fn reusable_steps(saved: &[SavedRecipeStep], current: &Current) -> Vec<String> {
    let mut kept: Vec<String> = Vec::new();
    for step in saved {
        if !step.reusable(current) {
            continue;
        }
        if step
            .intent
            .after
            .iter()
            .any(|needed| !kept.iter().any(|held| held == needed))
        {
            continue;
        }
        kept.push(step.intent.step_id.clone());
    }
    kept
}

use super::super::model::{IntentReceipt, StoreError, StoreResult};
use polaris_desktop_protocol::{ids::OperationId, request::RunTarget};

impl super::Writer {
    /// Saves what a step is about to do, before anything runs. The step is
    /// one operation of the run, so it is held where every other typed
    /// operation is held and published in the same commit.
    pub fn record_recipe_intent(
        &mut self,
        target: &RunTarget,
        operation: OperationId,
        intent: SavedRecipeIntent,
    ) -> StoreResult<IntentReceipt> {
        if intent.schema_version != SCHEMA_VERSION {
            return Err(StoreError::UnsupportedVersion(intent.schema_version));
        }
        let (mut next, receipt) = self.prepare_unapproved_intent(target, operation.clone())?;
        let index = self.run_index(target)?;
        let held = next.state.runs[index]
            .operations
            .iter_mut()
            .find(|held| held.operation_id == operation)
            .ok_or(StoreError::RunConflict)?;
        let saved = SavedRecipeStep {
            intent,
            terminal: None,
        };
        // A repeat of the same request is the same record or it is a
        // different operation wearing the same id.
        if receipt == IntentReceipt::AlreadyRecorded {
            if held.recipe.as_ref() != Some(&saved) {
                return Err(StoreError::RunConflict);
            }
            return Ok(receipt);
        }
        held.recipe = Some(saved);
        self.commit(next, true)?;
        Ok(receipt)
    }

    /// Saves how the step ended, onto the operation its intent was saved on.
    /// A step settles once: a second terminal is a conflict, not an update,
    /// so a re-run can never quietly turn a failure into a success.
    pub fn record_recipe_terminal(
        &mut self,
        target: &RunTarget,
        operation: &OperationId,
        terminal: SavedRecipeTerminal,
    ) -> StoreResult<()> {
        self.ready()?;
        let index = self.run_index(target)?;
        let mut next = self.published.clone();
        let held = next.state.runs[index]
            .operations
            .iter_mut()
            .find(|held| held.operation_id == *operation)
            .ok_or(StoreError::NotFound)?;
        let step = held.recipe.as_mut().ok_or(StoreError::NotFound)?;
        if step.terminal.is_some() {
            return Err(StoreError::RunConflict);
        }
        step.terminal = Some(terminal);
        self.commit(next, true)
    }

    /// Every recipe step this session has saved, in the order they were
    /// saved. What may be skipped is decided by `reusable_steps` from this,
    /// never by the order alone.
    pub fn recipe_steps(&self) -> Vec<SavedRecipeStep> {
        self.published
            .state
            .runs
            .iter()
            .flat_map(|run| run.operations.iter())
            .filter_map(|held| held.recipe.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests;
