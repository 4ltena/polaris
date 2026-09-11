//! Durable workflow evidence; never an execution grant or a disk skill loader.
use super::{StoreError, StoreResult};
use crate::workflow::{SessionWorkflow, WorkflowConfig, WorkflowGatesV1, WorkflowStateV1};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedWorkflow {
    pub config: WorkflowConfig,
    pub state: WorkflowStateV1,
    pub gates: WorkflowGatesV1,
}

impl SavedWorkflow {
    pub fn capture(workflow: &SessionWorkflow) -> Self {
        Self {
            config: workflow.config.clone(),
            state: workflow.state(),
            gates: workflow.gates.clone(),
        }
    }

    pub fn validate(&self) -> StoreResult<()> {
        if serde_json::to_vec(self)?.len() > 64 * 1024 {
            return Err(StoreError::Overflow);
        }
        SessionWorkflow::restore(self.config.clone(), self.gates.clone(), self.state.clone())
            .map_err(|_| StoreError::Corrupt("workflowの保存版"))?;
        Ok(())
    }

    /// The trusted owner supplies current artifact identity. Missing evidence
    /// cannot preserve a previously passed verification across external edits.
    pub fn restore(&self, artifact_hash: Option<&str>) -> StoreResult<SessionWorkflow> {
        self.validate()?;
        let mut gates = self.gates.clone();
        if let Some(evidence) = &mut gates.verification {
            evidence.stale |= artifact_hash != Some(evidence.artifact_hash.as_str());
        }
        SessionWorkflow::restore(self.config.clone(), gates, self.state.clone())
            .map_err(|_| StoreError::Corrupt("workflowの保存版"))
    }
}
