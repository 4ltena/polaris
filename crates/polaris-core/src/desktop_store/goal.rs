//! M6-3a: the goal a session is working toward, and the instructions that
//! arrived while a run was already going. A goal decides whether another run
//! is worth starting; it never occupies the input and never grants anything.
//! An instruction is durable as soon as it is received, and separately marked
//! when a run actually applied it, so an acknowledgement is not an apply.
use super::{StoreError, StoreResult};
use polaris_desktop_protocol::ids::{AttemptId, DecimalU64, RunId};
use serde::{Deserialize, Serialize};

pub const MAX_GOAL_BYTES: usize = 4096;
pub const MAX_INSTRUCTION_BYTES: usize = 4096;
/// The bound stops an unbounded queue from a client that never runs anything.
/// At the bound the oldest already-applied instruction makes room, so a long
/// session does not silently stop accepting them.
pub const MAX_INSTRUCTIONS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedGoal {
    pub text: String,
    pub revision: DecimalU64,
    /// A cleared goal keeps its revision so a later run cannot resurrect it.
    pub active: bool,
    /// Set by an explicit stop; only an explicit resume clears it.
    pub stopped: bool,
    /// Automatic continuations already spent for this goal (M6-2b). A new
    /// goal text starts a fresh budget; a stop and a resume do not.
    #[serde(default)]
    pub automatic_used: DecimalU64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedInstruction {
    pub instruction_id: String,
    pub text: String,
    /// The session revision at which it was received.
    pub received_revision: DecimalU64,
    /// The run that applied it. Absent means received but not yet applied.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub applied_to: Option<AppliedInstruction>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AppliedInstruction {
    pub run_id: RunId,
    pub attempt_id: AttemptId,
}

impl SavedGoal {
    pub fn validate(&self) -> StoreResult<()> {
        if self.revision.get() == 0
            || self.text.len() > MAX_GOAL_BYTES
            || (self.active && self.text.trim().is_empty())
        {
            return Err(StoreError::Corrupt("goal"));
        }
        Ok(())
    }
}

impl SavedInstruction {
    pub fn validate(&self) -> StoreResult<()> {
        if self.instruction_id.is_empty()
            || self.instruction_id.len() > 64
            || !self
                .instruction_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
            || self.text.trim().is_empty()
            || self.text.len() > MAX_INSTRUCTION_BYTES
            || self.received_revision.get() == 0
        {
            return Err(StoreError::Corrupt("instruction"));
        }
        Ok(())
    }
}

/// Call after deserialization and before publication.
pub fn validate(goal: Option<&SavedGoal>, instructions: &[SavedInstruction]) -> StoreResult<()> {
    if let Some(goal) = goal {
        goal.validate()?;
    }
    if instructions.len() > MAX_INSTRUCTIONS {
        return Err(StoreError::Overflow);
    }
    let mut seen = std::collections::BTreeSet::new();
    for instruction in instructions {
        instruction.validate()?;
        if !seen.insert(instruction.instruction_id.as_str()) {
            return Err(StoreError::Corrupt("instruction"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
