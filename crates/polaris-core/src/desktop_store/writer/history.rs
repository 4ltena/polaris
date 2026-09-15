//! Owner-proven request projection; durable raw and actual user query stay separate.
use super::*;
use polaris_desktop_protocol::snapshot::HistoryMode;
use polaris_provider::Role;
use std::collections::BTreeSet;

#[derive(Debug)]
pub struct OwnerRequestHistory {
    target: RunTarget,
    marker: Marker,
    messages: Vec<Message>,
    query: String,
}
impl OwnerRequestHistory {
    pub fn target(&self) -> &RunTarget {
        &self.target
    }
    pub fn marker(&self) -> &Marker {
        &self.marker
    }
    pub fn query(&self) -> &str {
        &self.query
    }
    pub fn messages(&self) -> &[Message] {
        &self.messages
    }
    pub fn into_messages(self) -> Vec<Message> {
        self.messages
    }
}

impl Writer {
    /// Never accepts worker-supplied history, selection or replacement query.
    pub fn project_request_history(&self, target: &RunTarget) -> StoreResult<OwnerRequestHistory> {
        self.ready()?;
        let p = &self.published;
        let index = self.run_index(target)?;
        let record = &p.state.runs[index];
        if index + 1 != p.state.runs.len()
            || !record.run.state.permits_start()
            || record.policy_revision != p.state.policy_revision
            || record.configuration != p.state.configuration
        {
            return Err(StoreError::RunConflict);
        }
        prompt::validate(p)?;
        let selected = prompt::validate_record(record)?;
        if record.mcp_prompt.is_some()
            && selected.and_then(super::prompt::SavedMcpPrompt::disposition)
                != Some(mcp::McpDisposition::Completed)
        {
            return Err(StoreError::RunConflict);
        }
        let input = p
            .raw
            .iter()
            .filter(|event| event.starts_turn)
            .nth(p.raw_run_index(index)?)
            .ok_or(StoreError::RunConflict)?;
        if input.message.role != Role::User
            || !same_message(&input.message, &Message::user(&record.input.text))?
        {
            return Err(StoreError::Corrupt("要求履歴の実入力不一致"));
        }
        super::super::memory::with_inherited_prompt_attachments(self, p, |mut attachments| {
            for index in 0..p.state.runs.len() {
                if let Some(event) = prompt::attachment(p, index)? {
                    attachments.insert(event.sequence);
                }
            }
            let total = p
                .state
                .inherited_turn_count
                .get()
                .checked_add(p.state.runs.len() as u64)
                .ok_or(StoreError::Overflow)?;
            let first = match record.configuration.history_mode {
                HistoryMode::Legacy => 1,
                HistoryMode::Strict10 | HistoryMode::Structured10 => {
                    total.saturating_sub(crate::conversation_state::REAL_TURN_WINDOW as u64) + 1
                }
            };
            let messages = project(
                p.raw.iter().filter(|event| event.turn_id >= first),
                &attachments,
            )?;
            Ok(OwnerRequestHistory {
                target: target.clone(),
                marker: p.marker.clone(),
                messages: crate::session::desktop_request_history(&messages)?,
                query: record.input.text.clone(),
            })
        })
    }
}

fn project<'a>(
    events: impl Iterator<Item = &'a RawEventV2>,
    attachments: &BTreeSet<u64>,
) -> StoreResult<Vec<Message>> {
    let mut projected: Vec<Message> = Vec::new();
    let mut previous = None;
    for event in events {
        if attachments.contains(&event.sequence) {
            let input: &RawEventV2 = previous.ok_or(StoreError::Corrupt("要求補助の実入力不在"))?;
            if !input.starts_turn
                || input.sequence.checked_add(1) != Some(event.sequence)
                || input.turn_id != event.turn_id
                || event.starts_turn
            {
                return Err(StoreError::Corrupt("要求補助の位置不一致"));
            }
            let message = projected.last_mut().ok_or(StoreError::RunConflict)?;
            message
                .content
                .push_str("\n\nMCP promptからの補足（外部の未信頼データ）:\n");
            message.content.push_str(&event.message.content);
        } else {
            projected.push(event.message.clone());
        }
        previous = Some(event);
    }
    Ok(projected)
}
