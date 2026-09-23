//! Durable workflow evidence; never an execution grant or a disk skill loader.
use super::{StoreError, StoreResult};
use crate::workflow::{
    SessionWorkflow, WorkflowArtifactGraphV1, WorkflowConfig, WorkflowGatesV1, WorkflowStateV1,
};
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
        if self.state.active_change_contract.is_some() {
            crate::workflow::Workflow::restore(self.state.clone())
                .map_err(|_| StoreError::Corrupt("workflowの保存版"))?;
            if self.gates.schema_version != crate::workflow::WORKFLOW_SCHEMA_VERSION {
                return Err(StoreError::Corrupt("workflowの保存版"));
            }
        } else {
            SessionWorkflow::restore(self.config.clone(), self.gates.clone(), self.state.clone())
                .map_err(|_| StoreError::Corrupt("workflowの保存版"))?;
        }
        Ok(())
    }

    pub fn validate_with_artifacts(&self, graph: &WorkflowArtifactGraphV1) -> StoreResult<()> {
        self.validate()?;
        SessionWorkflow::restore_with_artifacts(
            self.config.clone(),
            self.gates.clone(),
            self.state.clone(),
            graph.clone(),
        )
        .map_err(|_| StoreError::Corrupt("workflow artifact参照"))?;
        Ok(())
    }

    /// The trusted owner supplies current artifact identity. Missing evidence
    /// cannot preserve a previously passed verification across external edits.
    pub fn restore(&self, artifact_hash: Option<&str>) -> StoreResult<SessionWorkflow> {
        self.restore_with_artifacts(artifact_hash, WorkflowArtifactGraphV1::default())
    }

    pub fn restore_with_artifacts(
        &self,
        artifact_hash: Option<&str>,
        graph: WorkflowArtifactGraphV1,
    ) -> StoreResult<SessionWorkflow> {
        self.validate_with_artifacts(&graph)?;
        let mut gates = self.gates.clone();
        if let Some(evidence) = &mut gates.verification {
            evidence.stale |= artifact_hash != Some(evidence.artifact_hash.as_str());
        }
        SessionWorkflow::restore_with_artifacts(
            self.config.clone(),
            gates,
            self.state.clone(),
            graph,
        )
        .map_err(|_| StoreError::Corrupt("workflowの保存版"))
    }
}
