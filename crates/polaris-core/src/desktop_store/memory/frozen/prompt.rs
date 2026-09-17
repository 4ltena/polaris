//! Validate host prompt provenance without recreating a parent execution owner.
use super::*;
use crate::desktop_store::McpDisposition;
use crate::desktop_store::{SavedMcpPrompt, writer};
use polaris_desktop_protocol::run_state::Observation;
use polaris_provider::Message;

#[derive(Serialize)]
pub(super) struct Selected {
    input: String,
    receipt: Option<SavedMcpPrompt>,
}
impl Selected {
    pub(super) fn from_record(record: &RunRecord) -> StoreResult<Option<Self>> {
        let receipt = writer::prompt::validate_record(record)?;
        Ok(record.mcp_prompt.as_ref().map(|_| Self {
            input: record.input.text.clone(),
            receipt: receipt.cloned(),
        }))
    }
}

pub(super) struct Raw<'a, 'b> {
    collector: &'a Collector<'b>,
    base_used: usize,
    used: usize,
    turns: u64,
    selected_count: usize,
    selected: Option<&'a Selected>,
    input: Option<RawEventV2>,
    attachment: bool,
    suffix: Vec<Message>,
    suffix_bytes: usize,
    attachments: BTreeSet<u64>,
}
impl<'a, 'b> Raw<'a, 'b> {
    pub(super) fn new(collector: &'a Collector<'b>, used: usize) -> Self {
        Self {
            collector,
            base_used: used,
            used,
            turns: 0,
            selected_count: 0,
            selected: None,
            input: None,
            attachment: false,
            suffix: Vec::new(),
            suffix_bytes: 2,
            attachments: BTreeSet::new(),
        }
    }

    pub(super) fn event(&mut self, event: &RawEventV2) -> StoreResult<()> {
        if event.starts_turn {
            self.finish_turn()?;
            self.turns = self.turns.checked_add(1).ok_or(StoreError::Overflow)?;
            if self.turns != event.turn_id {
                return Err(StoreError::Corrupt("固定原文のrun位置"));
            }
            self.selected = self
                .turns
                .checked_sub(self.collector.inherited_turn_count)
                .and_then(|ordinal| self.collector.prompts.get(&ordinal));
            self.input = None;
            self.attachment = false;
            self.suffix.clear();
            self.suffix_bytes = 2;
            self.used = self.base_used;
            if let Some(selected) = self.selected {
                charge(&mut self.used, event)?;
                if let Some(receipt) = &selected.receipt {
                    receipt.validate_input(self.collector.marker, &selected.input, event)?;
                }
                self.input = Some(event.clone());
                self.selected_count += 1;
            }
            return Ok(());
        }
        let (Some(selected), Some(input)) = (self.selected, &self.input) else {
            return Ok(());
        };
        let disposition = selected
            .receipt
            .as_ref()
            .and_then(SavedMcpPrompt::disposition);
        if disposition.is_some() && !self.attachment {
            selected.receipt.as_ref().unwrap().validate_pair(
                self.collector.marker,
                &selected.input,
                input,
                event,
            )?;
            self.attachment = true;
            charge(&mut self.used, &event.sequence)?;
            charge(&mut self.base_used, &event.sequence)?;
            self.attachments.insert(event.sequence);
            return Ok(());
        }
        if disposition != Some(McpDisposition::Completed) {
            return Err(StoreError::RunConflict);
        }
        if self.suffix.len() >= 256 {
            return Err(StoreError::Overflow);
        }
        let size = bounded::encoded_size(
            &event.message,
            (4 * 1024 * 1024usize).saturating_sub(self.suffix_bytes),
        )?;
        self.suffix_bytes = self
            .suffix_bytes
            .checked_add(size + usize::from(!self.suffix.is_empty()))
            .filter(|n| *n <= 4 * 1024 * 1024)
            .ok_or(StoreError::Overflow)?;
        charge(&mut self.used, &event.message)?;
        self.suffix.push(event.message.clone());
        Ok(())
    }

    fn finish_turn(&self) -> StoreResult<()> {
        if let Some(selected) = self.selected {
            if selected
                .receipt
                .as_ref()
                .and_then(SavedMcpPrompt::disposition)
                .is_some()
                && !self.attachment
            {
                return Err(StoreError::Corrupt("固定原文のprompt補助不在"));
            }
            writer::validate_turn_suffix(&self.suffix, Observation::Cancelled)?;
        }
        Ok(())
    }

    pub(super) fn finish(self) -> StoreResult<BTreeSet<u64>> {
        self.finish_turn()?;
        if self
            .collector
            .inherited_turn_count
            .checked_add(self.collector.run_count)
            != Some(self.turns)
            || self.selected_count != self.collector.prompts.len()
        {
            return Err(StoreError::Corrupt("固定原文のrun対応不在"));
        }
        Ok(self.attachments)
    }
}
