//! One host-selected prompt per accepted input; saved proof never grants a retry.
use super::*;
use crate::mcp::results::{MAX_VISIBLE_BYTES, RunBinding};
use mcp::{McpDisposition, McpTerminalInput};
use polaris_desktop_protocol::request::McpPromptChoice;
use polaris_mcp::{
    OperationBinding,
    catalog::{ItemId, Kind},
    registration::{bounded_json, canonical_hash},
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct InputProof {
    epoch: u64,
    sequence: u64,
    turn: u64,
    hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Proof {
    run: RunId,
    attempt: AttemptId,
    operation: OperationId,
    project: ProjectId,
    policy_revision: DecimalU64,
    expires_at_ms: u64,
    input: InputProof,
    choice_hash: String,
    server: String,
    prompt: String,
    registry_revision: u64,
    destination_hash: String,
    connection_generation: String,
    catalog_generation: String,
    arguments_hash: String,
    selection_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Terminal {
    disposition: McpDisposition,
    envelope_hash: String,
}

/// Provenance only. Deserializing it cannot recreate a BoundOperation or grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SavedMcpPrompt {
    intent: Proof,
    terminal: Option<Terminal>,
}
impl SavedMcpPrompt {
    pub fn disposition(&self) -> Option<McpDisposition> {
        self.terminal.as_ref().map(|value| value.disposition)
    }

    pub(in crate::desktop_store) fn validate_input(
        &self,
        marker: &Marker,
        input_text: &str,
        input: &RawEventV2,
    ) -> StoreResult<()> {
        check_input_event(marker, input_text, input, &self.intent)
    }

    /// Call after validating the run receipt and its fixed owner publication.
    /// This only proves raw provenance; it cannot issue a transport grant.
    pub(in crate::desktop_store) fn validate_pair(
        &self,
        marker: &Marker,
        input_text: &str,
        input: &RawEventV2,
        event: &RawEventV2,
    ) -> StoreResult<()> {
        self.validate_input(marker, input_text, input)?;
        let terminal = self.terminal.as_ref().ok_or(StoreError::RunConflict)?;
        if event.sequence
            != self
                .intent
                .input
                .sequence
                .checked_add(1)
                .ok_or(StoreError::Overflow)?
            || event.epoch != self.intent.input.epoch
            || event.turn_id != self.intent.input.turn
            || event.starts_turn
            || !same_message(&event.message, &Message::user(&event.message.content))?
            || event.message.content.len() > MAX_VISIBLE_BYTES
            || content_hash(event.message.content.as_bytes()) != terminal.envelope_hash
        {
            return Err(StoreError::Corrupt(
                "MCP prompt supplemental source association",
            ));
        }
        let envelope: Envelope = serde_json::from_str(&event.message.content)?;
        if envelope.intent != self.intent
            || envelope.disposition != terminal.disposition
            || serde_json::to_string(&envelope)? != event.message.content
        {
            return Err(StoreError::Corrupt("MCP prompt result source or format"));
        }
        validate_envelope(&envelope)
    }
}

/// Host-created publication proposal. Retain it for confirm-only ACK recovery.
pub struct McpPromptIntent {
    root_identity: (u64, u64),
    session: SessionId,
    proof: Proof,
}
impl McpPromptIntent {
    /// Read-only identity for the transport's final check, never a send grant.
    pub fn binding(&self) -> OperationBinding {
        self.proof.binding()
    }
    pub fn reply_budget(&self) -> StoreResult<usize> {
        let envelope = Envelope {
            schema_version: 1,
            intent: self.proof.clone(),
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
            .filter(|n| *n > 0)
            .ok_or(StoreError::Overflow)
    }
}

pub struct McpPromptPublication {
    root_identity: (u64, u64),
    session: SessionId,
    proof: Proof,
    terminal: Terminal,
    result: ResultId,
    message: Message,
}

/// A canonical supplemental User event, not a new user input or tool result.
pub struct McpPromptResult {
    pub message: Message,
    pub disposition: McpDisposition,
}
impl McpPromptResult {
    pub fn permits_continuation(&self) -> bool {
        self.disposition == McpDisposition::Completed
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    schema_version: u8,
    intent: Proof,
    disposition: McpDisposition,
    reply: Option<Value>,
    failure: Option<String>,
}

// One owned wire shape is shared by the serializer and migration decoder.
// This evidence cannot reconstruct a BoundOperation or grant a retry.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalPromptBinding {
    item: ItemId,
    registry_revision: u64,
    destination_hash: String,
    connection_generation: String,
    catalog_generation: String,
    schema_hash: Option<String>,
    arguments_hash: String,
    selection_id: Option<String>,
    resource_uri: Option<String>,
}
impl From<&OperationBinding> for LocalPromptBinding {
    fn from(value: &OperationBinding) -> Self {
        // Exhaustive destructuring keeps this wire shape coupled to its source.
        let OperationBinding {
            item,
            registry_revision,
            destination_hash,
            connection_generation,
            catalog_generation,
            schema_hash,
            arguments_hash,
            selection_id,
            resource_uri,
        } = value;
        Self {
            item: item.clone(),
            registry_revision: *registry_revision,
            destination_hash: destination_hash.clone(),
            connection_generation: connection_generation.clone(),
            catalog_generation: catalog_generation.clone(),
            schema_hash: schema_hash.clone(),
            arguments_hash: arguments_hash.clone(),
            selection_id: selection_id.clone(),
            resource_uri: resource_uri.clone(),
        }
    }
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalPromptIntent {
    session: String,
    run: String,
    attempt: String,
    project: String,
    binding: LocalPromptBinding,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct LocalPromptEnvelope {
    schema_version: u8,
    intent: LocalPromptIntent,
    disposition: McpDisposition,
    reply: Option<Value>,
    failure: Option<String>,
}
impl LocalPromptIntent {
    fn validate(&self) -> StoreResult<()> {
        let binding = &self.binding;
        let choice = McpPromptChoice {
            server: binding.item.server_id.clone(),
            prompt: binding.item.key.clone(),
            arguments: serde_json::Map::new(),
        };
        validate_choice(&choice)?;
        if binding.item.kind != Kind::Prompt
            || binding.schema_hash.is_some()
            || binding.resource_uri.is_some()
            || binding.connection_generation.len() != 32
            || !ascii_id(&binding.connection_generation, 32)
            || !binding
                .selection_id
                .as_ref()
                .is_some_and(|id| id.len() == 32 && ascii_id(id, 32))
            || [
                &binding.destination_hash,
                &binding.catalog_generation,
                &binding.arguments_hash,
            ]
            .into_iter()
            .any(|hash| !valid_hash(hash))
            || [&self.session, &self.run, &self.attempt, &self.project]
                .into_iter()
                .any(|id| id.is_empty() || id.len() > 256 || id.chars().any(char::is_control))
        {
            return Err(StoreError::RunConflict);
        }
        Ok(())
    }
}
fn local_prompt_intent(
    run: &RunBinding,
    project: &str,
    binding: &OperationBinding,
) -> StoreResult<LocalPromptIntent> {
    let value = LocalPromptIntent {
        session: run.session.clone(),
        run: run.run.clone(),
        attempt: run.attempt.clone(),
        project: project.into(),
        binding: binding.into(),
    };
    value.validate()?;
    Ok(value)
}

#[cfg(test)]
pub(in crate::desktop_store) fn inspect_local_terminal(
    message: &Message,
    project: &str,
    session: &str,
) -> StoreResult<Option<McpDisposition>> {
    if message.role != polaris_provider::Role::User {
        return Err(StoreError::RunConflict);
    }
    // Other host supplements have no MCP framing. A reserved framing object
    // must decode strictly; malformed evidence is never treated as absent.
    let framing: Value = match serde_json::from_str(&message.content) {
        Ok(value) => value,
        Err(_)
            if message
                .content
                .trim_start()
                .starts_with("{\"schema_version\":") =>
        {
            return Err(StoreError::RunConflict);
        }
        Err(_) => return Ok(None),
    };
    let reserved = framing
        .get("intent")
        .and_then(|intent| intent.get("binding"))
        .is_some()
        || (framing.get("schema_version").is_some()
            && (framing.get("intent").is_some() || framing.get("disposition").is_some()));
    if !reserved {
        return Ok(None);
    }
    if message.content.len() > MAX_VISIBLE_BYTES {
        return Err(StoreError::Overflow);
    }
    let envelope: LocalPromptEnvelope = serde_json::from_str(&message.content)?;
    envelope.intent.validate()?;
    mcp::validate_terminal_payload(
        envelope.disposition,
        envelope.reply.as_ref(),
        envelope.failure.as_deref(),
    )?;
    if envelope.schema_version != 1
        || envelope.intent.project != project
        || envelope.intent.session != session
        || serde_json::to_string(&envelope)? != message.content
    {
        return Err(StoreError::RunConflict);
    }
    if let Some(reply) = &envelope.reply {
        let source = &reply["source"];
        if source["server"].as_str() != Some(envelope.intent.binding.item.server_id.as_str())
            || source["selection_id"].as_str() != envelope.intent.binding.selection_id.as_deref()
            || source.get("model_call").is_some()
            || !mcp::valid_result_generation(source)
        {
            return Err(StoreError::RunConflict);
        }
    }
    Ok(Some(envelope.disposition))
}

/// Local CLI/TUI runs have no desktop writer. Reserve room for the exact
/// provenance envelope before accepting a projected server reply.
pub(crate) fn local_mcp_prompt_reply_budget(
    run: &RunBinding,
    project: &str,
    binding: &OperationBinding,
) -> StoreResult<usize> {
    let envelope = LocalPromptEnvelope {
        schema_version: 1,
        intent: local_prompt_intent(run, project, binding)?,
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

/// The caller persists this supplemental user-role event after the accepted
/// input and before the first provider request. It never starts a new turn.
pub(crate) fn local_mcp_prompt_terminal(
    run: &RunBinding,
    project: &str,
    binding: &OperationBinding,
    input: McpTerminalInput<'_>,
) -> StoreResult<McpPromptResult> {
    let (disposition, reply, failure) = mcp::terminal_payload(input)?;
    let envelope = LocalPromptEnvelope {
        schema_version: 1,
        intent: local_prompt_intent(run, project, binding)?,
        disposition,
        reply,
        failure,
    };
    let bytes = bounded_json(&envelope, MAX_VISIBLE_BYTES).map_err(|_| StoreError::Overflow)?;
    Ok(McpPromptResult {
        message: Message::user(String::from_utf8(bytes).map_err(|_| StoreError::RunConflict)?),
        disposition,
    })
}

pub(super) fn validate_choice(choice: &McpPromptChoice) -> StoreResult<()> {
    if !ascii_id(&choice.server, 64)
        || choice.prompt.is_empty()
        || choice.prompt.len() > 16 * 1024
        || choice.prompt.chars().any(char::is_control)
    {
        return Err(StoreError::RunConflict);
    }
    choice_hash(choice).map(|_| ())
}

fn choice_hash(choice: &McpPromptChoice) -> StoreResult<String> {
    canonical_hash(
        &json!({"server":choice.server,"prompt":choice.prompt,"arguments":choice.arguments}),
    )
    .map_err(|_| StoreError::Overflow)
}

fn ascii_id(text: &str, limit: usize) -> bool {
    !text.is_empty()
        && text.len() <= limit
        && text
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
}

impl Proof {
    fn binding(&self) -> OperationBinding {
        OperationBinding {
            item: ItemId {
                server_id: self.server.clone(),
                kind: Kind::Prompt,
                key: self.prompt.clone(),
            },
            registry_revision: self.registry_revision,
            destination_hash: self.destination_hash.clone(),
            connection_generation: self.connection_generation.clone(),
            catalog_generation: self.catalog_generation.clone(),
            schema_hash: None,
            arguments_hash: self.arguments_hash.clone(),
            selection_id: Some(self.selection_id.clone()),
            resource_uri: None,
        }
    }

    fn validate(&self, record: &RunRecord, operation: &OperationId) -> StoreResult<()> {
        let choice = record.mcp_prompt.as_ref().ok_or(StoreError::RunConflict)?;
        validate_choice(choice)?;
        if self.run != record.run.run_id
            || self.attempt != record.run.attempt_id
            || &self.operation != operation
            || self.policy_revision != record.policy_revision
            || self.server != choice.server
            || self.prompt != choice.prompt
            || self.choice_hash != choice_hash(choice)?
            || self.arguments_hash
                != canonical_hash(&json!(choice.arguments)).map_err(|_| StoreError::Overflow)?
            || self.connection_generation.len() != 32
            || !ascii_id(&self.connection_generation, 32)
            || self.selection_id.len() != 32
            || !ascii_id(&self.selection_id, 32)
            || self.expires_at_ms == 0
            || self.input.epoch == 0
            || self.input.sequence == 0
            || self.input.turn == 0
            || [
                &self.destination_hash,
                &self.catalog_generation,
                &self.input.hash,
            ]
            .into_iter()
            .any(|h| !valid_hash(h))
        {
            return Err(StoreError::Corrupt(
                "MCP prompt selection and operation evidence association",
            ));
        }
        Ok(())
    }
}

fn input(p: &Published, index: usize) -> StoreResult<&RawEventV2> {
    p.raw
        .iter()
        .filter(|event| event.starts_turn)
        .nth(p.raw_run_index(index)?)
        .ok_or(StoreError::RunConflict)
}

fn check_input(p: &Published, index: usize, proof: &Proof) -> StoreResult<()> {
    let event = input(p, index)?;
    check_input_event(&p.marker, &p.state.runs[index].input.text, event, proof)
}

fn check_input_event(
    marker: &Marker,
    input_text: &str,
    event: &RawEventV2,
    proof: &Proof,
) -> StoreResult<()> {
    if proof.project != marker.project_id
        || proof.input.epoch != marker.epoch.get()
        || proof.input.epoch != event.epoch
        || proof.input.sequence != event.sequence
        || proof.input.turn != event.turn_id
        || !event.starts_turn
        || proof.input.hash != content_hash(&serde_json::to_vec(event)?)
        || !same_message(&event.message, &Message::user(input_text))?
    {
        return Err(StoreError::Corrupt("MCP prompt user input evidence"));
    }
    Ok(())
}

fn result_id(hash: &str) -> StoreResult<ResultId> {
    if !valid_hash(hash) {
        return Err(StoreError::RunConflict);
    }
    Ok(ResultId::new(format!("mcp-prompt-{hash}"))?)
}

fn validate_envelope(envelope: &Envelope) -> StoreResult<()> {
    if envelope.schema_version != 1 {
        return Err(StoreError::RunConflict);
    }
    mcp::validate_terminal_payload(
        envelope.disposition,
        envelope.reply.as_ref(),
        envelope.failure.as_deref(),
    )?;
    if let Some(reply) = &envelope.reply {
        let source = &reply["source"];
        if source["server"].as_str() != Some(&envelope.intent.server)
            || source["selection_id"].as_str() != Some(&envelope.intent.selection_id)
            || source.get("model_call").is_some()
            || !mcp::valid_result_generation(source)
        {
            return Err(StoreError::RunConflict);
        }
    }
    Ok(())
}

impl Writer {
    /// Bind a prepared host selection to the immutable accepted input. No I/O to MCP.
    pub fn prepare_mcp_prompt_intent(
        &self,
        target: &RunTarget,
        operation: OperationId,
        binding: &OperationBinding,
        expires_at_ms: u64,
    ) -> StoreResult<McpPromptIntent> {
        self.ready()?;
        let index = self.run_index(target)?;
        let record = &self.published.state.runs[index];
        let choice = record.mcp_prompt.as_ref().ok_or(StoreError::RunConflict)?;
        if binding.item.kind != Kind::Prompt
            || binding.schema_hash.is_some()
            || binding.resource_uri.is_some()
        {
            return Err(StoreError::RunConflict);
        }
        let event = input(&self.published, index)?;
        let proof = Proof {
            run: target.run_id.clone(),
            attempt: target.attempt_id.clone(),
            operation,
            project: self.published.marker.project_id.clone(),
            policy_revision: record.policy_revision,
            expires_at_ms,
            input: InputProof {
                epoch: event.epoch,
                sequence: event.sequence,
                turn: event.turn_id,
                hash: content_hash(&serde_json::to_vec(event)?),
            },
            choice_hash: choice_hash(choice)?,
            server: binding.item.server_id.clone(),
            prompt: binding.item.key.clone(),
            registry_revision: binding.registry_revision,
            destination_hash: binding.destination_hash.clone(),
            connection_generation: binding.connection_generation.clone(),
            catalog_generation: binding.catalog_generation.clone(),
            arguments_hash: binding.arguments_hash.clone(),
            selection_id: binding
                .selection_id
                .clone()
                .ok_or(StoreError::RunConflict)?,
        };
        proof.validate(record, &proof.operation)?;
        check_input(&self.published, index, &proof)?;
        let intent = McpPromptIntent {
            root_identity: self.root_identity()?,
            session: self.published.marker.session_id.clone(),
            proof,
        };
        intent.reply_budget()?;
        Ok(intent)
    }

    /// Only NewlyPublished is eligible for a fresh owner's one-shot transport grant.
    pub fn begin_mcp_prompt(
        &mut self,
        intent: &McpPromptIntent,
        now_ms: u64,
    ) -> StoreResult<IntentReceipt> {
        self.ready()?;
        let index = self.prompt_index(intent.root_identity, &intent.session, &intent.proof)?;
        let record = &self.published.state.runs[index];
        if let Some(existing) = record
            .operations
            .iter()
            .find(|op| op.operation_id == intent.proof.operation)
        {
            return if existing
                .prompt
                .as_ref()
                .is_some_and(|p| p.intent == intent.proof)
            {
                Ok(IntentReceipt::AlreadyRecorded)
            } else {
                Err(StoreError::RunConflict)
            };
        }
        if record.operations.iter().any(|op| op.prompt.is_some())
            || now_ms >= intent.proof.expires_at_ms
            || self.published.state.policy_revision != intent.proof.policy_revision
            || self.published.raw.last().map(|event| event.sequence)
                != Some(intent.proof.input.sequence)
        {
            return Err(StoreError::RunConflict);
        }
        let target = RunTarget {
            run_id: intent.proof.run.clone(),
            attempt_id: intent.proof.attempt.clone(),
        };
        let (mut next, receipt) =
            self.prepare_unapproved_intent(&target, intent.proof.operation.clone())?;
        if receipt != IntentReceipt::NewlyPublished {
            return Err(StoreError::RunConflict);
        }
        next.state.runs[index]
            .operations
            .last_mut()
            .ok_or(StoreError::RunConflict)?
            .prompt = Some(SavedMcpPrompt {
            intent: intent.proof.clone(),
            terminal: None,
        });
        self.commit(next, false)?;
        Ok(receipt)
    }

    /// Read back the exact publication after an uncertain ACK. Never grant or resend.
    pub fn confirm_mcp_prompt_intent(&mut self, intent: &McpPromptIntent) -> StoreResult<bool> {
        self.recover()?;
        let index = self.prompt_index(intent.root_identity, &intent.session, &intent.proof)?;
        match self.published.state.runs[index]
            .operations
            .iter()
            .find(|op| op.operation_id == intent.proof.operation)
        {
            None => Ok(false),
            Some(op) if op.prompt.as_ref().is_some_and(|p| p.intent == intent.proof) => Ok(true),
            _ => Err(StoreError::RunConflict),
        }
    }

    pub fn prepare_mcp_prompt_terminal(
        &self,
        intent: &McpPromptIntent,
        input: McpTerminalInput<'_>,
    ) -> StoreResult<McpPromptPublication> {
        self.ready()?;
        let index = self.prompt_index(intent.root_identity, &intent.session, &intent.proof)?;
        if !self.published.state.runs[index]
            .operations
            .iter()
            .any(|op| op.prompt.as_ref().is_some_and(|p| p.intent == intent.proof))
        {
            return Err(StoreError::NotFound);
        }
        let (disposition, reply, failure) = mcp::terminal_payload(input)?;
        let envelope = Envelope {
            schema_version: 1,
            intent: intent.proof.clone(),
            disposition,
            reply,
            failure,
        };
        validate_envelope(&envelope)?;
        let bytes = bounded_json(&envelope, MAX_VISIBLE_BYTES).map_err(|_| StoreError::Overflow)?;
        let hash = content_hash(&bytes);
        Ok(McpPromptPublication {
            root_identity: intent.root_identity,
            session: intent.session.clone(),
            proof: intent.proof.clone(),
            result: result_id(&hash)?,
            terminal: Terminal {
                disposition,
                envelope_hash: hash,
            },
            message: Message::user(String::from_utf8(bytes).map_err(|_| StoreError::RunConflict)?),
        })
    }

    pub fn publish_mcp_prompt_terminal(
        &mut self,
        publication: &McpPromptPublication,
    ) -> StoreResult<McpPromptResult> {
        self.ready()?;
        let index = self.prompt_index(
            publication.root_identity,
            &publication.session,
            &publication.proof,
        )?;
        let record = &self.published.state.runs[index];
        let op_index = record
            .operations
            .iter()
            .position(|op| op.operation_id == publication.proof.operation)
            .ok_or(StoreError::NotFound)?;
        let saved = record.operations[op_index]
            .prompt
            .as_ref()
            .ok_or(StoreError::RunConflict)?;
        if saved.intent != publication.proof {
            return Err(StoreError::RunConflict);
        }
        if saved.terminal.is_some() {
            return self.confirm_prompt_saved(publication);
        }
        if record.run.state.is_terminal()
            || self.published.raw.last().map(|event| event.sequence)
                != Some(publication.proof.input.sequence)
        {
            return Err(StoreError::RunConflict);
        }
        let mut next = self.published.clone();
        let raw_index = next.raw_run_index(index)?;
        append_turn_suffix(&mut next.raw, raw_index, vec![publication.message.clone()])?;
        let op = &mut next.state.runs[index].operations[op_index];
        op.result_id = Some(publication.result.clone());
        op.prompt.as_mut().unwrap().terminal = Some(publication.terminal.clone());
        self.commit(next, true)?;
        self.confirm_prompt_saved(publication)
    }

    pub fn confirm_mcp_prompt_terminal(
        &mut self,
        publication: &McpPromptPublication,
    ) -> StoreResult<McpPromptResult> {
        self.recover()?;
        self.confirm_prompt_saved(publication)
    }

    fn prompt_index(
        &self,
        root_identity: (u64, u64),
        session: &SessionId,
        proof: &Proof,
    ) -> StoreResult<usize> {
        if self.root_identity()? != root_identity || &self.published.marker.session_id != session {
            return Err(StoreError::TargetMismatch);
        }
        let index = self.run_index(&RunTarget {
            run_id: proof.run.clone(),
            attempt_id: proof.attempt.clone(),
        })?;
        proof.validate(&self.published.state.runs[index], &proof.operation)?;
        check_input(&self.published, index, proof)?;
        Ok(index)
    }

    fn confirm_prompt_saved(
        &self,
        publication: &McpPromptPublication,
    ) -> StoreResult<McpPromptResult> {
        let index = self.prompt_index(
            publication.root_identity,
            &publication.session,
            &publication.proof,
        )?;
        let op = self.published.state.runs[index]
            .operations
            .iter()
            .find(|op| op.operation_id == publication.proof.operation)
            .ok_or(StoreError::NotFound)?;
        let saved = op.prompt.as_ref().ok_or(StoreError::RunConflict)?;
        let event = attachment(&self.published, index)?.ok_or(StoreError::RunConflict)?;
        if saved.intent != publication.proof
            || saved.terminal.as_ref() != Some(&publication.terminal)
            || op.result_id.as_ref() != Some(&publication.result)
            || !same_message(&event.message, &publication.message)?
        {
            return Err(StoreError::RunConflict);
        }
        Ok(McpPromptResult {
            message: event.message.clone(),
            disposition: publication.terminal.disposition,
        })
    }
}

/// Only this exact, fully verified event may be excluded from the assistant/tool suffix.
pub(super) fn attachment(p: &Published, index: usize) -> StoreResult<Option<&RawEventV2>> {
    let record = &p.state.runs[index];
    let Some(saved) = validate_record(record)? else {
        return Ok(None);
    };
    if saved.terminal.is_none() {
        return Ok(None);
    }
    let sequence = saved
        .intent
        .input
        .sequence
        .checked_add(1)
        .ok_or(StoreError::Overflow)?;
    let event = p
        .raw
        .iter()
        .find(|event| event.sequence == sequence)
        .ok_or(StoreError::RunConflict)?;
    saved.validate_pair(&p.marker, &record.input.text, input(p, index)?, event)?;
    Ok(Some(event))
}

pub(in crate::desktop_store) fn validate_state(state: &Sidecar) -> StoreResult<()> {
    for record in &state.runs {
        validate_record(record)?;
        for op in &record.operations {
            if op.prompt.is_some()
                && state
                    .approval_records
                    .iter()
                    .any(|approval| approval.pending.operation_id == op.operation_id)
            {
                return Err(StoreError::Corrupt("MCP prompt operation association"));
            }
        }
    }
    Ok(())
}

/// Validate one run without reconstructing a mutable owner or a send grant.
pub(in crate::desktop_store) fn validate_record(
    record: &RunRecord,
) -> StoreResult<Option<&SavedMcpPrompt>> {
    if let Some(choice) = &record.mcp_prompt {
        validate_choice(choice)?;
    }
    let mut found = None;
    for op in &record.operations {
        let Some(saved) = &op.prompt else {
            continue;
        };
        if found.is_some() || op.mcp.is_some() || saved.terminal.is_some() != op.result_id.is_some()
        {
            return Err(StoreError::Corrupt("MCP prompt operation association"));
        }
        saved.intent.validate(record, &op.operation_id)?;
        if let Some(terminal) = &saved.terminal
            && (op.result_id.as_ref() != Some(&result_id(&terminal.envelope_hash)?)
                || (terminal.disposition == McpDisposition::OutcomeUnknown
                    && record.run.state.is_terminal()
                    && record.run.state != RunState::OutcomeUnknown))
        {
            return Err(StoreError::RunConflict);
        }
        found = Some(saved);
    }
    if record.mcp_prompt.is_some()
        && record.run.state == RunState::Succeeded
        && found.and_then(SavedMcpPrompt::disposition) != Some(McpDisposition::Completed)
    {
        return Err(StoreError::RunConflict);
    }
    Ok(found)
}

pub(super) fn validate(p: &Published) -> StoreResult<()> {
    validate_state(&p.state)?;
    for (index, record) in p.state.runs.iter().enumerate() {
        if record.mcp_prompt.is_none() {
            continue;
        }
        let input = input(p, index)?;
        let supplemental = attachment(p, index)?.map(|event| event.sequence);
        let mut suffix = Vec::new();
        for event in p
            .raw
            .iter()
            .filter(|event| event.turn_id == input.turn_id && !event.starts_turn)
        {
            if Some(event.sequence) != supplemental {
                suffix.push(event.message.clone());
            }
        }
        if let Some(saved) = record.operations.iter().find_map(|op| op.prompt.as_ref()) {
            check_input(p, index, &saved.intent)?;
        }
        if !suffix.is_empty()
            && !record.operations.iter().any(|op| {
                op.prompt.as_ref().and_then(SavedMcpPrompt::disposition)
                    == Some(McpDisposition::Completed)
            })
        {
            return Err(StoreError::RunConflict);
        }
        validate_turn_suffix(&suffix, Observation::Cancelled)?;
    }
    Ok(())
}

pub(in crate::desktop_store) fn validate_transition(
    before: &Sidecar,
    after: &Sidecar,
) -> StoreResult<()> {
    for prior in &before.runs {
        let next = after
            .runs
            .iter()
            .find(|run| run.run.run_id == prior.run.run_id)
            .ok_or(StoreError::RunConflict)?;
        if prior.mcp_prompt != next.mcp_prompt {
            return Err(StoreError::Corrupt("Admitted MCP prompt selection changed"));
        }
        for operation in &prior.operations {
            let current = next
                .operations
                .iter()
                .find(|op| op.operation_id == operation.operation_id);
            match (&operation.prompt, current.and_then(|op| op.prompt.as_ref())) {
                (None, None) => {}
                (Some(old), Some(new))
                    if old.intent == new.intent
                        && (old.terminal.is_none() || old.terminal == new.terminal)
                        && (operation.result_id.is_none()
                            || current.unwrap().result_id == operation.result_id) => {}
                _ => {
                    return Err(StoreError::Corrupt(
                        "MCP prompt evidence changed or deleted",
                    ));
                }
            }
        }
    }
    Ok(())
}
