//! Display projection of one saved run: its accepted input and raw turn. The
//! run-to-turn mapping is checked, never inferred from text or order on screen.
use super::*;
use polaris_desktop_protocol::run_details::RunDetailKind;
use polaris_provider::Role;
use serde_json::json;
use std::collections::BTreeSet;

const MAX_DETAILS_BYTES: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunDetailEntry {
    /// Host generated from the raw sequence and the item's ordinal in it; an
    /// unsaved result reuses its call's ID with an `-unsaved` suffix.
    pub id: String,
    pub kind: RunDetailKind,
    pub text: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunDetails {
    pub session_revision: DecimalU64,
    pub accepted_revision: DecimalU64,
    pub run_state: RunState,
    pub history_gap: bool,
    pub entries: Vec<RunDetailEntry>,
}

impl Writer {
    /// Read only: no model, tool, file or ledger effect.
    pub fn project_run_details(&self, target: &RunTarget) -> StoreResult<RunDetails> {
        self.ready()?;
        project(&self.published, target)
    }
}

pub(in crate::desktop_store) fn project(
    p: &Published,
    target: &RunTarget,
) -> StoreResult<RunDetails> {
    let mismatch = || StoreError::Corrupt("run詳細の対応不一致");
    let inputs: Vec<&RawEventV2> = p.raw.iter().filter(|event| event.starts_turn).collect();
    let inherited =
        usize::try_from(p.state.inherited_turn_count.get()).map_err(|_| StoreError::Overflow)?;
    let accepted: Vec<&RequestRecord> = p
        .state
        .requests
        .iter()
        .filter(|record| matches!(record.result, RequestResult::RunAccepted { .. }))
        .collect();
    if inputs.len()
        != inherited
            .checked_add(p.state.runs.len())
            .ok_or(StoreError::Overflow)?
        || accepted.len() != p.state.runs.len()
    {
        return Err(mismatch());
    }
    let mut previous: Option<DecimalU64> = None;
    let mut found = None;
    for (index, record) in p.state.runs.iter().enumerate() {
        let matching: Vec<_> = accepted
            .iter()
            .filter(|request| {
                matches!(&request.result, RequestResult::RunAccepted { run_id, attempt_id }
                    if *run_id == record.run.run_id && *attempt_id == record.run.attempt_id)
            })
            .collect();
        let [acceptance] = matching.as_slice() else {
            return Err(mismatch());
        };
        if previous.is_some_and(|previous| acceptance.accepted_revision <= previous) {
            return Err(mismatch());
        }
        previous = Some(acceptance.accepted_revision);
        let input = inputs[inherited + index];
        if input.message.role != Role::User
            || !same_message(&input.message, &Message::user(&record.input.text))?
        {
            return Err(mismatch());
        }
        if record.run.run_id == target.run_id && record.run.attempt_id == target.attempt_id {
            found = Some((index, acceptance.accepted_revision, input));
        }
    }
    let (index, accepted_revision, input) = found.ok_or(StoreError::NotFound)?;
    let record = &p.state.runs[index];
    let attachment = prompt::attachment(p, index)?.map(|event| event.sequence);
    let mut entries = Vec::new();
    let mut bytes = 0usize;
    let mut push = |id: String, kind, text: String| {
        bytes = bytes
            .checked_add(text.len())
            .filter(|total| *total <= MAX_DETAILS_BYTES)
            .ok_or(StoreError::Overflow)?;
        entries.push(RunDetailEntry { id, kind, text });
        Ok::<_, StoreError>(())
    };
    push(
        format!("{}-0", input.sequence),
        RunDetailKind::UserInput,
        input.message.content.clone(),
    )?;
    let mut calls = Vec::new();
    let mut answered = BTreeSet::new();
    for event in p
        .raw
        .iter()
        .filter(|event| event.turn_id == input.turn_id && !event.starts_turn)
    {
        let message = &event.message;
        let mut ordinal = 0;
        match message.role {
            Role::User if Some(event.sequence) == attachment => push(
                format!("{}-0", event.sequence),
                RunDetailKind::McpPrompt,
                message.content.clone(),
            )?,
            Role::Assistant => {
                if !message.content.is_empty() {
                    push(
                        format!("{}-{ordinal}", event.sequence),
                        RunDetailKind::Assistant,
                        message.content.clone(),
                    )?;
                    ordinal += 1;
                }
                for call in &message.tool_calls {
                    let text =
                        json!({"id": call.id, "name": call.name, "arguments": call.arguments});
                    let entry = format!("{}-{ordinal}", event.sequence);
                    push(entry.clone(), RunDetailKind::ToolCall, text.to_string())?;
                    ordinal += 1;
                    calls.push((entry, call.id.clone()));
                }
                if !message.hosted_web_search.is_empty() {
                    let text = serde_json::to_string(&message.hosted_web_search)?;
                    push(
                        format!("{}-{ordinal}", event.sequence),
                        RunDetailKind::HostedWebSearch,
                        text,
                    )?;
                }
            }
            Role::Tool => {
                let id = message.tool_call_id.clone().ok_or_else(mismatch)?;
                let text = json!({"tool_call_id": id, "content": message.content});
                push(
                    format!("{}-{ordinal}", event.sequence),
                    RunDetailKind::ToolResult,
                    text.to_string(),
                )?;
                answered.insert(id);
            }
            _ => return Err(mismatch()),
        }
    }
    for (entry, id) in calls {
        if !answered.contains(&id) {
            let text = json!({ "tool_call_id": id }).to_string();
            push(
                format!("{entry}-unsaved"),
                RunDetailKind::ToolResultUnsaved,
                text,
            )?;
        }
    }
    Ok(RunDetails {
        session_revision: p.marker.session_revision,
        accepted_revision,
        run_state: record.run.state,
        history_gap: record.history_gap.is_some(),
        entries,
    })
}
