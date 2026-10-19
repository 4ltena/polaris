//! Version-two management contracts. Durable publication is owned by the
//! generation/store migration, and global proofs by the MCP coordinator.
use super::*;
use crate::mcp::credentials::McpSecretReferenceV1;
use std::collections::{BTreeMap, BTreeSet};

mod reducer;
#[cfg(unix)]
pub use super::store::generation::{
    CurrentV2, ManagementMigrationAnchorV2, ManagementMigrationAnchorsV2,
    ManagementMigrationTreeV2, ManifestV2, MigrationManifestV2,
};
pub use reducer::ManagementEffectEventV2;

const MAX_REFERENCES: usize = 1024;
const MAX_CHAIN_DEPTH: u8 = 8;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialMutationKindV1 {
    Set,
    Delete,
    SetupBatch,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LegacyCredentialDescriptorV1 {
    pub store_identity: ManagementEffectHashV1,
    pub origin_project_identity: ManagementEffectHashV1,
    pub inventory: ManagementInventoryIdV1,
    pub kind: CredentialMutationKindV1,
    #[serde(with = "references")]
    pub references: Vec<McpSecretReferenceV1>,
    pub reference_set_hash: ManagementEffectHashV1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "authorization", rename_all = "snake_case", deny_unknown_fields)]
pub enum CredentialMutationAuthorizationV1 {
    LegacyV1 {
        source_effect_hash: ManagementEffectHashV1,
        verified_descriptor: Option<LegacyCredentialDescriptorV1>,
    },
    Unarmed,
    Armed {
        ledger_epoch_id: ManagementEffectHashV1,
        reserved_entry_hash: ManagementEffectHashV1,
    },
}

impl CredentialMutationAuthorizationV1 {
    pub fn arm(
        &self,
        ledger_epoch_id: ManagementEffectHashV1,
        reserved_entry_hash: ManagementEffectHashV1,
    ) -> Result<Self, ManagementEffectStoreError> {
        if !matches!(self, Self::Unarmed) {
            return Err(ManagementEffectStoreError::InvalidTransition);
        }
        nonzero(ledger_epoch_id)?;
        nonzero(reserved_entry_hash)?;
        Ok(Self::Armed {
            ledger_epoch_id,
            reserved_entry_hash,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialManagementExtensionV1 {
    pub store_identity: ManagementEffectHashV1,
    pub kind: CredentialMutationKindV1,
    pub reference_set_hash: ManagementEffectHashV1,
    pub expected_revision: u64,
    pub before_token: Option<ManagementEffectHashV1>,
    pub before_receipt_id: Option<ManagementEffectHashV1>,
    pub root_operation_id: ManagementOperationIdV1,
    pub predecessor_operation_id: Option<ManagementOperationIdV1>,
    pub chain_depth: u8,
    pub credential_plan_hash: ManagementEffectHashV1,
    pub planned_credential_steps: u8,
    pub authorization: CredentialMutationAuthorizationV1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareManagementEffectV2 {
    pub base: PrepareManagementEffectV1,
    pub credential: Option<CredentialManagementExtensionV1>,
    #[serde(with = "references")]
    pub credential_references: Vec<McpSecretReferenceV1>,
}

impl PrepareManagementEffectV2 {
    pub fn validate_new(
        &self,
        project: ManagementEffectHashV1,
    ) -> Result<(), ManagementEffectStoreError> {
        super::store::validate_request_shape(project, &self.base)?;
        super::store::validate_new_inventory_contract(&self.base)?;
        let inventory = self.base.inventory.encoded();
        match &self.credential {
            None if !credential_inventory(&inventory) && self.credential_references.is_empty() => {}
            Some(extension) => {
                if !kind_matches_inventory(extension.kind, &inventory)
                    || !matches!(
                        extension.authorization,
                        CredentialMutationAuthorizationV1::Unarmed
                    )
                    || extension.before_token.is_none()
                    || extension.before_receipt_id.is_none()
                    || !(1..=MAX_CHAIN_DEPTH).contains(&extension.chain_depth)
                    || !(1..=2).contains(&extension.planned_credential_steps)
                    || (extension.kind != CredentialMutationKindV1::SetupBatch
                        && (extension.planned_credential_steps != 1
                            || self.credential_references.len() != 1))
                    || (extension.predecessor_operation_id.is_none()
                        && (extension.root_operation_id != self.base.operation_id
                            || extension.chain_depth != 1))
                    || (extension.predecessor_operation_id.is_some() && extension.chain_depth == 1)
                    || extension.root_operation_id.0.0 == [0; ID_BYTES]
                    || extension.predecessor_operation_id == Some(self.base.operation_id)
                {
                    return Err(ManagementEffectStoreError::InvalidInventoryContract);
                }
                nonzero(extension.store_identity)?;
                nonzero(extension.reference_set_hash)?;
                nonzero(extension.credential_plan_hash)?;
                nonzero(extension.before_token.expect("checked"))?;
                nonzero(extension.before_receipt_id.expect("checked"))?;
                if reference_set_hash(&self.credential_references)? != extension.reference_set_hash
                {
                    return Err(ManagementEffectStoreError::InvalidValue);
                }
            }
            _ => return Err(ManagementEffectStoreError::InvalidInventoryContract),
        }
        if serde_json::to_vec(self)?.len() > ManagementEffectLimitsV1::default().max_record_bytes {
            return Err(ManagementEffectStoreError::StoreFull);
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagementUnknownReasonV2 {
    ProcessRestart,
    InterruptedAfterStart,
    ReceiptPersistenceFailed,
    DetachedEffect,
    UnclassifiedEffect,
    LegacyAuthorizationUnavailable,
}

impl From<ManagementUnknownReasonV1> for ManagementUnknownReasonV2 {
    fn from(value: ManagementUnknownReasonV1) -> Self {
        match value {
            ManagementUnknownReasonV1::ProcessRestart => Self::ProcessRestart,
            ManagementUnknownReasonV1::InterruptedAfterStart => Self::InterruptedAfterStart,
            ManagementUnknownReasonV1::ReceiptPersistenceFailed => Self::ReceiptPersistenceFailed,
            ManagementUnknownReasonV1::DetachedEffect => Self::DetachedEffect,
            ManagementUnknownReasonV1::UnclassifiedEffect => Self::UnclassifiedEffect,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum ManagementEffectStateV2 {
    Prepared,
    EffectStarted,
    OutcomeUnknown {
        reason: ManagementUnknownReasonV2,
    },
    Completed {
        evidence_hash: ManagementEffectHashV1,
    },
    Denied {
        reason: ManagementDenialReasonV1,
    },
    ReconciledAbandon {
        evidence_hash: ManagementEffectHashV1,
    },
    ReconciledSupersededV1 {
        resolution_operation_id: ManagementOperationIdV1,
        certificate_id: ManagementEffectHashV1,
    },
}

impl ManagementEffectStateV2 {
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            Self::Completed { .. }
                | Self::Denied { .. }
                | Self::ReconciledAbandon { .. }
                | Self::ReconciledSupersededV1 { .. }
        )
    }

    pub fn blocks_new_work(&self) -> bool {
        !self.is_terminal()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagementEffectSnapshotV2 {
    pub request: PrepareManagementEffectV2,
    pub state: ManagementEffectStateV2,
    pub last_sequence: u64,
    pub credential_authorization: Option<CredentialMutationAuthorizationV1>,
    pub unknown_reason: Option<ManagementUnknownReasonV2>,
}

impl ManagementEffectSnapshotV2 {
    /// Current v1 records lack a credential descriptor. Migration keeps their
    /// source authorization without inventing a store, reference set or plan.
    pub(crate) fn from_legacy(
        source: &ManagementEffectSnapshotV1,
    ) -> Result<Self, ManagementEffectStoreError> {
        let credential = credential_inventory(&source.request.inventory.encoded());
        let authorization = if credential {
            Some(CredentialMutationAuthorizationV1::LegacyV1 {
                source_effect_hash: ManagementEffectHashV1::digest(&serde_json::to_vec(source)?),
                verified_descriptor: None,
            })
        } else {
            None
        };
        let state = match source.state {
            ManagementEffectStateV1::Prepared if credential => {
                ManagementEffectStateV2::OutcomeUnknown {
                    reason: ManagementUnknownReasonV2::LegacyAuthorizationUnavailable,
                }
            }
            ManagementEffectStateV1::Prepared => ManagementEffectStateV2::Prepared,
            ManagementEffectStateV1::EffectStarted if credential => {
                ManagementEffectStateV2::OutcomeUnknown {
                    reason: ManagementUnknownReasonV2::LegacyAuthorizationUnavailable,
                }
            }
            ManagementEffectStateV1::EffectStarted => ManagementEffectStateV2::EffectStarted,
            ManagementEffectStateV1::OutcomeUnknown { .. } if credential => {
                ManagementEffectStateV2::OutcomeUnknown {
                    reason: ManagementUnknownReasonV2::LegacyAuthorizationUnavailable,
                }
            }
            ManagementEffectStateV1::OutcomeUnknown { reason } => {
                ManagementEffectStateV2::OutcomeUnknown {
                    reason: reason.into(),
                }
            }
            ManagementEffectStateV1::Completed { evidence_hash } => {
                ManagementEffectStateV2::Completed { evidence_hash }
            }
            ManagementEffectStateV1::Denied { reason } => {
                ManagementEffectStateV2::Denied { reason }
            }
            ManagementEffectStateV1::ReconciledAbandon { evidence_hash } => {
                ManagementEffectStateV2::ReconciledAbandon { evidence_hash }
            }
        };
        let unknown_reason = match state {
            ManagementEffectStateV2::OutcomeUnknown { reason } => Some(reason),
            _ => None,
        };
        Ok(Self {
            request: PrepareManagementEffectV2 {
                base: source.request.clone(),
                credential: None,
                credential_references: Vec::new(),
            },
            state,
            last_sequence: source.last_sequence,
            credential_authorization: authorization,
            unknown_reason,
        })
    }
}

/// Validates the complete retained chain, including archived terminal effects.
/// This is a contract check, not evidence that the global ledger was read.
pub fn validate_recovery_chains(
    effects: &BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV2>,
) -> Result<(), ManagementEffectStoreError> {
    let mut successors = BTreeSet::new();
    for (id, effect) in effects {
        if *id != effect.request.base.operation_id {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        validate_snapshot(effect)?;
        let Some(extension) = &effect.request.credential else {
            continue;
        };
        let Some(predecessor) = extension.predecessor_operation_id else {
            continue;
        };
        if !successors.insert(predecessor) {
            return Err(ManagementEffectStoreError::InvalidTransition);
        }
        let prior = effects
            .get(&predecessor)
            .ok_or(ManagementEffectStoreError::InvalidTransition)?;
        if !matches!(
            prior.state,
            ManagementEffectStateV2::OutcomeUnknown { .. }
                | ManagementEffectStateV2::ReconciledSupersededV1 { .. }
        ) {
            return Err(ManagementEffectStoreError::InvalidTransition);
        }
        if prior.request.base.scope.project != effect.request.base.scope.project {
            return Err(ManagementEffectStoreError::WrongProjectScope);
        }
        if let Some(before) = &prior.request.credential {
            if extension.root_operation_id != before.root_operation_id
                || extension.chain_depth
                    != before
                        .chain_depth
                        .checked_add(1)
                        .ok_or(ManagementEffectStoreError::InvalidTransition)?
                || extension.kind != before.kind
                || extension.store_identity != before.store_identity
                || extension.reference_set_hash != before.reference_set_hash
                || effect.request.credential_references != prior.request.credential_references
            {
                return Err(ManagementEffectStoreError::InvalidTransition);
            }
        } else if let Some(CredentialMutationAuthorizationV1::LegacyV1 {
            verified_descriptor: Some(descriptor),
            ..
        }) = &prior.credential_authorization
        {
            if extension.root_operation_id != predecessor
                || extension.chain_depth != 2
                || descriptor.store_identity != extension.store_identity
                || descriptor.origin_project_identity != effect.request.base.scope.project
                || descriptor.kind != extension.kind
                || descriptor.reference_set_hash != extension.reference_set_hash
                || descriptor.references != effect.request.credential_references
                || reference_set_hash(&descriptor.references)? != descriptor.reference_set_hash
            {
                return Err(ManagementEffectStoreError::InvalidTransition);
            }
        } else {
            return Err(ManagementEffectStoreError::InvalidTransition);
        }
    }

    for (id, effect) in effects {
        let ManagementEffectStateV2::ReconciledSupersededV1 {
            resolution_operation_id,
            certificate_id,
        } = effect.state
        else {
            continue;
        };
        nonzero(certificate_id)?;
        if resolution_operation_id == *id {
            return Err(ManagementEffectStoreError::InvalidTransition);
        }
        let resolver = effects
            .get(&resolution_operation_id)
            .ok_or(ManagementEffectStoreError::InvalidTransition)?;
        if !matches!(resolver.state, ManagementEffectStateV2::Completed { .. })
            || !matches!(
                resolver.credential_authorization,
                Some(CredentialMutationAuthorizationV1::Armed { .. })
            )
        {
            return Err(ManagementEffectStoreError::InvalidTransition);
        }
        let mut link = resolution_operation_id;
        let mut found = false;
        for _ in 0..MAX_CHAIN_DEPTH {
            let current = effects
                .get(&link)
                .ok_or(ManagementEffectStoreError::InvalidTransition)?;
            let Some(parent) = current
                .request
                .credential
                .as_ref()
                .and_then(|extension| extension.predecessor_operation_id)
            else {
                break;
            };
            if parent == *id {
                found = true;
                break;
            }
            if !matches!(effects.get(&parent).map(|prior| &prior.state),
                Some(ManagementEffectStateV2::ReconciledSupersededV1 {
                    resolution_operation_id: prior_resolver, ..
                }) if *prior_resolver == resolution_operation_id)
            {
                return Err(ManagementEffectStoreError::InvalidTransition);
            }
            link = parent;
        }
        if !found {
            return Err(ManagementEffectStoreError::InvalidTransition);
        }
    }
    Ok(())
}

fn validate_snapshot(
    effect: &ManagementEffectSnapshotV2,
) -> Result<(), ManagementEffectStoreError> {
    if effect.last_sequence == 0 {
        return Err(ManagementEffectStoreError::CorruptJournal);
    }
    match effect.state {
        ManagementEffectStateV2::Completed { evidence_hash }
        | ManagementEffectStateV2::ReconciledAbandon { evidence_hash } => nonzero(evidence_hash)?,
        ManagementEffectStateV2::ReconciledSupersededV1 { certificate_id, .. } => {
            nonzero(certificate_id)?;
            if effect.unknown_reason.is_none() {
                return Err(ManagementEffectStoreError::CorruptJournal);
            }
        }
        ManagementEffectStateV2::OutcomeUnknown { reason }
            if effect.unknown_reason != Some(reason) =>
        {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        _ => {}
    }
    let inventory = effect.request.base.inventory.encoded();
    super::store::validate_request_shape(effect.request.base.scope.project, &effect.request.base)?;
    if effect.request.credential.is_some() {
        effect
            .request
            .validate_new(effect.request.base.scope.project)?;
        match &effect.credential_authorization {
            Some(CredentialMutationAuthorizationV1::Unarmed) => {
                if matches!(
                    effect.state,
                    ManagementEffectStateV2::Completed { .. }
                        | ManagementEffectStateV2::ReconciledSupersededV1 { .. }
                ) {
                    return Err(ManagementEffectStoreError::InvalidTransition);
                }
            }
            Some(CredentialMutationAuthorizationV1::Armed {
                ledger_epoch_id,
                reserved_entry_hash,
            }) => {
                nonzero(*ledger_epoch_id)?;
                nonzero(*reserved_entry_hash)?;
                if matches!(
                    effect.state,
                    ManagementEffectStateV2::Prepared | ManagementEffectStateV2::Denied { .. }
                ) {
                    return Err(ManagementEffectStoreError::InvalidTransition);
                }
            }
            _ => return Err(ManagementEffectStoreError::InvalidTransition),
        }
    } else if credential_inventory(&inventory) {
        let Some(CredentialMutationAuthorizationV1::LegacyV1 {
            source_effect_hash,
            verified_descriptor,
        }) = &effect.credential_authorization
        else {
            return Err(ManagementEffectStoreError::InvalidTransition);
        };
        nonzero(*source_effect_hash)?;
        if !effect.request.credential_references.is_empty()
            || matches!(
                effect.state,
                ManagementEffectStateV2::Prepared | ManagementEffectStateV2::EffectStarted
            )
        {
            return Err(ManagementEffectStoreError::InvalidTransition);
        }
        if let Some(descriptor) = verified_descriptor {
            nonzero(descriptor.store_identity)?;
            if effect.request.base.rewind != ManagementRewindDispositionV1::IrreversibleLocal
                || descriptor.origin_project_identity != effect.request.base.scope.project
                || descriptor.inventory != effect.request.base.inventory
                || !legacy_kind_matches_inventory(descriptor.kind, &inventory)
                || reference_set_hash(&descriptor.references)? != descriptor.reference_set_hash
            {
                return Err(ManagementEffectStoreError::InvalidInventoryContract);
            }
        } else if matches!(
            effect.state,
            ManagementEffectStateV2::ReconciledSupersededV1 { .. }
        ) {
            return Err(ManagementEffectStoreError::InvalidTransition);
        }
    } else if effect.credential_authorization.is_some()
        || !effect.request.credential_references.is_empty()
    {
        return Err(ManagementEffectStoreError::InvalidInventoryContract);
    }
    Ok(())
}

pub fn reference_set_hash(
    values: &[McpSecretReferenceV1],
) -> Result<ManagementEffectHashV1, ManagementEffectStoreError> {
    if values.is_empty()
        || values.len() > MAX_REFERENCES
        || values
            .windows(2)
            .any(|pair| pair[0].as_str() >= pair[1].as_str())
    {
        return Err(ManagementEffectStoreError::InvalidValue);
    }
    let names: Vec<_> = values.iter().map(McpSecretReferenceV1::as_str).collect();
    let mut encoded = b"polaris-mcp-reference-set-v1\0".to_vec();
    encoded.extend_from_slice(&serde_json::to_vec(&names)?);
    Ok(ManagementEffectHashV1::digest(&encoded))
}

fn nonzero(value: ManagementEffectHashV1) -> Result<(), ManagementEffectStoreError> {
    if value == ManagementEffectHashV1::zero() {
        return Err(ManagementEffectStoreError::InvalidValue);
    }
    Ok(())
}

fn credential_inventory(value: &str) -> bool {
    matches!(value, "C24" | "C25" | "C27" | "D32" | "D33" | "D37")
}

fn kind_matches_inventory(kind: CredentialMutationKindV1, inventory: &str) -> bool {
    matches!(
        (kind, inventory),
        (CredentialMutationKindV1::Set, "C24")
            | (CredentialMutationKindV1::Delete, "C25")
            | (CredentialMutationKindV1::SetupBatch, "C27" | "D37")
    )
}

fn legacy_kind_matches_inventory(kind: CredentialMutationKindV1, inventory: &str) -> bool {
    kind_matches_inventory(kind, inventory)
        || matches!(
            (kind, inventory),
            (CredentialMutationKindV1::Set, "D32") | (CredentialMutationKindV1::Delete, "D33")
        )
}

mod references {
    use super::*;
    pub fn serialize<S>(values: &[McpSecretReferenceV1], serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let names: Vec<_> = values.iter().map(McpSecretReferenceV1::as_str).collect();
        names.serialize(serializer)
    }

    pub fn deserialize<'de, D>(deserializer: D) -> Result<Vec<McpSecretReferenceV1>, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let names = Vec::<String>::deserialize(deserializer)?;
        if names.len() > MAX_REFERENCES {
            return Err(serde::de::Error::custom("MCP reference count exceeded"));
        }
        names
            .into_iter()
            .map(|name| McpSecretReferenceV1::parse(name).map_err(serde::de::Error::custom))
            .collect()
    }
}

#[cfg(test)]
mod tests;
