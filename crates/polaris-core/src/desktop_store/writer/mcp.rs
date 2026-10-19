//! Atomic MCP intent/raw publication using the existing desktop writer.
use super::*;
use crate::mcp::{
    operation::{McpOperation, SavedMcpIntent},
    results::{MAX_VISIBLE_BYTES, ProjectedPart, ReplyProjection},
};
use polaris_mcp::{
    OperationFailure,
    registration::{bounded_json, canonical_hash},
};
use serde::{Deserialize, Serialize};

pub const MCP_TOOL_OPERATION: &str = "mcp_tool";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpDisposition {
    Completed,
    BeforeSend,
    OutcomeUnknown,
    InputRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedMcpOperation {
    intent: SavedMcpIntent,
    terminal: Option<SavedMcpTerminal>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedMcpTerminal {
    disposition: McpDisposition,
    envelope_hash: String,
}

impl SavedMcpOperation {
    pub fn disposition(&self) -> Option<McpDisposition> {
        self.terminal.as_ref().map(|terminal| terminal.disposition)
    }
}

pub enum McpTerminalInput<'a> {
    Reply(&'a ReplyProjection),
    /// Only the transport's pre-send classification may become BeforeSend.
    /// Projection or publication failure after dispatch is never pre-send.
    Failure(OperationFailure),
}

/// Retain across commit/ACK failure. It cannot dispatch or expose a tool result.
pub struct McpTerminalPublication {
    root_identity: (u64, u64),
    project: ProjectId,
    session: SessionId,
    epoch: DecimalU64,
    intent: SavedMcpIntent,
    terminal: SavedMcpTerminal,
    result: ResultId,
    message: Message,
}

/// Returned only after durable publication or readback confirmation.
pub struct McpPublishedResult {
    pub message: Message,
    pub disposition: McpDisposition,
    successful: bool,
}

impl McpPublishedResult {
    /// A confirmed terminal may permit another request without having succeeded.
    /// Neither pre-send failures nor server tool errors satisfy workflow checks.
    pub fn succeeded(&self) -> bool {
        self.successful
    }

    pub fn permits_continuation(&self) -> bool {
        matches!(
            self.disposition,
            McpDisposition::Completed | McpDisposition::BeforeSend
        )
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    schema_version: u8,
    intent: SavedMcpIntent,
    disposition: McpDisposition,
    reply: Option<serde_json::Value>,
    failure: Option<String>,
}

/// Decode persisted local evidence without recreating an execution grant.
#[cfg(test)]
pub(in crate::desktop_store) fn inspect_local_terminal(
    messages: &[Message],
    result: &Message,
) -> StoreResult<McpDisposition> {
    if result.role != polaris_provider::Role::Tool || result.content.len() > MAX_VISIBLE_BYTES {
        return Err(StoreError::RunConflict);
    }
    let envelope: Envelope = serde_json::from_str(&result.content)?;
    envelope
        .intent
        .validate()
        .map_err(|_| StoreError::RunConflict)?;
    validate_envelope(&envelope)?;
    check_call(messages, &envelope.intent, true)?;
    if result.tool_call_id.as_deref() != Some(envelope.intent.model_call_id.as_str())
        || serde_json::to_string(&envelope)? != result.content
    {
        return Err(StoreError::RunConflict);
    }
    Ok(envelope.disposition)
}

/// Reserve the exact durable proof framing before projecting any reply.
/// InputRequired is the longest disposition that carries a reply. The null
/// placeholder is replaced by the serialized projection, not added to it.
pub(crate) fn mcp_reply_budget(operation: &McpOperation) -> StoreResult<usize> {
    let envelope = Envelope {
        schema_version: 1,
        intent: SavedMcpIntent::from_operation(operation).map_err(|_| StoreError::RunConflict)?,
        disposition: McpDisposition::InputRequired,
        reply: None,
        failure: None,
    };
    let overhead = serde_json::to_vec(&envelope)?
        .len()
        .checked_sub(4)
        .ok_or(StoreError::Overflow)?;
    MAX_VISIBLE_BYTES
        .checked_sub(overhead)
        .filter(|budget| *budget > 0)
        .ok_or(StoreError::Overflow)
}

/// The same canonical tool message for a host without the desktop store. The
/// session history is its only record; resume closes an unanswered call.
pub(crate) fn local_mcp_terminal(
    operation: &McpOperation,
    input: McpTerminalInput<'_>,
) -> StoreResult<McpPublishedResult> {
    let (disposition, reply, failure) = terminal_payload(input)?;
    let envelope = Envelope {
        schema_version: 1,
        intent: SavedMcpIntent::from_operation(operation).map_err(|_| StoreError::RunConflict)?,
        disposition,
        reply,
        failure,
    };
    validate_envelope(&envelope)?;
    let bytes = bounded_json(&envelope, MAX_VISIBLE_BYTES).map_err(|_| StoreError::Overflow)?;
    let successful = disposition == McpDisposition::Completed
        && envelope
            .reply
            .as_ref()
            .is_some_and(|reply| reply["status"] == "complete");
    Ok(McpPublishedResult {
        message: Message::tool_result(
            &envelope.intent.model_call_id,
            String::from_utf8(bytes).map_err(|_| StoreError::RunConflict)?,
        ),
        disposition,
        successful,
    })
}

pub(super) fn terminal_payload(
    input: McpTerminalInput<'_>,
) -> StoreResult<(McpDisposition, Option<serde_json::Value>, Option<String>)> {
    let payload = match input {
        McpTerminalInput::Reply(reply) => {
            if reply.parts.len() > MAX_VISIBLE_BYTES / 16 {
                return Err(StoreError::Overflow);
            }
            for part in &reply.parts {
                if let ProjectedPart::Structured { value }
                | ProjectedPart::InputRequired { request: value } = part
                {
                    canonical_hash(value).map_err(|_| StoreError::Overflow)?;
                }
            }
            let bytes = bounded_json(reply, MAX_VISIBLE_BYTES).map_err(|_| StoreError::Overflow)?;
            let disposition = if reply.status == "input_required" {
                McpDisposition::InputRequired
            } else if ["complete", "tool_error", "unsupported_content"].contains(&reply.status)
                && !reply
                    .parts
                    .iter()
                    .any(|part| matches!(part, ProjectedPart::InputRequired { .. }))
            {
                McpDisposition::Completed
            } else {
                return Err(StoreError::RunConflict);
            };
            (disposition, Some(serde_json::from_slice(&bytes)?), None)
        }
        McpTerminalInput::Failure(failure) => {
            let (disposition, error) = match failure {
                OperationFailure::BeforeSend(error) => (McpDisposition::BeforeSend, error),
                OperationFailure::OutcomeUnknown(error) => (McpDisposition::OutcomeUnknown, error),
            };
            (disposition, None, Some(error.to_string()))
        }
    };
    Ok(payload)
}

impl Writer {
    /// Reconcile an interrupted worker with already published MCP raw results.
    /// Call only after its execution owner is quiescent, before the final write.
    /// This reads evidence; it neither changes the outcome nor retries a call.
    pub fn reconcile_mcp_turn_suffix(
        &self,
        target: &RunTarget,
        worker: Vec<Message>,
        workflow: Option<&super::super::SavedWorkflow>,
    ) -> StoreResult<Vec<Message>> {
        self.ready()?;
        validate_turn_suffix(&worker, Observation::Cancelled)?;
        let index = self.run_index(target)?;
        let saved = saved_turn_suffix(&self.published, self.published.raw_run_index(index)?)?;
        let common = saved.len().min(worker.len());
        if !same_messages(&saved[..common], &worker[..common])? {
            return Err(StoreError::RunConflict);
        }
        if saved.len() <= worker.len() {
            return Ok(worker);
        }
        let record = &self.published.state.runs[index];
        // Do not relax exact terminal replay or unrelated workflow checkpoints.
        // Unconfirmed MCP publications must stay with the execution owner.
        if record.run.state.is_terminal()
            || record.workflow.as_ref() != workflow
            || !record.operations.iter().any(|op| {
                op.mcp.as_ref().is_some_and(|mcp| {
                    saved
                        .last()
                        .and_then(|message| message.tool_call_id.as_deref())
                        == Some(mcp.intent.model_call_id.as_str())
                        && mcp.terminal.is_some()
                })
            })
            || record.operations.iter().any(|op| {
                op.mcp
                    .as_ref()
                    .is_some_and(|mcp| mcp.terminal.is_none() || op.result_id.is_none())
            })
        {
            return Err(StoreError::RunConflict);
        }
        Ok(saved)
    }

    /// Only NewlyPublished is eligible for the owner's one-shot dispatch.
    /// Cancel, registry and credential identity are still checked by that owner.
    pub fn consume_mcp_approval_intent(
        &mut self,
        target: &RunTarget,
        approval: &ApprovalId,
        operation: &McpOperation,
        full_turn_suffix: Vec<Message>,
        now_ms: u64,
    ) -> StoreResult<IntentReceipt> {
        self.ready()?;
        let proof =
            SavedMcpIntent::from_operation(operation).map_err(|_| StoreError::RunConflict)?;
        if proof.resource.is_some() {
            return Err(StoreError::RunConflict);
        }
        if proof.run_id != target.run_id.as_str() || proof.attempt_id != target.attempt_id.as_str()
        {
            return Err(StoreError::RunConflict);
        }
        validate_turn_suffix(&full_turn_suffix, Observation::Cancelled)?;
        check_call(&full_turn_suffix, &proof, false)?;
        let index = self.run_index(target)?;
        let raw_index = self.published.raw_run_index(index)?;
        let tail = turn_suffix_tail(&self.published, raw_index, &full_turn_suffix)?;
        let operation_id = OperationId::new(&proof.operation_id)?;
        if self.published.state.runs[index]
            .operations
            .iter()
            .any(|op| {
                op.operation_id != operation_id
                    && op.mcp.as_ref().is_some_and(|saved| {
                        !matches!(
                            saved.disposition(),
                            Some(McpDisposition::Completed | McpDisposition::BeforeSend)
                        )
                    })
            })
        {
            return Err(StoreError::RunConflict);
        }
        let (mut next, receipt) = self.prepare_approval_intent(
            target,
            approval,
            &operation_id,
            MCP_TOOL_OPERATION,
            &proof.target,
            &proof.payload_hash,
            now_ms,
        )?;
        let record = &mut next.state.runs[index];
        let op = record
            .operations
            .iter_mut()
            .find(|op| op.operation_id == operation_id)
            .ok_or(StoreError::RunConflict)?;
        let evidence = SavedMcpOperation {
            intent: proof,
            terminal: None,
        };
        if receipt == IntentReceipt::AlreadyRecorded {
            if op.mcp.as_ref() != Some(&evidence) || !tail.is_empty() {
                return Err(StoreError::RunConflict);
            }
            return Ok(receipt);
        }
        op.mcp = Some(evidence);
        append_turn_suffix(&mut next.raw, raw_index, tail)?;
        self.commit(next, true)?;
        Ok(receipt)
    }

    /// Reconcile an uncertain intent ACK without granting or retrying dispatch.
    /// The caller must know it never issued a grant before recording BeforeSend
    /// from a positive result. An existing intent alone is not proof of no send.
    pub fn confirm_mcp_intent(
        &mut self,
        target: &RunTarget,
        approval: &ApprovalId,
        operation: &McpOperation,
        full_turn_suffix: &[Message],
    ) -> StoreResult<bool> {
        self.recover()?;
        let proof =
            SavedMcpIntent::from_operation(operation).map_err(|_| StoreError::RunConflict)?;
        if proof.resource.is_some() {
            return Err(StoreError::RunConflict);
        }
        if proof.run_id != target.run_id.as_str() || proof.attempt_id != target.attempt_id.as_str()
        {
            return Err(StoreError::RunConflict);
        }
        validate_turn_suffix(full_turn_suffix, Observation::Cancelled)?;
        check_call(full_turn_suffix, &proof, false)?;
        let index = self.run_index(target)?;
        let raw_index = self.published.raw_run_index(index)?;
        let tail = turn_suffix_tail(&self.published, raw_index, full_turn_suffix)?;
        let ledger = self
            .published
            .state
            .approval_records
            .iter()
            .find(|record| &record.pending.approval_id == approval)
            .ok_or(StoreError::NotFound)?;
        let pending = &ledger.pending;
        if pending.run_id != target.run_id
            || pending.attempt_id != target.attempt_id
            || pending.operation_id.as_str() != proof.operation_id
            || pending.operation != MCP_TOOL_OPERATION
            || pending.scope != proof.target
            || pending.payload_hash != proof.payload_hash
            || pending.policy_revision.get() != proof.policy_revision
            || pending.expires_at_unix_ms.get() != proof.expires_at_ms
            || ledger.decision != Some(ApprovalDecision::Allow)
        {
            return Err(StoreError::RunConflict);
        }
        let saved = self.published.state.runs[index]
            .operations
            .iter()
            .find(|saved| saved.operation_id.as_str() == proof.operation_id);
        let Some(saved) = saved else {
            return if ledger.consumed {
                Err(StoreError::RunConflict)
            } else {
                Ok(false)
            };
        };
        if !ledger.consumed
            || saved.result_id.is_some()
            || !tail.is_empty()
            || saved.mcp.as_ref()
                != Some(&SavedMcpOperation {
                    intent: proof,
                    terminal: None,
                })
        {
            return Err(StoreError::RunConflict);
        }
        Ok(true)
    }

    /// Resource capability was verified by the host's opaque BoundOperation.
    /// This path never creates or consumes a generic tool approval.
    pub(crate) fn record_mcp_read_intent(
        &mut self,
        target: &RunTarget,
        operation: &McpOperation,
        full_turn_suffix: Vec<Message>,
        now_ms: u64,
    ) -> StoreResult<IntentReceipt> {
        self.ready()?;
        let proof =
            SavedMcpIntent::from_operation(operation).map_err(|_| StoreError::RunConflict)?;
        let index = self.check_mcp_read_proof(target, &proof)?;
        let record = &self.published.state.runs[index];
        if !record.run.state.permits_start()
            || now_ms >= proof.expires_at_ms
            || proof.policy_revision != self.published.state.policy_revision.get()
            || record.policy_revision != self.published.state.policy_revision
            || record.operations.iter().any(|op| {
                op.operation_id.as_str() != proof.operation_id
                    && op.mcp.as_ref().is_some_and(|mcp| {
                        !matches!(
                            mcp.disposition(),
                            Some(McpDisposition::Completed | McpDisposition::BeforeSend)
                        )
                    })
            })
        {
            return Err(StoreError::RunConflict);
        }
        validate_turn_suffix(&full_turn_suffix, Observation::Cancelled)?;
        check_call(&full_turn_suffix, &proof, false)?;
        let raw_index = self.published.raw_run_index(index)?;
        let tail = turn_suffix_tail(&self.published, raw_index, &full_turn_suffix)?;
        let operation_id = OperationId::new(&proof.operation_id)?;
        let (mut next, receipt) = self.prepare_unapproved_intent(target, operation_id.clone())?;
        let op = next.state.runs[index]
            .operations
            .iter_mut()
            .find(|op| op.operation_id == operation_id)
            .ok_or(StoreError::RunConflict)?;
        let evidence = SavedMcpOperation {
            intent: proof,
            terminal: None,
        };
        if receipt == IntentReceipt::AlreadyRecorded {
            if op.mcp.as_ref() != Some(&evidence) || op.result_id.is_some() || !tail.is_empty() {
                return Err(StoreError::RunConflict);
            }
            return Ok(receipt);
        }
        op.mcp = Some(evidence);
        append_turn_suffix(&mut next.raw, raw_index, tail)?;
        self.commit(next, true)?;
        Ok(receipt)
    }

    pub(crate) fn confirm_mcp_read_intent(
        &mut self,
        target: &RunTarget,
        operation: &McpOperation,
        full_turn_suffix: &[Message],
    ) -> StoreResult<bool> {
        self.recover()?;
        let proof =
            SavedMcpIntent::from_operation(operation).map_err(|_| StoreError::RunConflict)?;
        let index = self.check_mcp_read_proof(target, &proof)?;
        validate_turn_suffix(full_turn_suffix, Observation::Cancelled)?;
        check_call(full_turn_suffix, &proof, false)?;
        let tail = turn_suffix_tail(
            &self.published,
            self.published.raw_run_index(index)?,
            full_turn_suffix,
        )?;
        let Some(op) = self.published.state.runs[index]
            .operations
            .iter()
            .find(|op| op.operation_id.as_str() == proof.operation_id)
        else {
            return Ok(false);
        };
        if op.result_id.is_some()
            || !tail.is_empty()
            || op.mcp.as_ref()
                != Some(&SavedMcpOperation {
                    intent: proof,
                    terminal: None,
                })
        {
            return Err(StoreError::RunConflict);
        }
        Ok(true)
    }

    fn check_mcp_read_proof(
        &self,
        target: &RunTarget,
        proof: &SavedMcpIntent,
    ) -> StoreResult<usize> {
        if proof.resource.is_none()
            || proof.run_id != target.run_id.as_str()
            || proof.attempt_id != target.attempt_id.as_str()
            || proof.target != self.published.marker.project_id.as_str()
            || self
                .published
                .state
                .approval_records
                .iter()
                .any(|record| record.pending.operation_id.as_str() == proof.operation_id)
        {
            return Err(StoreError::RunConflict);
        }
        self.run_index(target)
    }

    /// Build a bounded result from the stored intent. No disk mutation or RPC.
    pub fn prepare_mcp_terminal(
        &self,
        target: &RunTarget,
        operation: &OperationId,
        input: McpTerminalInput<'_>,
    ) -> StoreResult<McpTerminalPublication> {
        self.ready()?;
        let index = self.run_index(target)?;
        let saved = self.published.state.runs[index]
            .operations
            .iter()
            .find(|op| &op.operation_id == operation)
            .and_then(|op| op.mcp.as_ref())
            .ok_or(StoreError::NotFound)?;
        let (disposition, reply, failure) = terminal_payload(input)?;
        let envelope = Envelope {
            schema_version: 1,
            intent: saved.intent.clone(),
            disposition,
            reply,
            failure,
        };
        validate_envelope(&envelope)?;
        let bytes = bounded_json(&envelope, MAX_VISIBLE_BYTES).map_err(|_| StoreError::Overflow)?;
        let envelope_hash = content_hash(&bytes);
        Ok(McpTerminalPublication {
            root_identity: self.root_identity()?,
            project: self.published.marker.project_id.clone(),
            session: self.published.marker.session_id.clone(),
            epoch: self.published.marker.epoch,
            intent: saved.intent.clone(),
            result: result_id(&envelope_hash)?,
            terminal: SavedMcpTerminal {
                disposition,
                envelope_hash,
            },
            message: Message::tool_result(
                &saved.intent.model_call_id,
                String::from_utf8(bytes).map_err(|_| StoreError::RunConflict)?,
            ),
        })
    }

    /// Commit terminal classification and the matching raw result atomically.
    /// A failure must stop continuation; retain publication for confirm below.
    pub fn publish_mcp_terminal(
        &mut self,
        publication: &McpTerminalPublication,
    ) -> StoreResult<McpPublishedResult> {
        self.ready()?;
        let (index, op_index) = self.mcp_publication_index(publication)?;
        let op = &self.published.state.runs[index].operations[op_index];
        if op.mcp.as_ref().unwrap().terminal.is_some() {
            return self.confirm_mcp_saved(publication);
        }
        if self.published.state.runs[index].run.state.is_terminal() {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.published.clone();
        let raw_index = next.raw_run_index(index)?;
        let mut suffix = saved_turn_suffix(&next, raw_index)?;
        suffix.push(publication.message.clone());
        validate_turn_suffix(&suffix, Observation::Cancelled)?;
        append_turn_suffix(&mut next.raw, raw_index, vec![publication.message.clone()])?;
        let op = &mut next.state.runs[index].operations[op_index];
        op.result_id = Some(publication.result.clone());
        op.mcp.as_mut().unwrap().terminal = Some(publication.terminal.clone());
        self.commit(next, true)?;
        self.confirm_mcp_saved(publication)
    }

    /// Reconcile an uncertain ACK with the actual marker; never publish/replay.
    pub fn confirm_mcp_terminal(
        &mut self,
        publication: &McpTerminalPublication,
    ) -> StoreResult<McpPublishedResult> {
        self.recover()?;
        self.confirm_mcp_saved(publication)
    }

    fn mcp_publication_index(
        &self,
        publication: &McpTerminalPublication,
    ) -> StoreResult<(usize, usize)> {
        if self.root_identity()? != publication.root_identity
            || self.published.marker.project_id != publication.project
            || self.published.marker.session_id != publication.session
            || self.published.marker.epoch != publication.epoch
        {
            return Err(StoreError::TargetMismatch);
        }
        let index = self.run_index(&RunTarget {
            run_id: RunId::new(&publication.intent.run_id)?,
            attempt_id: AttemptId::new(&publication.intent.attempt_id)?,
        })?;
        let op_index = self.published.state.runs[index]
            .operations
            .iter()
            .position(|op| op.operation_id.as_str() == publication.intent.operation_id)
            .ok_or(StoreError::NotFound)?;
        if self.published.state.runs[index].operations[op_index]
            .mcp
            .as_ref()
            .map(|mcp| &mcp.intent)
            != Some(&publication.intent)
        {
            return Err(StoreError::RunConflict);
        }
        Ok((index, op_index))
    }

    fn confirm_mcp_saved(
        &self,
        publication: &McpTerminalPublication,
    ) -> StoreResult<McpPublishedResult> {
        let (index, op_index) = self.mcp_publication_index(publication)?;
        let op = &self.published.state.runs[index].operations[op_index];
        let suffix = saved_turn_suffix(&self.published, self.published.raw_run_index(index)?)?;
        let messages: Vec<_> = suffix
            .iter()
            .filter(|message| {
                message.tool_call_id.as_deref() == Some(&publication.intent.model_call_id)
            })
            .collect();
        if op.result_id.as_ref() != Some(&publication.result)
            || op.mcp.as_ref().unwrap().terminal.as_ref() != Some(&publication.terminal)
            || messages.len() != 1
            || !same_message(messages[0], &publication.message)?
        {
            return Err(StoreError::RunConflict);
        }
        let envelope: Envelope = serde_json::from_str(&publication.message.content)?;
        let successful = envelope.disposition == McpDisposition::Completed
            && envelope
                .reply
                .as_ref()
                .is_some_and(|reply| reply["status"] == "complete");
        Ok(McpPublishedResult {
            message: publication.message.clone(),
            disposition: publication.terminal.disposition,
            successful,
        })
    }
}

pub(in crate::desktop_store) fn unresolved(op: &Operation) -> bool {
    op.result_id.is_none()
        || op
            .prompt
            .as_ref()
            .and_then(super::prompt::SavedMcpPrompt::disposition)
            == Some(McpDisposition::OutcomeUnknown)
        || op.mcp.as_ref().and_then(SavedMcpOperation::disposition)
            == Some(McpDisposition::OutcomeUnknown)
}

pub(in crate::desktop_store) fn validate_transition(
    old: &Sidecar,
    next: &Sidecar,
) -> StoreResult<()> {
    for run in &old.runs {
        for op in &run.operations {
            let current = next
                .runs
                .iter()
                .find(|candidate| candidate.run.run_id == run.run.run_id)
                .and_then(|run| {
                    run.operations
                        .iter()
                        .find(|candidate| candidate.operation_id == op.operation_id)
                });
            match (&op.mcp, current.and_then(|op| op.mcp.as_ref())) {
                (None, None) => {}
                (Some(old), Some(new))
                    if old.intent == new.intent
                        && (old.terminal.is_none() || old.terminal == new.terminal)
                        && (op.result_id.is_none()
                            || current.unwrap().result_id == op.result_id) => {}
                _ => return Err(StoreError::Corrupt("MCP操作証拠の変更・削除")),
            }
        }
    }
    Ok(())
}

pub(in crate::desktop_store) fn validate_state(state: &Sidecar) -> StoreResult<()> {
    for record in &state.runs {
        for op in &record.operations {
            let Some(saved) = &op.mcp else {
                continue;
            };
            let proof = &saved.intent;
            proof
                .validate()
                .map_err(|_| StoreError::Corrupt("MCP操作証拠"))?;
            if proof.run_id != record.run.run_id.as_str()
                || proof.attempt_id != record.run.attempt_id.as_str()
                || proof.operation_id != op.operation_id.as_str()
                || proof.policy_revision != record.policy_revision.get()
                || saved.terminal.is_some() != op.result_id.is_some()
            {
                return Err(StoreError::Corrupt("MCP操作の対応"));
            }
            let approval = state
                .approval_records
                .iter()
                .find(|a| a.pending.operation_id == op.operation_id);
            if proof.resource.is_some() {
                if approval.is_some() {
                    return Err(StoreError::Corrupt("MCP読取りとtool承認の混同"));
                }
            } else {
                let approval = approval.ok_or(StoreError::Corrupt("MCP承認証拠"))?;
                let a = &approval.pending;
                if a.run_id != record.run.run_id
                    || a.attempt_id != record.run.attempt_id
                    || !approval.consumed
                    || approval.decision != Some(ApprovalDecision::Allow)
                    || a.payload_hash != proof.payload_hash
                    || a.scope != proof.target
                    || a.operation != MCP_TOOL_OPERATION
                    || a.policy_revision.get() != proof.policy_revision
                    || a.expires_at_unix_ms.get() != proof.expires_at_ms
                {
                    return Err(StoreError::Corrupt("MCP承認・操作の対応"));
                }
            }
            if let Some(terminal) = &saved.terminal {
                if op.result_id.as_ref() != Some(&result_id(&terminal.envelope_hash)?) {
                    return Err(StoreError::Corrupt("MCP結果IDの対応"));
                }
                if (terminal.disposition == McpDisposition::OutcomeUnknown
                    && record.run.state.is_terminal()
                    && record.run.state != RunState::OutcomeUnknown)
                    || (terminal.disposition == McpDisposition::InputRequired
                        && record.run.state == RunState::Succeeded)
                {
                    return Err(StoreError::RunConflict);
                }
            }
        }
    }
    Ok(())
}

pub(super) fn validate(p: &Published) -> StoreResult<()> {
    validate_state(&p.state)?;
    for (index, record) in p.state.runs.iter().enumerate() {
        if !record.operations.iter().any(|op| op.mcp.is_some()) {
            continue;
        }
        let suffix = saved_turn_suffix(p, p.raw_run_index(index)?)?;
        validate_turn_suffix(&suffix, Observation::Cancelled)?;
        for op in &record.operations {
            let Some(saved) = &op.mcp else {
                continue;
            };
            let proof = &saved.intent;
            if proof.resource.is_some() && proof.target != p.marker.project_id.as_str() {
                return Err(StoreError::Corrupt("MCP読取りprojectの対応"));
            }
            check_call(&suffix, proof, saved.terminal.is_some())?;
            if let Some(terminal) = &saved.terminal {
                let message = suffix
                    .iter()
                    .find(|message| message.tool_call_id.as_deref() == Some(&proof.model_call_id))
                    .ok_or(StoreError::RunConflict)?;
                if message.content.len() > MAX_VISIBLE_BYTES
                    || content_hash(message.content.as_bytes()) != terminal.envelope_hash
                {
                    return Err(StoreError::Corrupt("MCP結果hash"));
                }
                let envelope: Envelope = serde_json::from_str(&message.content)?;
                if envelope.intent != *proof
                    || envelope.disposition != terminal.disposition
                    || serde_json::to_string(&envelope)? != message.content
                {
                    return Err(StoreError::Corrupt("MCP結果の出典・形式"));
                }
                validate_envelope(&envelope)?;
            }
        }
    }
    Ok(())
}

fn check_call(messages: &[Message], proof: &SavedMcpIntent, has_result: bool) -> StoreResult<()> {
    let calls: Vec<_> = messages
        .iter()
        .flat_map(|message| &message.tool_calls)
        .filter(|call| call.id == proof.model_call_id)
        .collect();
    let result_count = messages
        .iter()
        .filter(|message| message.tool_call_id.as_deref() == Some(&proof.model_call_id))
        .count();
    if calls.len() != 1
        || result_count != usize::from(has_result)
        || calls[0].name != proof.alias().map_err(|_| StoreError::RunConflict)?
        || !calls[0].arguments.is_object()
        || canonical_hash(&calls[0].arguments).map_err(|_| StoreError::Overflow)?
            != proof
                .resource
                .as_ref()
                .map_or(&proof.arguments_hash, |read| &read.meta_arguments_hash)
                .as_str()
    {
        return Err(StoreError::RunConflict);
    }
    if proof.resource.is_some() {
        crate::mcp::operation::read::ResourceRead::parse(calls[0])
            .and_then(|read| read.check(&proof.binding()))
            .map_err(|_| StoreError::RunConflict)?;
    }
    Ok(())
}

fn result_id(hash: &str) -> StoreResult<ResultId> {
    if !valid_hash(hash) {
        return Err(StoreError::RunConflict);
    }
    Ok(ResultId::new(format!("mcp-result-{hash}"))?)
}

fn validate_envelope(envelope: &Envelope) -> StoreResult<()> {
    if envelope.schema_version != 1 {
        return Err(StoreError::RunConflict);
    }
    validate_terminal_payload(
        envelope.disposition,
        envelope.reply.as_ref(),
        envelope.failure.as_deref(),
    )?;
    if let Some(reply) = &envelope.reply {
        let source = &reply["source"];
        if source["server"].as_str() != Some(&envelope.intent.server_id)
            || source["model_call"].as_str() != Some(&envelope.intent.model_call_id)
            || source.get("selection_id").is_some()
            || !valid_result_generation(source)
        {
            return Err(StoreError::RunConflict);
        }
    }
    Ok(())
}

pub(super) fn valid_result_generation(source: &serde_json::Value) -> bool {
    source["result_generation"]
        .as_str()
        .is_some_and(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
}

pub(super) fn validate_terminal_payload(
    disposition: McpDisposition,
    reply: Option<&serde_json::Value>,
    failure: Option<&str>,
) -> StoreResult<()> {
    match disposition {
        McpDisposition::Completed | McpDisposition::InputRequired => {
            let reply = reply.ok_or(StoreError::RunConflict)?;
            let status = reply["status"].as_str().ok_or(StoreError::RunConflict)?;
            if failure.is_some()
                || (disposition == McpDisposition::InputRequired && status != "input_required")
                || (disposition == McpDisposition::Completed
                    && !["complete", "tool_error", "unsupported_content"].contains(&status))
            {
                return Err(StoreError::RunConflict);
            }
        }
        McpDisposition::BeforeSend | McpDisposition::OutcomeUnknown => {
            if reply.is_some()
                || !failure.is_some_and(|failure| {
                    [
                        "mcp_invalid_registration",
                        "mcp_limit",
                        "mcp_storage",
                        "mcp_protection",
                        "mcp_revision_conflict",
                        "mcp_recovery_required",
                        "mcp_auth",
                        "mcp_transport",
                        "mcp_protocol",
                        "mcp_cancelled",
                        "mcp_timeout",
                        "mcp_stop_unconfirmed",
                    ]
                    .contains(&failure)
                })
            {
                return Err(StoreError::RunConflict);
            }
        }
    }
    Ok(())
}
