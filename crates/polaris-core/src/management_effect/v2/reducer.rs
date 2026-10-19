//! V2 lifecycle replay. The store supplies durable ledger proofs; this module
//! never treats a hash, a missing path, or an unreadable ledger as a proof.
use super::*;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagementEffectEventV2 {
    Prepared {
        request: PrepareManagementEffectV2,
    },
    EffectStarted {
        operation_id: ManagementOperationIdV1,
    },
    CredentialAuthorizationArmedV1 {
        operation_id: ManagementOperationIdV1,
        ledger_epoch_id: ManagementEffectHashV1,
        reserved_entry_hash: ManagementEffectHashV1,
    },
    OutcomeUnknown {
        operation_id: ManagementOperationIdV1,
        reason: ManagementUnknownReasonV2,
    },
    Completed {
        operation_id: ManagementOperationIdV1,
        evidence_hash: ManagementEffectHashV1,
    },
    Denied {
        operation_id: ManagementOperationIdV1,
        reason: ManagementDenialReasonV1,
    },
    ReconciledAbandon {
        operation_id: ManagementOperationIdV1,
        evidence_hash: ManagementEffectHashV1,
    },
    ReconciledSupersededV1 {
        operation_id: ManagementOperationIdV1,
        resolution_operation_id: ManagementOperationIdV1,
        certificate_id: ManagementEffectHashV1,
    },
}

impl ManagementEffectEventV2 {
    pub fn operation_id(&self) -> ManagementOperationIdV1 {
        match self {
            Self::Prepared { request } => request.base.operation_id,
            Self::EffectStarted { operation_id }
            | Self::CredentialAuthorizationArmedV1 { operation_id, .. }
            | Self::OutcomeUnknown { operation_id, .. }
            | Self::Completed { operation_id, .. }
            | Self::Denied { operation_id, .. }
            | Self::ReconciledAbandon { operation_id, .. }
            | Self::ReconciledSupersededV1 { operation_id, .. } => *operation_id,
        }
    }
}

/// Implemented by the store-scoped coordinator after durable read-back.
/// There is deliberately no permissive/default implementation. A failed
/// proof must leave the project snapshot and journal unchanged.
pub(crate) trait CredentialEvidenceVerifierV1 {
    fn reserved(
        &self,
        effect: &ManagementEffectSnapshotV2,
        epoch: ManagementEffectHashV1,
        entry: ManagementEffectHashV1,
    ) -> Result<(), ManagementEffectStoreError>;
    fn zero_match(
        &self,
        effect: &ManagementEffectSnapshotV2,
    ) -> Result<(), ManagementEffectStoreError>;
    fn completed(
        &self,
        effect: &ManagementEffectSnapshotV2,
        evidence: ManagementEffectHashV1,
    ) -> Result<(), ManagementEffectStoreError>;
    fn abandoned(
        &self,
        effect: &ManagementEffectSnapshotV2,
        evidence: ManagementEffectHashV1,
    ) -> Result<(), ManagementEffectStoreError>;
    fn superseded(
        &self,
        predecessor: &ManagementEffectSnapshotV2,
        resolver: &ManagementEffectSnapshotV2,
        effects: &BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV2>,
        certificate: ManagementEffectHashV1,
    ) -> Result<(), ManagementEffectStoreError>;
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(crate) struct ManagementEffectReplayV2 {
    pub(super) effects: BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV2>,
    sequence: u64,
}

impl ManagementEffectReplayV2 {
    pub(super) fn from_checkpoint(
        effects: BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV2>,
        sequence: u64,
    ) -> Result<Self, ManagementEffectStoreError> {
        validate_recovery_chains(&effects)?;
        if effects
            .values()
            .any(|effect| effect.last_sequence > sequence)
        {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        Ok(Self { effects, sequence })
    }

    pub(super) fn sequence(&self) -> u64 {
        self.sequence
    }
}

/// V2 sequence numbers remain monotonic across generation boundaries.
/// The generation reader supplies the previous verified journal cursor.
pub(crate) fn apply_event(
    replay: &mut ManagementEffectReplayV2,
    sequence: u64,
    event: &ManagementEffectEventV2,
    evidence: &impl CredentialEvidenceVerifierV1,
) -> Result<(), ManagementEffectStoreError> {
    if replay.sequence.checked_add(1) != Some(sequence) {
        return Err(ManagementEffectStoreError::CorruptJournal);
    }
    apply_transition(&mut replay.effects, sequence, event, evidence)?;
    replay.sequence = sequence;
    Ok(())
}

fn apply_transition(
    effects: &mut BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV2>,
    sequence: u64,
    event: &ManagementEffectEventV2,
    evidence: &impl CredentialEvidenceVerifierV1,
) -> Result<(), ManagementEffectStoreError> {
    if sequence == 0 {
        return Err(ManagementEffectStoreError::CorruptJournal);
    }
    let id = event.operation_id();
    if let ManagementEffectEventV2::Prepared { request } = event {
        request.validate_new(request.base.scope.project)?;
        if effects.contains_key(&id) {
            return Err(ManagementEffectStoreError::InvalidTransition);
        }
        let authorization = request.credential.as_ref().map(|c| c.authorization.clone());
        effects.insert(
            id,
            ManagementEffectSnapshotV2 {
                request: request.clone(),
                state: ManagementEffectStateV2::Prepared,
                last_sequence: sequence,
                credential_authorization: authorization,
                unknown_reason: None,
            },
        );
        if let Err(error) = validate_recovery_chains(effects) {
            effects.remove(&id);
            return Err(error);
        }
        return Ok(());
    }
    let old = effects
        .get(&id)
        .ok_or(ManagementEffectStoreError::InvalidTransition)?;
    if old.state.is_terminal() || sequence <= old.last_sequence {
        return Err(ManagementEffectStoreError::InvalidTransition);
    }
    let credential = old.credential_authorization.is_some();
    let legacy = matches!(
        old.credential_authorization,
        Some(CredentialMutationAuthorizationV1::LegacyV1 { .. })
    );
    if legacy
        && !matches!(
            event,
            ManagementEffectEventV2::ReconciledSupersededV1 { .. }
        )
    {
        return Err(ManagementEffectStoreError::InvalidTransition);
    }
    let mut next = old.clone();
    match event {
        ManagementEffectEventV2::Prepared { .. } => unreachable!(),
        ManagementEffectEventV2::EffectStarted { .. } => {
            if !matches!(old.state, ManagementEffectStateV2::Prepared) {
                return Err(ManagementEffectStoreError::InvalidTransition);
            }
            next.state = ManagementEffectStateV2::EffectStarted;
        }
        ManagementEffectEventV2::CredentialAuthorizationArmedV1 {
            ledger_epoch_id,
            reserved_entry_hash,
            ..
        } => {
            if !matches!(old.state, ManagementEffectStateV2::EffectStarted) {
                return Err(ManagementEffectStoreError::InvalidTransition);
            }
            let auth = old
                .credential_authorization
                .as_ref()
                .ok_or(ManagementEffectStoreError::InvalidTransition)?;
            let armed = auth.arm(*ledger_epoch_id, *reserved_entry_hash)?;
            evidence.reserved(old, *ledger_epoch_id, *reserved_entry_hash)?;
            next.credential_authorization = Some(armed);
        }
        ManagementEffectEventV2::OutcomeUnknown { reason, .. } => {
            if !matches!(old.state, ManagementEffectStateV2::EffectStarted)
                || *reason == ManagementUnknownReasonV2::LegacyAuthorizationUnavailable
            {
                return Err(ManagementEffectStoreError::InvalidTransition);
            }
            next.state = ManagementEffectStateV2::OutcomeUnknown { reason: *reason };
            next.unknown_reason = Some(*reason);
        }
        ManagementEffectEventV2::Denied { reason, .. } => {
            if !matches!(old.state, ManagementEffectStateV2::Prepared) {
                return Err(ManagementEffectStoreError::InvalidTransition);
            }
            if credential {
                evidence.zero_match(old)?;
            }
            next.state = ManagementEffectStateV2::Denied { reason: *reason };
        }
        ManagementEffectEventV2::Completed { evidence_hash, .. } => {
            if !matches!(
                old.state,
                ManagementEffectStateV2::EffectStarted
                    | ManagementEffectStateV2::OutcomeUnknown { .. }
            ) {
                return Err(ManagementEffectStoreError::InvalidTransition);
            }
            nonzero(*evidence_hash)?;
            if credential {
                if !matches!(
                    old.credential_authorization,
                    Some(CredentialMutationAuthorizationV1::Armed { .. })
                ) {
                    return Err(ManagementEffectStoreError::InvalidTransition);
                }
                evidence.completed(old, *evidence_hash)?;
            }
            next.state = ManagementEffectStateV2::Completed {
                evidence_hash: *evidence_hash,
            };
        }
        ManagementEffectEventV2::ReconciledAbandon { evidence_hash, .. } => {
            if !matches!(old.state, ManagementEffectStateV2::OutcomeUnknown { .. }) {
                return Err(ManagementEffectStoreError::InvalidTransition);
            }
            nonzero(*evidence_hash)?;
            if credential {
                if matches!(
                    old.credential_authorization,
                    Some(CredentialMutationAuthorizationV1::Unarmed)
                ) {
                    evidence.zero_match(old)?;
                }
                evidence.abandoned(old, *evidence_hash)?;
            }
            next.state = ManagementEffectStateV2::ReconciledAbandon {
                evidence_hash: *evidence_hash,
            };
        }
        ManagementEffectEventV2::ReconciledSupersededV1 {
            resolution_operation_id,
            certificate_id,
            ..
        } => {
            if !matches!(old.state, ManagementEffectStateV2::OutcomeUnknown { .. }) || !credential {
                return Err(ManagementEffectStoreError::InvalidTransition);
            }
            nonzero(*certificate_id)?;
            let resolver = effects
                .get(resolution_operation_id)
                .ok_or(ManagementEffectStoreError::InvalidTransition)?;
            if !matches!(resolver.state, ManagementEffectStateV2::Completed { .. }) {
                return Err(ManagementEffectStoreError::InvalidTransition);
            }
            validate_recovery_chains(effects)?;
            let mut link = *resolution_operation_id;
            let mut found = false;
            for _ in 0..MAX_CHAIN_DEPTH {
                let effect = effects
                    .get(&link)
                    .ok_or(ManagementEffectStoreError::InvalidTransition)?;
                let Some(parent) = effect
                    .request
                    .credential
                    .as_ref()
                    .and_then(|ext| ext.predecessor_operation_id)
                else {
                    break;
                };
                if parent == id {
                    found = true;
                    break;
                }
                if !matches!(effects.get(&parent).map(|effect| &effect.state),
                    Some(ManagementEffectStateV2::ReconciledSupersededV1 {
                        resolution_operation_id: prior_resolver, ..
                    }) if prior_resolver == resolution_operation_id)
                {
                    return Err(ManagementEffectStoreError::InvalidTransition);
                }
                link = parent;
            }
            if !found {
                return Err(ManagementEffectStoreError::InvalidTransition);
            }
            evidence.superseded(old, resolver, effects, *certificate_id)?;
            next.state = ManagementEffectStateV2::ReconciledSupersededV1 {
                resolution_operation_id: *resolution_operation_id,
                certificate_id: *certificate_id,
            };
        }
    }
    next.last_sequence = sequence;
    validate_snapshot(&next)?;
    let original = effects.insert(id, next).expect("existing transition");
    if let Err(error) = validate_recovery_chains(effects) {
        effects.insert(id, original);
        return Err(error);
    }
    Ok(())
}
