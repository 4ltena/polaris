//! Ephemeral raw-event identity and request slots for lossless input preparation.

use crate::{
    conversation_state::{RawEventV2, content_hash},
    input_preparation::{InputPreparationMode, Preparation, Projection, prepare},
};
use polaris_provider::{Message, Role};
use std::{
    ops::Range,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SourceBinding {
    session: String,
    epoch: u64,
    turn: u64,
    sequence: u64,
    hash: String,
}

impl SourceBinding {
    pub(crate) fn from_event(session: &str, event: &RawEventV2) -> Option<Self> {
        (event.starts_turn && event.message.role == Role::User).then(|| Self {
            session: session.to_owned(),
            epoch: event.epoch,
            turn: event.turn_id,
            sequence: event.sequence,
            hash: content_hash(event.message.content.as_bytes()),
        })
    }
}

/// Constructed only alongside a host projection of a verified raw event.
/// Neither text matching nor the position of the last user establishes identity.
#[derive(Debug, Clone)]
pub struct BoundInput {
    pub(crate) source: SourceBinding,
    pub(crate) index: usize,
    range: Range<usize>,
    fresh: Arc<AtomicBool>,
}

impl BoundInput {
    pub(crate) fn new(source: SourceBinding, index: usize, bytes: usize) -> Self {
        Self {
            source,
            index,
            range: 0..bytes,
            fresh: Arc::new(AtomicBool::new(true)),
        }
    }

    pub(crate) fn original<'a>(&self, messages: &'a [Message]) -> Option<&'a str> {
        let message = messages.get(self.index)?;
        if message.role != Role::User
            || !message.tool_calls.is_empty()
            || message.tool_call_id.is_some()
        {
            return None;
        }
        let raw = message.content.get(self.range.clone())?;
        (content_hash(raw.as_bytes()) == self.source.hash).then_some(raw)
    }

    pub(crate) fn take_fresh(&mut self) -> bool {
        self.fresh.swap(false, Ordering::AcqRel)
    }

    pub(crate) fn refreshed(&self, mut candidate: Self) -> Option<Self> {
        if self.source != candidate.source {
            return None;
        }
        candidate.fresh = self.fresh.clone();
        Some(candidate)
    }
}

/// Build raw messages and the corresponding slot in the same pass.
pub(crate) fn project_events<'a>(
    session: &str,
    events: impl Iterator<Item = &'a RawEventV2>,
    source: Option<&SourceBinding>,
) -> (Vec<Message>, Option<BoundInput>) {
    let mut messages = Vec::new();
    let mut bound = None;
    for event in events {
        if source.is_some_and(|source| {
            source.session == session
                && source.epoch == event.epoch
                && source.sequence == event.sequence
                && source.turn == event.turn_id
        }) && let Some(identity) = SourceBinding::from_event(session, event)
            && Some(&identity) == source
        {
            bound = Some(BoundInput::new(
                identity,
                messages.len(),
                event.message.content.len(),
            ));
        }
        messages.push(event.message.clone());
    }
    (messages, bound)
}

#[cfg(test)]
mod tests;

/// One root invocation's cached projection. Never serialized into history.
pub(crate) struct PreparedInput {
    bound: BoundInput,
    projection: Projection,
}

impl PreparedInput {
    pub(crate) fn prepare(
        bound: &BoundInput,
        messages: &[Message],
        mode: InputPreparationMode,
    ) -> Option<(Option<Self>, Preparation)> {
        let preparation = prepare(bound.original(messages)?, mode);
        let prepared = preparation.projection.as_ref().map(|projection| Self {
            bound: bound.clone(),
            projection: projection.clone(),
        });
        Some((prepared, preparation))
    }

    pub(crate) fn apply(&self, messages: &mut [Message], current: Option<&BoundInput>) -> bool {
        let Some(current) = current else {
            return false;
        };
        if current.source != self.bound.source
            || current.original(messages).is_none()
            || self.projection.source_hash != current.source.hash
        {
            return false;
        }
        messages[current.index]
            .content
            .replace_range(current.range.clone(), &self.projection.encoded);
        true
    }
}
