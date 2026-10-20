//! Request history with optional durable v2 storage or a v3 desktop owner port.

use polaris_provider::{Message, ReasoningItem, ToolCall};

use std::{collections::BTreeSet, sync::Arc};

use crate::subagent_control::{CancelResultV1, LogicalChildId, ParentBindingV1};
#[cfg(test)]
use crate::subagent_control::{
    CapabilitySnapshotV1, ChildAttemptId, ChildBindingV1, ChildControlPlaneV1, ChildStateV1,
    RootAttemptId, RootRunId, TaskId,
};
#[cfg(test)]
use std::sync::Mutex;

/// One exact host-authorized routine scope for the current user turn.
///
/// The model cannot construct this value through a tool call.  A root host
/// must bind already-validated routine eligibility to the exact prompt before
/// the agent may materialize or publish a routine change contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoutineChangeAuthorizationV1 {
    prompt_sha256: String,
    eligibility: crate::workflow::RoutineEligibilityV1,
    verification: Vec<crate::workflow::VerificationCriterionV1>,
}

impl RoutineChangeAuthorizationV1 {
    pub fn new(
        previous_user_prompt: &str,
        eligibility: crate::workflow::RoutineEligibilityV1,
        verification: Vec<crate::workflow::VerificationCriterionV1>,
    ) -> Result<Self, String> {
        eligibility.validate()?;
        if eligibility.blocker().is_some() {
            return Err("routine authorizationには単一componentの低影響変更が必要です".into());
        }
        if verification.is_empty() {
            return Err("routine authorizationにはhost固定の検証基準が必要です".into());
        }
        let mut ids = BTreeSet::new();
        for criterion in &verification {
            criterion.validate()?;
            if !criterion.has_explicit_evidence_kinds() {
                return Err("routine authorizationにはtyped検証基準が必要です".into());
            }
            if !ids.insert(criterion.id.clone()) {
                return Err("routine authorizationの検証基準が重複しています".into());
            }
        }
        Ok(Self {
            prompt_sha256: crate::agent::workflow_artifact::prompt_sha256(previous_user_prompt),
            eligibility,
            verification,
        })
    }

    pub(crate) fn contract_input_for(
        &self,
        previous_user_prompt: &str,
    ) -> Option<(
        &crate::workflow::RoutineEligibilityV1,
        &[crate::workflow::VerificationCriterionV1],
    )> {
        (self.prompt_sha256 == crate::agent::workflow_artifact::prompt_sha256(previous_user_prompt))
            .then_some((&self.eligibility, self.verification.as_slice()))
    }
}

mod delegation;
pub(crate) use delegation::ArmedChildMcpCaptureV1;
#[cfg(test)]
use delegation::unix_time_ms;
pub use delegation::{
    ChildCancelAckV1, ChildControlProjectionV1, ChildWaitSnapshotV1, DelegationPersistenceHookV1,
    DelegationProjectionHookV1, RootCancelAckV1, RootDelegationCheckpointV1,
    RootDelegationConfigV1, RootDelegationControlV1, RootDelegationHostBindingV1,
    RootDelegationProjectionV1, RuntimeCancelEffectV1,
};

#[derive(Default, Clone)]
pub struct Session {
    /// Installed by root entry points only; new child sessions do not inherit it.
    #[doc(hidden)]
    pub request_prompt: Option<crate::prompt::AlwaysOn>,
    pub messages: Vec<Message>,
    /// Configuration-provided examples; never real user turns or raw events.
    pub examples: Vec<Message>,
    /// Frozen root configuration; never inherited by a new child session.
    pub input_preparation: crate::input_preparation::InputPreparationMode,
    /// Ephemeral correspondence produced by the raw-history owner, never persisted.
    #[doc(hidden)]
    pub input_source: Option<crate::request_input::BoundInput>,
    pub persistence: Option<crate::session_store::PersistedSession>,
    pub strict_history: Option<std::sync::Arc<crate::conversation_memory::StrictHistory>>,
    pub summary_usage: Option<polaris_provider::UsageMeter>,
    /// Current preparation only. The run owner records this without using the
    /// model's reference budget; reset before every preparation, including failures.
    #[doc(hidden)]
    pub memory_observation: Option<crate::conversation_memory::observation::MemoryObservation>,
    /// Sticky write failure: a failed append must never lead to a later request.
    #[doc(hidden)]
    pub persistence_error: Option<String>,
    /// What resumable subagents left behind, for the length of this run.
    /// Shared with every wave the session starts, so a follow-up addressed
    /// to the same worker continues rather than re-reads.
    pub child_memory: crate::spawn::ChildMemory,
    /// Host-owned root control plane. Children receive the same owner only
    /// through a typed parent binding; model text can neither install nor
    /// alter it.
    pub delegation_control: Option<Arc<RootDelegationControlV1>>,
    /// Host-owned verification bridge. It is absent from children and
    /// sidechats; model text only carries opaque IDs into this port.
    pub verification_owner:
        Option<Arc<dyn crate::verification::checkpoint::VerificationToolOwnerV1>>,
    /// Durable checkpoint metadata is reference-only. Running checkpoints are
    /// recovered as outcome_unknown before another model request is made.
    pub verification_checkpoint:
        Option<crate::verification::checkpoint::SavedVerificationCheckpointV1>,
    #[doc(hidden)]
    pub delegation_parent: Option<ParentBindingV1>,
    /// Immutable parent-captured MCP evidence for this exact child attempt.
    /// It is projected into requests only and never pushed to history.
    #[doc(hidden)]
    pub leased_mcp_reads: Option<crate::mcp::lease::ChildMcpReadContextV1>,
    /// Optional v2 workflow runtime. Never inferred from model or tool text.
    pub workflow: Option<crate::workflow::SessionWorkflow>,
    /// Exact host authorization for one routine change proposal.  It is
    /// prompt-bound, ephemeral, root-only and never inferred from model text.
    #[doc(hidden)]
    pub routine_change_authorization: Option<RoutineChangeAuthorizationV1>,
    /// Saves the full original history before a valid summary replaces it.
    pub before_compact: Option<std::sync::Arc<crate::compaction::ArchiveHook>>,
    /// Optional local override; no model window is inferred.
    pub compaction_threshold: Option<usize>,
    /// Automatic retry watermark; transient and not persisted with history.
    pub compaction_retry_tokens: Option<usize>,
    /// Explicitly configured durable tool-result storage; never serialized as credentials.
    pub tool_memory: Option<crate::tool_memory::ToolMemory>,
    /// Stops only root-triggered automatic `files.md` regeneration. Manual
    /// `spawn` remains available, and the default preserves legacy behavior.
    pub disable_files_md_auto_regenerate: bool,
    /// Desktop-only unmodified turn output, independent of request projections.
    /// The trusted worker initializes this; it is never a replacement for disk publication.
    pub desktop_transcript: Option<DesktopTranscript>,
    /// Trusted v3 owner port. It can prepare request context but cannot replace raw history.
    pub desktop_memory: Option<std::sync::Arc<crate::desktop_memory::DesktopMemory>>,
    /// Owner-mediated atomic workflow/history checkpoints; never a second writer.
    pub desktop_workflow: Option<std::sync::Arc<crate::desktop_workflow::DesktopWorkflow>>,
    /// Root-only MCP detection and approved registration; children never get it.
    pub mcp_setup: Option<std::sync::Arc<crate::mcp::setup::SetupTool>>,
    /// Weighs each delegated task against what its type declared. `None`
    /// keeps the declaration, which is the ordinary case and not a
    /// degraded one — the weight still routes, it is simply not refined.
    pub tier_decider: Option<std::sync::Arc<dyn crate::tier::TierDecider>>,
    /// The typed judge, used to narrow the optional skill catalogue before
    /// the first request. `None` keeps the lexical order, which is what
    /// every run did before the judge existed.
    pub decider: Option<std::sync::Arc<dyn polaris_provider::decision::Decider>>,
    /// The light model that settles what the judge left undecided. The
    /// same independently constructed selector the MCP catalogue uses.
    pub light_model: Option<std::sync::Arc<dyn polaris_provider::Provider>>,
    /// How much of `--approval always`'s band the person agreed in advance
    /// to stop being asked about (M18). `Off` unless the project's
    /// `[approval] auto` says otherwise, and it takes a judge as well —
    /// neither alone changes anything.
    pub approval_auto: crate::approval::AutoApproval,
    /// Root-only bundled setting commands. The model passes an operation
    /// name, never a command; a real install stays the person's to confirm.
    pub setting: Option<std::sync::Arc<crate::recipe::tool::SettingTool>>,
    /// Present when this machine can drive an application: macOS, the
    /// accessibility permission granted, and a typed decider to choose
    /// between the candidates the accessibility tree offers. Absent
    /// everywhere else, and the `computer` tool is absent with it.
    pub computer: Option<std::sync::Arc<crate::computer::ComputerTool>>,
    /// What the judge said this turn is, or that it could not say. Kept so
    /// a classification can be attributed and a failure cannot be mistaken
    /// for a decision.
    pub turn_intent: Option<crate::turn_intent::TurnIntentV1>,
    /// Request-only bootstrap for a broad first-turn project analysis. It is
    /// derived from the project root and never persisted as conversation data.
    pub project_analysis: crate::project_analysis::ProjectAnalysisState,
    /// TUI root turns clear any prior override, then apply the effort chosen
    /// by the typed judge before the first main-model request. Explicit CLI
    /// measurement effort leaves this false.
    pub automatic_effort: bool,
    /// Whether the run is waiting on the user. While it is, no tool that
    /// changes a file is offered.
    pub clarification: crate::turn_intent::ClarificationState,
}

#[derive(Default, Clone)]
pub struct DesktopTranscript {
    messages: Vec<Message>,
}
impl DesktopTranscript {
    pub(crate) fn messages(&self) -> &[Message] {
        &self.messages
    }

    pub(crate) fn into_messages(self) -> Vec<Message> {
        self.messages
    }
}

/// Deterministic request-only annotations for a user-authorized continuation.
/// Missing tool outcomes are explicitly unknown and never trigger reexecution.
/// The durable raw history remains unchanged, including its interrupted calls.
pub fn desktop_request_history(raw: &[Message]) -> std::io::Result<Vec<Message>> {
    desktop_request_history_bound(raw, None).map(|(messages, _)| messages)
}

pub(crate) fn desktop_request_history_bound(
    raw: &[Message],
    mut source: Option<crate::request_input::BoundInput>,
) -> std::io::Result<(Vec<Message>, Option<crate::request_input::BoundInput>)> {
    use polaris_provider::Role;
    let mut projected = Vec::new();
    let mut pending: Vec<String> = Vec::new();
    let source_index = source.as_ref().map(|bound| bound.index);
    for (index, message) in raw.iter().enumerate() {
        if message.role != Role::Tool {
            for id in pending.drain(..) {
                projected.push(Message::tool_result(&id,
                    "中断により結果は不明です。外部への影響を確認してから続行してください。自動再実行はしていません。"));
            }
        }
        match message.role {
            Role::Assistant => {
                if message.tool_call_id.is_some() {
                    return Err(std::io::Error::other("invalid assistant history"));
                }
                for call in &message.tool_calls {
                    if call.id.is_empty() || pending.contains(&call.id) {
                        return Err(std::io::Error::other("invalid tool call history"));
                    }
                    pending.push(call.id.clone());
                }
            }
            Role::Tool => {
                if !message.tool_calls.is_empty() {
                    return Err(std::io::Error::other("invalid tool result history"));
                }
                let Some(index) = message
                    .tool_call_id
                    .as_ref()
                    .and_then(|id| pending.iter().position(|p| p == id))
                else {
                    return Err(std::io::Error::other("orphan tool result history"));
                };
                pending.remove(index);
            }
            Role::User => {
                if !message.tool_calls.is_empty() || message.tool_call_id.is_some() {
                    return Err(std::io::Error::other("invalid user history"));
                }
            }
        }
        if source_index == Some(index)
            && let Some(source) = &mut source
        {
            source.index = projected.len();
        }
        projected.push(message.clone());
    }
    for id in pending {
        projected.push(Message::tool_result(&id,
            "中断により結果は不明です。外部への影響を確認してから続行してください。自動再実行はしていません。"));
    }
    Ok((projected, source))
}

impl Session {
    /// Save reference-only verification metadata through the existing session
    /// marker owner. Desktop owners expose their own typed path later; a
    /// desktop transcript without that path fails closed.
    pub(crate) fn persist_verification_checkpoint(
        &mut self,
        checkpoint: crate::verification::checkpoint::SavedVerificationCheckpointV1,
    ) -> std::io::Result<()> {
        self.check_persistence()?;
        if self.desktop_transcript.is_some() {
            return Err(std::io::Error::other(
                "desktop verification checkpoint ownerがありません",
            ));
        }
        let store = self.persistence.as_ref().ok_or_else(|| {
            std::io::Error::other("verification checkpointには永続sessionが必要です")
        })?;
        let generation = store.snapshot()?.state.generation;
        store.publish_verification_checkpoint(generation, checkpoint.clone())?;
        self.verification_checkpoint = Some(checkpoint);
        Ok(())
    }

    pub(crate) fn publish_verification_convergence(
        &mut self,
        checkpoint: crate::verification::SavedVerificationCheckpointV1,
        bundle: crate::verification::owner::VerifiedConvergenceBundleV1,
    ) -> std::io::Result<()> {
        self.check_persistence()?;
        if self.desktop_transcript.is_some() {
            return Err(std::io::Error::other(
                "desktop verification convergence ownerがありません",
            ));
        }
        let store = self.persistence.as_ref().ok_or_else(|| {
            std::io::Error::other("verification convergenceには永続sessionが必要です")
        })?;
        let generation = store.snapshot()?.state.generation;
        let (workflow, checkpoint) =
            store.publish_verification_convergence(generation, checkpoint, bundle)?;
        self.workflow = Some(workflow);
        self.verification_checkpoint = Some(checkpoint);
        Ok(())
    }

    /// Restore durable checkpoint metadata before the next tool request. A
    /// crashed running adapter is never resumed automatically.
    pub fn restore_verification_checkpoint(&mut self) -> std::io::Result<()> {
        let Some(store) = self.persistence.as_ref() else {
            return Ok(());
        };
        let snapshot = store.snapshot()?;
        let Some(mut checkpoint) = snapshot.state.verification_checkpoint else {
            return Ok(());
        };
        let was_running =
            checkpoint.stage == crate::verification::VerificationCheckpointStageV1::Running;
        checkpoint.mark_crash_recovered();
        checkpoint.validate().map_err(std::io::Error::other)?;
        if was_running {
            store.publish_verification_checkpoint(snapshot.state.generation, checkpoint.clone())?;
        }
        self.verification_checkpoint = Some(checkpoint);
        Ok(())
    }
    pub fn new() -> Self {
        Self::default()
    }

    pub fn install_root_delegation_control(
        &mut self,
        control: Arc<RootDelegationControlV1>,
    ) -> Result<(), String> {
        if self.delegation_control.is_some() || self.delegation_parent.is_some() {
            return Err("root delegation control is already installed".into());
        }
        self.delegation_control = Some(control);
        self.delegation_parent = Some(ParentBindingV1::Root);
        Ok(())
    }

    pub(crate) fn install_child_delegation_control(
        &mut self,
        control: Arc<RootDelegationControlV1>,
        logical_child_id: LogicalChildId,
        reads: crate::mcp::lease::ChildMcpReadContextV1,
    ) {
        self.delegation_control = Some(control);
        self.delegation_parent = Some(ParentBindingV1::Child { logical_child_id });
        self.leased_mcp_reads = Some(reads);
    }

    pub(crate) fn issue_spawn_control(
        &self,
        count: usize,
    ) -> Result<Option<crate::spawn::SpawnControlPort>, String> {
        let Some(control) = &self.delegation_control else {
            return Ok(None);
        };
        let parent = self
            .delegation_parent
            .clone()
            .ok_or_else(|| "delegation parent binding is missing".to_owned())?;
        control.issue_spawn_port(count, parent).map(Some)
    }

    /// Root-only host preparation for one possible immediate MCP-read →
    /// spawn handoff. Absence of a root owner is the ordinary no-lease path.
    pub(crate) fn arm_child_mcp_capture(
        &self,
    ) -> Result<Option<ArmedChildMcpCaptureV1>, crate::mcp::lease::LeaseError> {
        match (&self.delegation_control, &self.delegation_parent) {
            (Some(control), Some(ParentBindingV1::Root)) => {
                control.arm_child_mcp_capture().map(Some)
            }
            _ => Ok(None),
        }
    }

    pub(crate) fn capture_armed_parent_mcp_read(
        &self,
        armed: &ArmedChildMcpCaptureV1,
        call: &ToolCall,
        message: &Message,
    ) -> Result<bool, crate::mcp::lease::LeaseError> {
        let control = self
            .delegation_control
            .as_ref()
            .ok_or(crate::mcp::lease::LeaseError::Unavailable)?;
        control.capture_armed_parent_mcp_read(armed, call, message)
    }

    pub(crate) fn bind_armed_capture_to_next_spawn(
        &self,
        armed: &ArmedChildMcpCaptureV1,
        child_count: usize,
    ) -> Result<(), String> {
        let control = self
            .delegation_control
            .as_ref()
            .ok_or_else(|| "MCP capture owner is unavailable".to_owned())?;
        control.bind_armed_capture_to_next_spawn(armed, child_count)
    }

    pub(crate) fn discard_armed_child_mcp_capture(
        &self,
        armed: &ArmedChildMcpCaptureV1,
    ) -> Result<usize, crate::mcp::lease::LeaseError> {
        let control = self
            .delegation_control
            .as_ref()
            .ok_or(crate::mcp::lease::LeaseError::Unavailable)?;
        control.discard_armed_child_mcp_capture(armed)
    }

    /// Terminal root cleanup for one-shot hosts. Restores deliberately create
    /// a fresh owner, so this never revives a previous process's leases.
    pub async fn revoke_root_mcp_leases(&self) -> usize {
        match (&self.delegation_control, &self.delegation_parent) {
            (Some(control), Some(ParentBindingV1::Root)) => control.revoke_root_terminal().await,
            _ => 0,
        }
    }

    /// Finish one interactive root turn and remove its process-local control
    /// plane before the next turn installs fresh task/run coordinates.
    ///
    /// Interactive hosts reuse `Session` across turns. Keeping the previous
    /// owner would bind later children to a stale active task and retain MCP
    /// snapshots past their root lifetime. An interrupted turn first records
    /// a root cancellation intent; normal completion only revokes leases.
    pub async fn finish_root_delegation_control(
        &mut self,
        cancel_running: bool,
    ) -> Result<usize, String> {
        let Some(control) = self.delegation_control.take() else {
            self.delegation_parent = None;
            self.leased_mcp_reads = None;
            return Ok(0);
        };
        if !matches!(self.delegation_parent, Some(ParentBindingV1::Root)) {
            self.delegation_control = Some(control);
            return Err("only a root session can finish root delegation control".into());
        }

        let mut cancellation_error = None;
        if cancel_running {
            let snapshot = control.snapshot().await;
            match control
                .request_root_cancel(&control.host_binding(), snapshot.revision)
                .await
            {
                Ok(ack) => {
                    for child in ack.children {
                        if !matches!(child.result, CancelResultV1::AlreadyTerminal { .. })
                            && let Err(error) =
                                control.finish_runtime_abort_unknown(&child.reference).await
                        {
                            cancellation_error.get_or_insert(error);
                        }
                    }
                }
                Err(error) => cancellation_error = Some(error),
            }
        }
        let revoked = control.revoke_root_terminal().await;
        self.delegation_parent = None;
        self.leased_mcp_reads = None;
        match cancellation_error {
            Some(error) => Err(error),
            None => Ok(revoked),
        }
    }

    /// Publish a candidate only after its result and history can be saved together.
    /// The controller remains at the previous phase until the caller receives ACK.
    pub(crate) async fn publish_workflow(
        &mut self,
        candidate: &crate::workflow::WorkflowStateV1,
        result: Option<polaris_provider::Message>,
    ) -> std::io::Result<()> {
        self.publish_workflow_with_gate(candidate, result, None)
            .await
    }

    /// Publish a phase transition only with the exact gate snapshot captured
    /// by `PreparedWorkflowTransition`. The hash is evidence for the durable
    /// owner to re-evaluate; it never grants permission by itself.
    pub(crate) async fn publish_workflow_transition(
        &mut self,
        candidate: &crate::workflow::WorkflowStateV1,
        result: Option<polaris_provider::Message>,
        gate_basis_hash: &str,
    ) -> std::io::Result<()> {
        self.publish_workflow_with_gate(candidate, result, Some(gate_basis_hash))
            .await
    }

    /// Publishes the bounded routine-change contract through the single
    /// durable owner and installs the exact acknowledged workflow snapshot.
    /// The caller supplies prose fields only; approval, revision, hashes and
    /// the active reference are all derived and validated by the owner.
    pub(crate) async fn publish_and_approve_routine_contract(
        &mut self,
        expected_workflow_revision: u64,
        contract: crate::workflow::ChangeContractV1,
    ) -> std::io::Result<crate::workflow::ArtifactRefV1> {
        self.check_persistence()?;
        let current = self
            .workflow
            .as_ref()
            .ok_or_else(|| std::io::Error::other("workflowがありません"))?;
        if !current.config.enabled
            || current.state().phase != crate::workflow::Phase::Specify
            || current.state().revision != expected_workflow_revision
        {
            return Err(std::io::Error::other(
                "routine contractは現在のspecify revisionにだけ公開できます",
            ));
        }
        if contract.header.status != crate::workflow::ArtifactStatusV1::Draft
            || !contract.eligibility.is_routine()
        {
            return Err(std::io::Error::other(
                "routine条件を満たすdraftだけを公開できます",
            ));
        }

        // Set before handing the request to another owner. If the future is
        // dropped after commit but before ACK, no later request may overwrite
        // the durable workflow with this process's stale snapshot.
        self.persistence_error =
            Some("routine contract保存の結果が未確認です。保存状態を復元してください".into());
        let next = if let Some(owner) = self.desktop_workflow.clone() {
            owner
                .publish_and_approve_routine_contract(expected_workflow_revision, contract)
                .await
        } else if self.desktop_transcript.is_some() {
            Err(std::io::Error::other(
                "workflow artifactの保存所有者がありません",
            ))
        } else if let Some(store) = &self.persistence {
            store.publish_and_approve_routine_contract(expected_workflow_revision, contract)
        } else {
            Err(std::io::Error::other(
                "routine contractには永続sessionが必要です",
            ))
        };
        let next = match next {
            Ok(next) => next,
            Err(error) => {
                self.persistence_error = Some(error.to_string());
                return Err(error);
            }
        };
        let reference =
            next.state().active_change_contract.clone().ok_or_else(|| {
                std::io::Error::other("active change contractが保存されませんでした")
            })?;
        self.workflow = Some(next);
        self.persistence_error = None;
        Ok(reference)
    }

    /// Publish current observed verification evidence through the sole durable
    /// owner. The owner derives the active contract/task binding and required
    /// criterion set from immutable bodies; callers cannot supply either.
    #[expect(
        dead_code,
        reason = "session publication path is retained for non-desktop callers"
    )]
    pub(crate) async fn publish_convergence_trusted(
        &mut self,
        expected_workflow_revision: u64,
        bundle: crate::verification::owner::VerifiedConvergenceBundleV1,
    ) -> std::io::Result<()> {
        self.check_persistence()?;
        let current = self
            .workflow
            .as_ref()
            .ok_or_else(|| std::io::Error::other("workflowがありません"))?;
        if !current.config.enabled || current.state().revision != expected_workflow_revision {
            return Err(std::io::Error::other(
                "convergenceは現在のworkflow revisionにだけ保存できます",
            ));
        }
        self.persistence_error =
            Some("convergence保存の結果が未確認です。保存状態を復元してください".into());
        let next = if let Some(owner) = self.desktop_workflow.clone() {
            owner
                .publish_convergence_trusted(expected_workflow_revision, bundle)
                .await
        } else if self.desktop_transcript.is_some() {
            Err(std::io::Error::other("convergenceの保存所有者がありません"))
        } else if let Some(store) = &self.persistence {
            let expected_generation = store.snapshot()?.state.generation;
            store.publish_convergence_trusted(
                expected_generation,
                expected_workflow_revision,
                bundle,
            )
        } else {
            Err(std::io::Error::other(
                "convergenceには永続sessionが必要です",
            ))
        };
        match next {
            Ok(next) => {
                self.workflow = Some(next);
                self.persistence_error = None;
                Ok(())
            }
            Err(error) => {
                self.persistence_error = Some(error.to_string());
                Err(error)
            }
        }
    }

    /// Reads one exact body through the session's current durable owner. The
    /// complete reference and kind must already be present in the metadata
    /// graph; this never resolves aliases or a latest revision.
    pub(crate) async fn read_workflow_artifact(
        &self,
        kind: crate::workflow::WorkflowArtifactKindV1,
        reference: crate::workflow::ArtifactRefV1,
    ) -> std::io::Result<serde_json::Value> {
        self.check_persistence()?;
        if let Some(owner) = self.desktop_workflow.clone() {
            return owner.read_workflow_artifact(kind, reference).await;
        }
        if self.desktop_transcript.is_some() {
            return Err(std::io::Error::other(
                "workflow artifactの保存所有者がありません",
            ));
        }
        let store = self
            .persistence
            .as_ref()
            .ok_or_else(|| std::io::Error::other("workflow artifactには永続sessionが必要です"))?;
        let value = match kind {
            crate::workflow::WorkflowArtifactKindV1::ProjectPrinciples => {
                serde_json::to_value(store.load_project_principles(&reference)?)
            }
            crate::workflow::WorkflowArtifactKindV1::IdeaDecision => {
                serde_json::to_value(store.load_idea_decision(&reference)?)
            }
            crate::workflow::WorkflowArtifactKindV1::ChangeContract => {
                serde_json::to_value(store.load_change_contract(&reference)?)
            }
            crate::workflow::WorkflowArtifactKindV1::FeatureSpec => {
                serde_json::to_value(store.load_feature_spec(&reference)?)
            }
            crate::workflow::WorkflowArtifactKindV1::ImplementationPlan => {
                serde_json::to_value(store.load_implementation_plan(&reference)?)
            }
            crate::workflow::WorkflowArtifactKindV1::TaskGraph => {
                serde_json::to_value(store.load_task_graph(&reference)?)
            }
        }
        .map_err(std::io::Error::other)?;
        Ok(value)
    }

    async fn publish_workflow_with_gate(
        &mut self,
        candidate: &crate::workflow::WorkflowStateV1,
        result: Option<polaris_provider::Message>,
        gate_basis_hash: Option<&str>,
    ) -> std::io::Result<()> {
        self.check_persistence()?;
        // Set before awaiting: dropping the future can lose an ACK after the
        // writer committed. Only successful publication clears this latch.
        self.persistence_error =
            Some("workflow保存の結果が未確認です。保存状態を復元してください".into());
        let outcome = self
            .publish_workflow_inner(candidate, result, gate_basis_hash)
            .await;
        if let Err(error) = &outcome {
            // An uncertain publication must not be overwritten by a later
            // checkpoint using the controller's previous in-memory revision.
            self.persistence_error = Some(error.to_string());
        } else {
            self.persistence_error = None;
        }
        outcome
    }

    async fn publish_workflow_inner(
        &mut self,
        candidate: &crate::workflow::WorkflowStateV1,
        result: Option<polaris_provider::Message>,
        gate_basis_hash: Option<&str>,
    ) -> std::io::Result<()> {
        let current = self
            .workflow
            .as_ref()
            .ok_or_else(|| std::io::Error::other("workflowがありません"))?;
        let previous = crate::desktop_store::SavedWorkflow::capture(current);
        let phase_changed = previous.state.phase != candidate.phase;
        if phase_changed && current.gates.phase_gate_schema_version.is_some() {
            let supplied = gate_basis_hash
                .ok_or_else(|| std::io::Error::other("workflow phase gateの根拠がありません"))?;
            let proof = crate::workflow::evaluate_phase_transition(
                &previous.state,
                candidate.phase,
                &current.gates,
                current.artifact_graph(),
            )
            .map_err(std::io::Error::other)?;
            if proof.basis_hash != supplied {
                return Err(std::io::Error::other(
                    "workflow phase gateの根拠が保存状態と一致しません",
                ));
            }
        } else if let Some(supplied) = gate_basis_hash {
            let proof = crate::workflow::evaluate_phase_transition(
                &previous.state,
                candidate.phase,
                &current.gates,
                current.artifact_graph(),
            )
            .map_err(std::io::Error::other)?;
            if proof.basis_hash != supplied {
                return Err(std::io::Error::other(
                    "workflow phase gateの根拠が保存状態と一致しません",
                ));
            }
        }
        let next = crate::workflow::SessionWorkflow::restore_with_artifacts(
            current.config.clone(),
            current.gates.clone(),
            candidate.clone(),
            current.artifact_graph().clone(),
        )
        .map_err(std::io::Error::other)?;
        if let Some(port) = &self.desktop_workflow {
            let mut suffix = self
                .desktop_transcript
                .as_ref()
                .ok_or_else(|| std::io::Error::other("workflowの原文がありません"))?
                .messages
                .clone();
            if let Some(message) = &result {
                suffix.push(message.clone());
            }
            crate::desktop_store::validate_turn_suffix(
                &suffix,
                polaris_desktop_protocol::run_state::Observation::Cancelled,
            )
            .map_err(std::io::Error::other)?;
            port.checkpoint(
                suffix,
                previous,
                crate::desktop_store::SavedWorkflow::capture(&next),
                result
                    .as_ref()
                    .and_then(|message| message.tool_call_id.clone()),
                gate_basis_hash.map(str::to_owned),
            )
            .await?;
        } else if self.desktop_transcript.is_some() {
            return Err(std::io::Error::other("workflowの保存所有者がありません"));
        } else if let Some(store) = &self.persistence {
            let messages: Vec<_> = result.iter().cloned().collect();
            store.append_batch(&messages, Some(&next))?;
        }
        if let Some(message) = result {
            if let Some(transcript) = &mut self.desktop_transcript {
                transcript.messages.push(message.clone());
            }
            self.messages.push(message);
        }
        Ok(())
    }

    /// Claim one automatic attempt per substantial increase in history.
    /// Set before awaiting so cancellation also cannot cause a tight retry loop.
    pub(crate) fn begin_compaction_attempt(&mut self) -> bool {
        let tokens = crate::compaction::session_tokens(&self.messages);
        if let Some(previous) = self.compaction_retry_tokens {
            let growth = crate::compaction::MIN_TOKENS_TO_SUMMARIZE.max(previous / 10);
            if tokens >= previous && tokens.saturating_sub(previous) < growth {
                return false;
            }
        }
        self.compaction_retry_tokens = Some(tokens);
        true
    }

    pub(crate) async fn compact_automatically(
        &mut self,
        provider: &dyn polaris_provider::Provider,
    ) -> Result<Option<crate::compaction::CompactionReport>, crate::compaction::CompactionError>
    {
        let previous_retry_tokens = self.compaction_retry_tokens;
        if !self.begin_compaction_attempt() {
            return Ok(None);
        }
        let outcome = crate::compaction::compact_with_usage(
            provider,
            &mut self.messages,
            self.before_compact.as_deref(),
        )
        .await;
        if matches!(outcome.result, Ok(Some(_))) {
            // This summarizer does not carry raw identities across its rewrite.
            self.input_source = None;
        }
        let usage = outcome.usage_report;
        if usage.reported_responses == 0
            && usage.missing_responses == 0
            && usage.failed_requests == 0
        {
            // No eligible range (or too little to summarize): no provider
            // attempt occurred, so a short new turn must remain eligible.
            self.compaction_retry_tokens = previous_retry_tokens;
        } else if matches!(outcome.result, Ok(Some(_))) {
            self.compaction_retry_tokens = None;
        }
        outcome.result
    }

    pub fn push_user(&mut self, content: &str) {
        if let Err(error) = self.recover_interrupted_tools() {
            self.persistence_error = Some(error.to_string());
            return;
        }
        self.push_message(Message::user(content), true);
    }

    /// Commit one real input and its host-selected context under the same
    /// saved marker. The supplemental message never starts another turn.
    pub fn push_user_with_supplement(
        &mut self,
        content: &str,
        supplement: Message,
    ) -> std::io::Result<()> {
        self.recover_interrupted_tools()?;
        self.check_persistence()?;
        if self.desktop_transcript.is_some() {
            return Err(std::io::Error::other(
                "desktop inputはdesktop writerが所有します",
            ));
        }
        let message = Message::user(content);
        if let Some(persistence) = &self.persistence {
            let source = persistence.append_input_with_supplement(
                message.clone(),
                supplement.clone(),
                self.workflow.as_ref(),
            )?;
            self.input_source = source.map(|source| {
                crate::request_input::BoundInput::new(
                    source,
                    self.messages.len(),
                    message.content.len(),
                )
            });
        }
        self.messages.push(message);
        self.messages.push(supplement);
        Ok(())
    }

    /// Commit one session-manager admission exactly once. The caller advances
    /// the manager only after this returns `true`; `false` is an exact replay
    /// whose raw input was already committed and must not start another run.
    pub fn push_admitted_user(
        &mut self,
        content: &str,
        admission_id: &crate::session_manager::AdmissionId,
        payload_hash: &str,
    ) -> std::io::Result<bool> {
        self.push_admitted_user_inner(content, None, admission_id, payload_hash)
    }

    pub fn push_admitted_user_with_supplement(
        &mut self,
        content: &str,
        supplement: Message,
        admission_id: &crate::session_manager::AdmissionId,
        payload_hash: &str,
    ) -> std::io::Result<bool> {
        self.push_admitted_user_inner(content, Some(supplement), admission_id, payload_hash)
    }

    fn push_admitted_user_inner(
        &mut self,
        content: &str,
        supplement: Option<Message>,
        admission_id: &crate::session_manager::AdmissionId,
        payload_hash: &str,
    ) -> std::io::Result<bool> {
        self.recover_interrupted_tools()?;
        self.check_persistence()?;
        if self.desktop_transcript.is_some() {
            return Err(std::io::Error::other(
                "desktop admissionはdesktop writerが所有します",
            ));
        }
        let persistence = self
            .persistence
            .as_ref()
            .ok_or_else(|| std::io::Error::other("sidechat admissionにはv2セッションが必要です"))?;
        let already_committed = persistence
            .snapshot()?
            .state
            .admission_receipts
            .contains_key(admission_id.as_str());
        let message = Message::user(content);
        let source = match &supplement {
            Some(supplement) => persistence.append_admitted_input_with_supplement(
                message.clone(),
                supplement.clone(),
                self.workflow.as_ref(),
                admission_id,
                payload_hash,
            ),
            None => persistence.append_admitted_input(
                message.clone(),
                self.workflow.as_ref(),
                admission_id,
                payload_hash,
            ),
        }?;
        if already_committed {
            return Ok(false);
        }
        self.input_source = source.map(|source| {
            crate::request_input::BoundInput::new(
                source,
                self.messages.len(),
                message.content.len(),
            )
        });
        self.messages.push(message);
        if let Some(supplement) = supplement {
            self.messages.push(supplement);
        }
        Ok(true)
    }

    /// Mid-run steering is persisted but is never a fresh preparation candidate.
    pub(crate) fn push_instruction(&mut self, content: &str) {
        let original = self.input_source.take();
        self.push_user(content);
        self.input_source = original;
    }

    /// Rebuilt request slots retain eligibility from the original accepted input.
    pub(crate) fn refresh_input_source(
        &mut self,
        candidate: Option<crate::request_input::BoundInput>,
    ) {
        self.input_source = self
            .input_source
            .as_ref()
            .and_then(|source| candidate.and_then(|candidate| source.refreshed(candidate)));
    }

    /// Close only unresolved calls when the user resumes or begins a new turn.
    /// Their external effects are unknown; never retry a mutation automatically.
    pub fn recover_interrupted_tools(&mut self) -> std::io::Result<()> {
        self.check_persistence()?;
        let messages = if let Some(saved) = &self.persistence {
            let snapshot = saved.snapshot()?;
            snapshot
                .events
                .into_iter()
                .filter(|e| e.epoch == snapshot.state.epoch)
                .map(|e| e.message)
                .collect::<Vec<_>>()
        } else {
            self.messages.clone()
        };
        let mut pending = std::collections::BTreeSet::new();
        for message in &messages {
            for call in &message.tool_calls {
                pending.insert(call.id.clone());
            }
            if let Some(id) = &message.tool_call_id {
                pending.remove(id);
            }
        }
        if !pending.is_empty() {
            self.messages = messages;
            self.input_source = None;
        }
        for id in pending {
            self.push_tool_result(&id, "中断により結果は不明です。外部への影響を確認してから続行してください。自動再実行はしていません。");
            self.check_persistence()?;
        }
        Ok(())
    }

    pub fn push_assistant(&mut self, content: &str, reasoning: Vec<ReasoningItem>) {
        self.push_message(Message::assistant(content).with_reasoning(reasoning), false);
    }

    /// Records an assistant turn together with the tool calls it made.
    /// Under OpenAI's round-trip protocol, this message must remain in the
    /// history holding its own `tool_calls` before the tool results are sent.
    pub fn push_assistant_tool_calls(
        &mut self,
        content: &str,
        tool_calls: Vec<ToolCall>,
        reasoning: Vec<ReasoningItem>,
    ) {
        self.push_message(
            Message::assistant_with_tool_calls(content, tool_calls).with_reasoning(reasoning),
            false,
        );
    }

    /// Records a tool result, tying it to the id of the call it responds to.
    pub fn push_tool_result(&mut self, tool_call_id: &str, content: &str) {
        self.push_message(Message::tool_result(tool_call_id, content), false);
    }

    /// Few-shot and retrieved user-role text must pass false here; only real
    /// user input advances the durable turn counter.
    pub fn push_message(&mut self, message: Message, starts_turn: bool) {
        if self.persistence_error.is_some() {
            return;
        }
        if starts_turn {
            self.input_source = None;
        }
        if let Some(transcript) = &mut self.desktop_transcript {
            transcript.messages.push(message);
            // Validate before cloning into the mutable request view. Partial
            // tool groups are legitimate until completion, but user injection
            // or an oversized turn prevents further provider/tool dispatch.
            if let Err(error) = crate::desktop_store::validate_turn_suffix(
                &transcript.messages,
                polaris_desktop_protocol::run_state::Observation::Cancelled,
            ) {
                transcript.messages.pop();
                self.persistence_error = Some(error.to_string());
                return;
            }
            self.messages
                .push(transcript.messages.last().unwrap().clone());
            return;
        }
        if let Some(persistence) = &self.persistence {
            let result = if starts_turn {
                persistence.append_input(message.clone(), self.workflow.as_ref())
            } else {
                persistence
                    .append(message.clone(), false, self.workflow.as_ref())
                    .map(|_| None)
            };
            match result {
                Ok(Some(source)) => {
                    self.input_source = Some(crate::request_input::BoundInput::new(
                        source,
                        self.messages.len(),
                        message.content.len(),
                    ))
                }
                Ok(None) => {}
                Err(error) => {
                    self.persistence_error = Some(error.to_string());
                    return;
                }
            }
        }
        self.messages.push(message);
    }

    pub fn check_persistence(&self) -> std::io::Result<()> {
        if let Some(error) = &self.persistence_error {
            return Err(std::io::Error::other(error.clone()));
        }
        if let Some(persistence) = &self.persistence {
            persistence.snapshot()?;
        }
        Ok(())
    }

    pub fn checkpoint(&self) -> std::io::Result<()> {
        self.check_persistence()?;
        if let Some(persistence) = &self.persistence {
            persistence.checkpoint(self.workflow.as_ref())?;
        }
        Ok(())
    }

    /// Restore raw messages from a verified attachment. Request-window
    /// preparation remains the agent's responsibility before its next send.
    pub fn attach(
        &mut self,
        persistence: crate::session_store::PersistedSession,
    ) -> std::io::Result<()> {
        let snapshot = persistence.snapshot()?;
        self.input_source = None;
        self.messages = snapshot
            .events
            .into_iter()
            .filter(|event| event.epoch == snapshot.state.epoch)
            .map(|event| event.message)
            .collect();
        self.persistence = Some(persistence);
        self.persistence_error = None;
        Ok(())
    }

    pub fn clear_history(&mut self) -> std::io::Result<()> {
        self.check_persistence()?;
        if let Some(persistence) = &self.persistence {
            persistence.clear()?;
        }
        self.messages.clear();
        self.input_source = None;
        self.compaction_retry_tokens = None;
        Ok(())
    }

    pub fn uses_strict_history(&self) -> std::io::Result<bool> {
        if self.desktop_memory.is_some() {
            return Ok(true);
        }
        Ok(self
            .persistence
            .as_ref()
            .map(|saved| saved.snapshot())
            .transpose()?
            .is_some_and(|snapshot| {
                matches!(
                    snapshot.state.history_mode,
                    crate::conversation_state::HistoryMode::Strict10
                        | crate::conversation_state::HistoryMode::Structured10
                )
            }))
    }

    /// Publish only an already-started structured attempt before any checkpoint.
    /// An unknown response fails closed; this never starts a new model request.
    pub async fn recover_saved_history(&mut self) -> std::io::Result<()> {
        self.check_persistence()?;
        let structured = self
            .persistence
            .as_ref()
            .map(|saved| saved.snapshot())
            .transpose()?
            .is_some_and(|s| {
                s.state.history_mode == crate::conversation_state::HistoryMode::Structured10
            });
        if structured {
            self.recover_owned_history(true).await?;
        }
        Ok(())
    }

    /// Recover a pending expired turn before a resumed input changes its raw hash.
    pub async fn recover_history(&mut self) -> std::io::Result<()> {
        self.recover_saved_history().await?;
        self.recover_interrupted_tools()?;
        if !self.uses_strict_history()? {
            return Ok(());
        }
        self.recover_owned_history(false).await
    }

    async fn recover_owned_history(&self, saved_only: bool) -> std::io::Result<()> {
        let history = self
            .strict_history
            .as_ref()
            .ok_or_else(|| std::io::Error::other("strict10の埋め込み・要約backendがありません"))?;
        let saved = self
            .persistence
            .as_ref()
            .ok_or_else(|| std::io::Error::other("strict10には永続保存が必要です"))?;
        let recovered = if saved_only {
            history
                .recover_saved_structured_observed(&saved.store, &saved.database, &|snapshot| {
                    saved.accept_publication(snapshot)
                })
                .await?
        } else {
            history
                .recover_pending_observed(&saved.store, &saved.database, &|snapshot| {
                    saved.accept_publication(snapshot)
                })
                .await?
        };
        saved.accept_prepared(&recovered.snapshot)?;
        Ok(())
    }

    /// Runs once at a real user boundary. Returned evidence is ephemeral and
    /// never appended as another user turn or fed into the raw archive.
    pub async fn prepare_history(&mut self) -> std::io::Result<Option<Message>> {
        self.memory_observation = None;
        self.check_persistence()?;
        if let Some(memory) = &self.desktop_memory {
            let prepared = match memory.prepare().await {
                Ok(prepared) => prepared,
                Err(error) => {
                    if let Some(blocked) = error.get_ref().and_then(|inner| inner.downcast_ref::<crate::conversation_memory::observation::MemoryPreparationBlocked>()) {
                        self.memory_observation = Some(blocked.observation.clone());
                    }
                    return Err(error);
                }
            };
            self.messages = prepared.messages;
            self.refresh_input_source(prepared.input_source);
            self.memory_observation = Some(prepared.observation);
            return Ok(prepared.evidence);
        }
        if !self.uses_strict_history()? {
            return Ok(None);
        }
        let history = self
            .strict_history
            .as_ref()
            .ok_or_else(|| std::io::Error::other("strict10の埋め込み・要約backendがありません"))?;
        let saved = self
            .persistence
            .as_ref()
            .ok_or_else(|| std::io::Error::other("strict10には永続保存が必要です"))?;
        let snapshot = saved.snapshot()?;
        let query = snapshot
            .events
            .iter()
            .rev()
            .find(|event| event.epoch == snapshot.state.epoch && event.starts_turn)
            .ok_or_else(|| std::io::Error::other("現在のユーザー入力がありません"))?
            .message
            .content
            .clone();
        let prepared = history
            .prepare_for_session_observed(&saved.store, &saved.database, &query, &|snapshot| {
                saved.accept_publication(snapshot)
            })
            .await?;
        self.accept_prepared_history(prepared)
    }

    fn accept_prepared_history(
        &mut self,
        prepared: crate::conversation_memory::PreparedHistory,
    ) -> std::io::Result<Option<Message>> {
        self.memory_observation = None;
        self.persistence
            .as_ref()
            .ok_or_else(|| std::io::Error::other("履歴保存がありません"))?
            .accept_prepared(&prepared.snapshot)?;
        let request = match &prepared.structured {
            Some(context) => crate::conversation_memory::observation::prepare_structured(context)?,
            None => crate::conversation_memory::observation::prepare_strict(&prepared.retrieval)?,
        };
        self.memory_observation = Some(request.observation.clone());
        let request = request.require_dispatch()?;
        let (messages, source) = prepared
            .snapshot
            .recent_request(self.input_source.as_ref().map(|bound| &bound.source));
        self.messages = messages;
        self.refresh_input_source(source);
        Ok(request.text.map(Message::user))
    }
}

#[cfg(test)]
mod desktop_transcript_tests {
    //! Raw output must survive request-only retention and refuse excess before dispatch.
    use super::*;
    #[tokio::test]
    async fn workflow_publication_failure_or_cancellation_prevents_later_checkpoint() {
        use polaris_desktop_protocol::{
            ids::{AttemptId, RunId},
            request::RunTarget,
        };
        use std::{
            future::Future,
            task::{Context, Poll, Waker},
        };
        for cancelled in [false, true] {
            let mut session = Session::new();
            session.workflow = Some(crate::workflow::SessionWorkflow::new(
                crate::config::Config::default().workflow,
            ));
            session.desktop_transcript = Some(Default::default());
            let (port, owner) = crate::desktop_workflow::DesktopWorkflow::pair(RunTarget {
                run_id: RunId::new("run").unwrap(),
                attempt_id: AttemptId::new("attempt").unwrap(),
            });
            session.desktop_workflow = Some(port);
            let previous = session.workflow.as_ref().unwrap().state();
            let mut candidate = previous.clone();
            candidate.revision = previous.revision + 1;
            if cancelled {
                let mut future = Box::pin(session.publish_workflow(&candidate, None));
                match future
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                {
                    Poll::Pending => {}
                    Poll::Ready(result) => {
                        panic!(
                            "workflow publication settled before owner acknowledgement: {result:?}"
                        )
                    }
                }
                drop(future);
                drop(owner);
            } else {
                drop(owner);
                assert!(session.publish_workflow(&candidate, None).await.is_err());
            }
            assert_eq!(session.workflow.as_ref().unwrap().state(), previous);
            assert!(session.messages.is_empty());
            assert!(session.checkpoint().is_err());
            assert!(session.check_persistence().is_err());
        }
    }

    #[test]
    fn interrupted_request_projection_closes_unknown_calls_without_changing_raw() {
        let raw = vec![
            Message::assistant_with_tool_calls(
                "",
                vec![ToolCall {
                    id: "missing".into(),
                    name: "bash".into(),
                    arguments: serde_json::json!({"command":"do not repeat"}),
                }],
            ),
            Message::user("continue after inspecting effects"),
        ];
        let projected = desktop_request_history(&raw).unwrap();
        assert_eq!(raw.len(), 2);
        assert_eq!(projected.len(), 3);
        assert_eq!(projected[1].tool_call_id.as_deref(), Some("missing"));
        assert!(projected[1].content.contains("結果は不明"));
        assert_eq!(projected[2].content, raw[1].content);
        assert!(desktop_request_history(&[Message::tool_result("orphan", "value")]).is_err());
    }
    #[test]
    fn request_projection_does_not_replace_original_tool_output() {
        let mut session = Session::new();
        session.desktop_transcript = Some(Default::default());
        session.push_assistant_tool_calls(
            "",
            vec![ToolCall {
                id: "call".into(),
                name: "read".into(),
                arguments: serde_json::json!({"path":"file"}),
            }],
            vec![],
        );
        session.push_tool_result("call", "full original evidence");
        session.messages[1].content = "retained excerpt".into();
        session.push_assistant("done", vec![]);
        assert_eq!(
            session.desktop_transcript.unwrap().messages[1].content,
            "full original evidence"
        );
    }
    #[test]
    fn oversized_or_user_suffix_stops_further_messages_without_truncating() {
        for bad in [
            Message::user("injected"),
            Message::assistant("\0".repeat(22_000)),
        ] {
            let mut session = Session::new();
            session.desktop_transcript = Some(Default::default());
            session.push_message(bad, false);
            assert!(session.check_persistence().is_err());
            session.push_assistant("must not continue", vec![]);
            assert!(session.messages.is_empty());
            assert!(session.desktop_transcript.unwrap().messages.is_empty());
        }
    }
}

#[cfg(test)]
mod routine_authorization_tests {
    use super::*;

    fn verification() -> Vec<crate::workflow::VerificationCriterionV1> {
        vec![
            crate::workflow::VerificationCriterionV1::typed(
                "The focused common case passes.",
                "The focused common case fails.",
                true,
                [crate::workflow::CriterionEvidenceKindV1::CommonCase]
                    .into_iter()
                    .collect(),
            )
            .unwrap(),
        ]
    }

    #[test]
    fn m27_routine_authorization_is_host_bound_to_one_exact_prompt() {
        let authorization = RoutineChangeAuthorizationV1::new(
            "Fix the local parser",
            crate::workflow::RoutineEligibilityV1::local_component("polaris-core"),
            verification(),
        )
        .unwrap();
        assert!(
            authorization
                .contract_input_for("Fix the local parser")
                .is_some()
        );
        assert!(authorization.contract_input_for("Fix the parser").is_none());
    }

    #[test]
    fn m27_routine_authorization_rejects_unknown_or_cross_component_impact() {
        let mut unknown = crate::workflow::RoutineEligibilityV1::local_component("core");
        unknown.unknown_impact = true;
        assert!(RoutineChangeAuthorizationV1::new("request", unknown, verification()).is_err());

        let mut cross_component = crate::workflow::RoutineEligibilityV1::local_component("core");
        cross_component.affected_components.push("desktop".into());
        assert!(
            RoutineChangeAuthorizationV1::new("request", cross_component, verification()).is_err()
        );
    }
}

#[cfg(test)]
mod memory_observation_tests {
    use super::*;
    use crate::conversation_memory::{
        PreparedHistory,
        observation::MemoryPreparationBlocked,
        structured::{
            packing::{RetrievalStatus, prepare_context},
            sources::publication_view,
        },
    };
    use crate::conversation_state::HistoryMode;

    #[tokio::test]
    async fn m17_blocked_observation_requires_exact_snapshot_and_legacy_clears_it() {
        for stale in [false, true] {
            for status in [
                RetrievalStatus::ExtractionIncomplete,
                RetrievalStatus::EvidenceBudgetExceeded,
            ] {
                let root = tempfile::tempdir().unwrap();
                let database = root.path().join("memory.sqlite3");
                let saved = crate::session_store::PersistedSession::create(
                    root.path(),
                    &database,
                    "p",
                    HistoryMode::Strict10,
                    None,
                )
                .unwrap();
                let mut initial = saved.snapshot().unwrap().state;
                initial.history_mode = HistoryMode::Structured10;
                std::fs::write(
                    saved.store.directory().join("state.json"),
                    serde_json::to_vec(&initial).unwrap(),
                )
                .unwrap();
                let snapshot = saved.snapshot().unwrap();
                let mut db = polaris_memory::MemoryStore::open(&database).unwrap();
                db.enable_structured_memory().unwrap();
                let mut context = prepare_context(
                    &db,
                    &snapshot,
                    &publication_view(&snapshot.state).unwrap(),
                    "unknown",
                    |_, _| panic!("empty state has no sources"),
                )
                .unwrap();
                context.status = status;
                let prepared = PreparedHistory {
                    published_generation: snapshot.state.generation,
                    published_raw_hash: snapshot.raw_hash.clone(),
                    snapshot,
                    messages: vec![],
                    retrieval: vec![],
                    structured: Some(context),
                };
                let mut session = Session::new();
                session.persistence = Some(saved);
                if stale {
                    session.push_user("concurrent new input");
                }
                let error = session.accept_prepared_history(prepared).unwrap_err();
                if stale {
                    assert!(session.memory_observation.is_none());
                    assert!(!error.get_ref().unwrap().is::<MemoryPreparationBlocked>());
                    assert_eq!(
                        session.messages.last().unwrap().content,
                        "concurrent new input"
                    );
                } else {
                    let observation = session.memory_observation.as_ref().unwrap();
                    assert_eq!(observation.retrieval_status, Some(status));
                    assert!(!observation.dispatch_allowed());
                    assert!(error.get_ref().unwrap().is::<MemoryPreparationBlocked>());
                    session.persistence = None;
                    assert!(session.prepare_history().await.unwrap().is_none());
                    assert!(session.memory_observation.is_none());
                }
            }
        }
    }

    #[tokio::test]
    async fn root_owner_issues_unique_child_ids_without_model_task_text() {
        let control = RootDelegationControlV1::new(RootDelegationConfigV1 {
            session_id: "session-1".into(),
            root_run_id: RootRunId::new("root-1").unwrap(),
            root_attempt_id: RootAttemptId::new("root-attempt-1").unwrap(),
            active_task_id: TaskId::new("task-1").unwrap(),
            parent_capabilities: CapabilitySnapshotV1 {
                sandbox: crate::subagent_control::SandboxCapabilityV1::ReadOnly,
                write_roots: vec![],
                tool_names: vec!["read".into(), "spawn".into()],
                approval: crate::subagent_control::ApprovalCapabilityV1::Never,
                mcp: crate::subagent_control::McpCapabilityV1::ReadSnapshotsOnly,
            },
            model: "gpt-6-astra".into(),
            effort: "xhigh".into(),
        })
        .unwrap();
        let mut session = Session::new();
        session
            .install_root_delegation_control(control.clone())
            .unwrap();
        assert!(
            control
                .bind_next_spawn_tasks(vec![TaskId::new("model-selected-task").unwrap()])
                .is_err()
        );
        let first = session.issue_spawn_control(2).unwrap().unwrap();
        let second = session.issue_spawn_control(1).unwrap().unwrap();
        assert_eq!(first.binding.parent, ParentBindingV1::Root);
        assert_eq!(first.model, "gpt-6-astra");
        assert_eq!(first.effort, "xhigh");
        assert_ne!(
            first.children[0].logical_child_id,
            first.children[1].logical_child_id
        );
        assert_ne!(
            first.children[1].logical_child_id,
            second.children[0].logical_child_id
        );
        let projection = control.snapshot().await;
        assert_eq!(projection.session_id, "session-1");
        assert!(projection.children.is_empty());
        assert!(projection.mcp_leases.is_empty());
    }

    #[tokio::test]
    async fn host_only_mcp_capture_binds_exact_next_child_and_root_cleanup_revokes_it() {
        let control = RootDelegationControlV1::new(RootDelegationConfigV1 {
            session_id: "session-1".into(),
            root_run_id: RootRunId::new("root-1").unwrap(),
            root_attempt_id: RootAttemptId::new("root-attempt-1").unwrap(),
            active_task_id: TaskId::new("task-1").unwrap(),
            parent_capabilities: CapabilitySnapshotV1 {
                sandbox: crate::subagent_control::SandboxCapabilityV1::ReadOnly,
                write_roots: vec![],
                tool_names: vec!["read".into(), "spawn".into()],
                approval: crate::subagent_control::ApprovalCapabilityV1::Never,
                mcp: crate::subagent_control::McpCapabilityV1::ReadSnapshotsOnly,
            },
            model: "gpt-6-astra".into(),
            effort: "xhigh".into(),
        })
        .unwrap();
        let mut session = Session::new();
        session
            .install_root_delegation_control(control.clone())
            .unwrap();
        let armed = session.arm_child_mcp_capture().unwrap().unwrap();
        let call = ToolCall {
            id: "read-1".into(),
            name: "mcp_catalog".into(),
            arguments: serde_json::json!({
                "op":"read",
                "server":"docs",
                "key":"memory://read-1",
                "generation":"gen-1",
            }),
        };
        let canonical = Message::tool_result(
            "read-1",
            serde_json::json!({
                "schema_version":1,
                "intent":{},
                "disposition":"completed",
                "reply":{
                    "status":"complete",
                    "parts":[{"kind":"text","text":"Authorization: Bearer synthetic"}],
                },
                "failure":null,
            })
            .to_string(),
        );
        // The normal agent path appends the canonical reply before capture.
        session.push_message(canonical, false);
        session.check_persistence().unwrap();
        let stored = session.messages.last().unwrap().clone();
        assert!(
            session
                .capture_armed_parent_mcp_read(&armed, &call, &stored)
                .unwrap()
        );
        session.bind_armed_capture_to_next_spawn(&armed, 1).unwrap();
        let port = session.issue_spawn_control(1).unwrap().unwrap();
        let ids = &port.children[0];
        let exact = crate::mcp::lease::ChildLeaseBindingV1 {
            root_run_id: port.binding.root_run_id.clone(),
            child_attempt_id: ids.first_attempt_id.clone(),
            task_id: ids.task_id.clone(),
        };
        let (reads, guard) = port
            .lease_owner
            .as_ref()
            .unwrap()
            .grant_child(exact.clone(), unix_time_ms())
            .unwrap();
        let body = reads
            .request_message(unix_time_ms())
            .unwrap()
            .unwrap()
            .content;
        assert!(body.contains("[REDACTED]"));
        assert!(!body.contains("synthetic"));
        let sibling = crate::mcp::lease::ChildLeaseBindingV1 {
            task_id: TaskId::new("other-task").unwrap(),
            ..exact
        };
        let (sibling_reads, sibling_guard) = port
            .lease_owner
            .as_ref()
            .unwrap()
            .grant_child(sibling, unix_time_ms())
            .unwrap();
        assert!(
            sibling_reads
                .request_message(unix_time_ms())
                .unwrap()
                .is_none()
        );
        drop(sibling_guard);
        assert_eq!(session.revoke_root_mcp_leases().await, 1);
        assert!(matches!(
            reads.request_message(unix_time_ms()),
            Err(crate::mcp::lease::LeaseError::Revoked)
        ));
        drop(guard);
    }

    #[tokio::test]
    async fn host_persistence_receives_full_plane_once_per_secret_free_revision() {
        let persisted = Arc::new(Mutex::new(Vec::<RootDelegationCheckpointV1>::new()));
        let persisted_for_hook = persisted.clone();
        let control = RootDelegationControlV1::new_with_persistence_hook(
            RootDelegationConfigV1 {
                session_id: "session-1".into(),
                root_run_id: RootRunId::new("root-1").unwrap(),
                root_attempt_id: RootAttemptId::new("root-attempt-1").unwrap(),
                active_task_id: TaskId::new("task-1").unwrap(),
                parent_capabilities: CapabilitySnapshotV1 {
                    sandbox: crate::subagent_control::SandboxCapabilityV1::ReadOnly,
                    write_roots: vec![],
                    tool_names: vec!["read".into(), "spawn".into()],
                    approval: crate::subagent_control::ApprovalCapabilityV1::Never,
                    mcp: crate::subagent_control::McpCapabilityV1::ReadSnapshotsOnly,
                },
                model: "gpt-6-astra".into(),
                effort: "xhigh".into(),
            },
            Arc::new(move |checkpoint| {
                persisted_for_hook.lock().unwrap().push(checkpoint);
                Ok(())
            }),
        )
        .unwrap();
        let projected = Arc::new(Mutex::new(Vec::<RootDelegationProjectionV1>::new()));
        let projected_for_hook = projected.clone();
        control.set_projection_hook(Some(Arc::new(move |projection| {
            projected_for_hook.lock().unwrap().push(projection);
        })));

        let task_id = control.reserve_child_task_id().unwrap();
        control
            .bind_next_spawn_tasks(vec![task_id.clone()])
            .unwrap();
        let port = control.issue_spawn_port(1, ParentBindingV1::Root).unwrap();
        assert_eq!(port.children[0].task_id, task_id);
        let reference = crate::subagent_control::ChildAttemptRefV1 {
            logical_child_id: port.children[0].logical_child_id.clone(),
            attempt_id: port.children[0].first_attempt_id.clone(),
        };
        let queued = {
            let mut plane = port.plane.lock().await;
            plane
                .register_before_start(crate::subagent_control::RegisterChildV1 {
                    logical_child_id: reference.logical_child_id.clone(),
                    attempt_id: reference.attempt_id.clone(),
                    binding: port.binding.clone(),
                    role: "file-reader".into(),
                    tier: "dwarf".into(),
                    model: port.model.clone(),
                    effort: port.effort.clone(),
                    capabilities: port.parent_capabilities.clone(),
                })
                .unwrap();
            plane.clone()
        };
        port.on_change.as_ref().unwrap()(&queued).unwrap();
        let running = {
            let mut plane = port.plane.lock().await;
            plane.mark_running(&reference, &port.binding).unwrap();
            plane.clone()
        };
        port.on_change.as_ref().unwrap()(&running).unwrap();

        {
            let checkpoints = persisted.lock().unwrap();
            assert_eq!(checkpoints.len(), 2);
            assert_eq!(
                (checkpoints[0].expected_revision, checkpoints[0].revision),
                (0, 1)
            );
            assert_eq!(
                (checkpoints[1].expected_revision, checkpoints[1].revision),
                (1, 2)
            );
            assert_eq!(checkpoints[0].binding.session_id, "session-1");
            assert_eq!(checkpoints[0].binding.root_run_id.0, "root-1");
            assert_eq!(
                checkpoints[0].control_plane.children[0].attempts[0].state,
                ChildStateV1::Queued
            );
            assert_eq!(
                checkpoints[1].control_plane.children[0].attempts[0].state,
                ChildStateV1::Running
            );
        }

        {
            let projections = projected.lock().unwrap();
            assert_eq!(
                projections
                    .iter()
                    .map(|entry| entry.revision)
                    .collect::<Vec<_>>(),
                vec![1, 2]
            );
            assert_eq!(projections[0].children[0].state, ChildStateV1::Queued);
            assert_eq!(projections[1].children[0].state, ChildStateV1::Running);
        }
        assert_eq!(control.snapshot().await.revision, 2);
        assert!(
            control
                .install_persistence_hook(2, Arc::new(|_| Ok(())))
                .is_err()
        );
    }

    #[tokio::test]
    async fn restored_running_child_becomes_durable_outcome_unknown_without_restoring_leases() {
        let config = RootDelegationConfigV1 {
            session_id: "session-1".into(),
            root_run_id: RootRunId::new("root-1").unwrap(),
            root_attempt_id: RootAttemptId::new("root-attempt-1").unwrap(),
            active_task_id: TaskId::new("task-1").unwrap(),
            parent_capabilities: CapabilitySnapshotV1 {
                sandbox: crate::subagent_control::SandboxCapabilityV1::ReadOnly,
                write_roots: vec![],
                tool_names: vec!["read".into(), "spawn".into()],
                approval: crate::subagent_control::ApprovalCapabilityV1::Never,
                mcp: crate::subagent_control::McpCapabilityV1::ReadSnapshotsOnly,
            },
            model: "gpt-6-astra".into(),
            effort: "xhigh".into(),
        };
        let binding = ChildBindingV1 {
            root_run_id: config.root_run_id.clone(),
            root_attempt_id: config.root_attempt_id.clone(),
            active_task_id: config.active_task_id.clone(),
            parent: ParentBindingV1::Root,
        };
        let reference = crate::subagent_control::ChildAttemptRefV1 {
            logical_child_id: LogicalChildId::new("child-1").unwrap(),
            attempt_id: ChildAttemptId::new("attempt-1").unwrap(),
        };
        let mut plane = ChildControlPlaneV1::default();
        plane
            .register_before_start(crate::subagent_control::RegisterChildV1 {
                logical_child_id: reference.logical_child_id.clone(),
                attempt_id: reference.attempt_id.clone(),
                binding: binding.clone(),
                role: "file-reader".into(),
                tier: "dwarf".into(),
                model: "gpt-6-astra".into(),
                effort: "xhigh".into(),
                capabilities: config.parent_capabilities.clone(),
            })
            .unwrap();
        plane.mark_running(&reference, &binding).unwrap();
        let persisted = Arc::new(Mutex::new(Vec::<RootDelegationCheckpointV1>::new()));
        let persisted_for_hook = persisted.clone();
        let restored = RootDelegationControlV1::restore_with_persistence_hook(
            config.clone(),
            plane.clone(),
            4,
            Arc::new(move |checkpoint| {
                persisted_for_hook.lock().unwrap().push(checkpoint);
                Ok(())
            }),
        )
        .unwrap();
        {
            let checkpoints = persisted.lock().unwrap();
            assert_eq!(checkpoints.len(), 1);
            assert_eq!(
                (checkpoints[0].expected_revision, checkpoints[0].revision),
                (4, 5)
            );
            assert_eq!(
                checkpoints[0].control_plane.children[0].attempts[0].state,
                ChildStateV1::OutcomeUnknown
            );
        }
        let snapshot = restored.snapshot().await;
        assert_eq!(snapshot.revision, 5);
        assert_eq!(snapshot.children[0].state, ChildStateV1::OutcomeUnknown);
        assert!(snapshot.mcp_leases.is_empty());

        let mut foreign = config;
        foreign.root_run_id = RootRunId::new("root-2").unwrap();
        assert!(
            RootDelegationControlV1::restore_with_persistence_hook(
                foreign,
                plane,
                4,
                Arc::new(|_| Ok(())),
            )
            .is_err()
        );
    }
}
