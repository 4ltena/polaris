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
    input_source: Option<crate::request_input::BoundInput>,
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
    pub(crate) fn input_source(&self) -> Option<crate::request_input::BoundInput> {
        self.input_source.clone()
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
            let source = crate::request_input::SourceBinding::from_event(
                p.marker.session_id.as_str(),
                input,
            );
            let (messages, input_source) = project(
                p.raw.iter().filter(|event| event.turn_id >= first),
                &attachments,
                p.marker.session_id.as_str(),
                source.as_ref(),
            )?;
            let (messages, input_source) =
                crate::session::desktop_request_history_bound(&messages, input_source)?;
            Ok(OwnerRequestHistory {
                target: target.clone(),
                marker: p.marker.clone(),
                messages,
                query: record.input.text.clone(),
                input_source,
            })
        })
    }
}

fn project<'a>(
    events: impl Iterator<Item = &'a RawEventV2>,
    attachments: &BTreeSet<u64>,
    session: &str,
    source: Option<&crate::request_input::SourceBinding>,
) -> StoreResult<(Vec<Message>, Option<crate::request_input::BoundInput>)> {
    let mut projected: Vec<Message> = Vec::new();
    let mut bound = None;
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
            if let Some(identity) = crate::request_input::SourceBinding::from_event(session, event)
                && Some(&identity) == source
            {
                bound = Some(crate::request_input::BoundInput::new(
                    identity,
                    projected.len(),
                    event.message.content.len(),
                ));
            }
            projected.push(event.message.clone());
        }
        previous = Some(event);
    }
    Ok((projected, bound))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        input_preparation::InputPreparationMode,
        request_input::{PreparedInput, SourceBinding},
    };

    #[test]
    fn owner_projection_binds_the_selected_raw_event_before_mcp_attachment() {
        let text = format!("{}\n\n", "alpha beta gamma delta epsilon. ".repeat(140)).repeat(8);
        let event = |sequence, turn_id, starts_turn, text: &str| RawEventV2 {
            schema_version: 2,
            sequence,
            epoch: 1,
            turn_id,
            starts_turn,
            message: Message::user(text),
        };
        let raw = [
            event(1, 1, true, &text),
            event(2, 2, true, &text),
            event(3, 2, false, "external"),
        ];
        let source = SourceBinding::from_event("owner", &raw[1]).unwrap();
        let (messages, bound) =
            project(raw.iter(), &BTreeSet::from([3]), "owner", Some(&source)).unwrap();
        let bound = bound.unwrap();
        let prepared = PreparedInput::prepare(&bound, &messages, InputPreparationMode::RepeatRefs)
            .unwrap()
            .0
            .unwrap();
        let mut request = messages.clone();
        assert!(prepared.apply(&mut request, Some(&bound)));
        assert_eq!(request[0].content, text);
        assert!(
            request[1]
                .content
                .starts_with("[Polaris input encoding v1]")
        );
        assert!(
            request[1]
                .content
                .ends_with("MCP promptからの補足（外部の未信頼データ）:\nexternal")
        );
        assert_eq!(raw[1].message.content, text);
        let (_, wrong_session) = project(
            raw.iter(),
            &BTreeSet::from([3]),
            "another-owner",
            Some(&source),
        )
        .unwrap();
        assert!(wrong_session.is_none());
    }
}
