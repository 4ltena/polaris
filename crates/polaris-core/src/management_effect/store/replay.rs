//! Pure V1 journal envelopes and replay reducer.

#[cfg(any(unix, all(test, windows)))]
use super::{
    JOURNAL_VERSION, ManagementEffectEventEvidenceV1, ManagementEffectStateV1,
    ManagementEffectStoreError, validate_evidence_hash,
};
use super::{
    ManagementEffectCursorV1, ManagementEffectEventV1, ManagementEffectHashV1,
    ManagementEffectSnapshotV1, ManagementOperationIdV1, VerifiedManagementEffectEventV1,
};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub(super) type JournalEventV1 = ManagementEffectEventV1;

#[derive(Debug, Serialize)]
pub(super) struct UnsignedEnvelopeV1<'a> {
    pub(super) version: u32,
    pub(super) sequence: u64,
    pub(super) previous_hash: ManagementEffectHashV1,
    pub(super) event: &'a JournalEventV1,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct JournalEnvelopeV1 {
    pub(super) version: u32,
    pub(super) sequence: u64,
    pub(super) previous_hash: ManagementEffectHashV1,
    pub(super) event: JournalEventV1,
    pub(super) event_hash: ManagementEffectHashV1,
}

#[derive(Debug, Default)]
pub(super) struct JournalStateV1 {
    pub(super) events: BTreeMap<u64, VerifiedManagementEffectEventV1>,
    pub(super) generation: u64,
    pub(super) chain_hash: ManagementEffectHashV1,
    pub(super) archived_effects: u64,
    pub(super) prior_records: u64,
    pub(super) sequence: u64,
    pub(super) journal_hash: ManagementEffectHashV1,
    pub(super) records: usize,
    pub(super) effects: BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV1>,
}

impl JournalStateV1 {
    pub(super) fn cursor(&self) -> ManagementEffectCursorV1 {
        ManagementEffectCursorV1 {
            generation: self.generation,
            chain_hash: self.chain_hash,
            sequence: self.sequence,
            journal_hash: self.journal_hash,
        }
    }

    #[cfg(any(unix, all(test, windows)))]
    pub(super) fn apply(
        &mut self,
        envelope: JournalEnvelopeV1,
    ) -> Result<(), ManagementEffectStoreError> {
        if envelope.version != JOURNAL_VERSION
            || envelope.sequence != self.sequence + 1
            || envelope.previous_hash != self.journal_hash
        {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        let expected = ManagementEffectHashV1::digest(&serde_json::to_vec(&UnsignedEnvelopeV1 {
            version: envelope.version,
            sequence: envelope.sequence,
            previous_hash: envelope.previous_hash,
            event: &envelope.event,
        })?);
        if expected != envelope.event_hash {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        let verified = VerifiedManagementEffectEventV1 {
            evidence: ManagementEffectEventEvidenceV1 {
                generation: self.generation,
                sequence: envelope.sequence,
                event_hash: envelope.event_hash,
            },
            event: envelope.event.clone(),
        };
        self.apply_event(envelope.sequence, envelope.event)?;
        if self.events.insert(envelope.sequence, verified).is_some() {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        self.sequence = envelope.sequence;
        self.journal_hash = envelope.event_hash;
        self.records += 1;
        Ok(())
    }

    #[cfg(any(unix, all(test, windows)))]
    pub(super) fn apply_event(
        &mut self,
        sequence: u64,
        event: JournalEventV1,
    ) -> Result<(), ManagementEffectStoreError> {
        match event {
            JournalEventV1::Prepared { request } => {
                if self.effects.contains_key(&request.operation_id) {
                    return Err(ManagementEffectStoreError::CorruptJournal);
                }
                self.effects.insert(
                    request.operation_id,
                    ManagementEffectSnapshotV1 {
                        request,
                        state: ManagementEffectStateV1::Prepared,
                        last_sequence: sequence,
                    },
                );
            }
            JournalEventV1::EffectStarted { operation_id } => self.transition(
                operation_id,
                sequence,
                |state| matches!(state, ManagementEffectStateV1::Prepared),
                ManagementEffectStateV1::EffectStarted,
            )?,
            JournalEventV1::OutcomeUnknown {
                operation_id,
                reason,
            } => self.transition(
                operation_id,
                sequence,
                |state| matches!(state, ManagementEffectStateV1::EffectStarted),
                ManagementEffectStateV1::OutcomeUnknown { reason },
            )?,
            JournalEventV1::Completed {
                operation_id,
                evidence_hash,
            } => {
                validate_evidence_hash(evidence_hash)
                    .map_err(|_| ManagementEffectStoreError::CorruptJournal)?;
                self.transition(
                    operation_id,
                    sequence,
                    |state| {
                        matches!(
                            state,
                            ManagementEffectStateV1::EffectStarted
                                | ManagementEffectStateV1::OutcomeUnknown { .. }
                        )
                    },
                    ManagementEffectStateV1::Completed { evidence_hash },
                )?;
            }
            JournalEventV1::Denied {
                operation_id,
                reason,
            } => self.transition(
                operation_id,
                sequence,
                |state| matches!(state, ManagementEffectStateV1::Prepared),
                ManagementEffectStateV1::Denied { reason },
            )?,
            JournalEventV1::ReconciledAbandon {
                operation_id,
                evidence_hash,
            } => {
                validate_evidence_hash(evidence_hash)
                    .map_err(|_| ManagementEffectStoreError::CorruptJournal)?;
                self.transition(
                    operation_id,
                    sequence,
                    |state| matches!(state, ManagementEffectStateV1::OutcomeUnknown { .. }),
                    ManagementEffectStateV1::ReconciledAbandon { evidence_hash },
                )?;
            }
        }
        Ok(())
    }

    #[cfg(any(unix, all(test, windows)))]
    fn transition(
        &mut self,
        operation_id: ManagementOperationIdV1,
        sequence: u64,
        allowed: impl FnOnce(&ManagementEffectStateV1) -> bool,
        next: ManagementEffectStateV1,
    ) -> Result<(), ManagementEffectStoreError> {
        let effect = self
            .effects
            .get_mut(&operation_id)
            .ok_or(ManagementEffectStoreError::CorruptJournal)?;
        if effect.state.is_terminal() || !allowed(&effect.state) {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        effect.state = next;
        effect.last_sequence = sequence;
        Ok(())
    }
}
