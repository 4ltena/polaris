//! Management effect v2 request, snapshot and successor validation tests.
use super::*;

fn hash(n: u8) -> ManagementEffectHashV1 {
    ManagementEffectHashV1::digest(&[n])
}
fn id(n: u8) -> ManagementOperationIdV1 {
    format!("{n:032x}").parse().unwrap()
}
fn request(n: u8) -> PrepareManagementEffectV2 {
    let refs = vec![McpSecretReferenceV1::parse("account").unwrap()];
    PrepareManagementEffectV2 {
        base: PrepareManagementEffectV1 {
            operation_id: id(n),
            inventory: ManagementInventoryIdV1::parse("C24").unwrap(),
            scope: ManagementEffectScopeV1 {
                project: hash(1),
                session: None,
                generation: 0,
                revision: 0,
                permission: ManagementPermissionScopeV1::System,
                account_binding: None,
            },
            request_hash: hash(2),
            rewind: ManagementRewindDispositionV1::IrreversibleLocal,
        },
        credential: Some(CredentialManagementExtensionV1 {
            store_identity: hash(3),
            kind: CredentialMutationKindV1::Set,
            reference_set_hash: reference_set_hash(&refs).unwrap(),
            expected_revision: 0,
            before_token: Some(hash(4)),
            before_receipt_id: Some(hash(5)),
            root_operation_id: id(n),
            predecessor_operation_id: None,
            chain_depth: 1,
            credential_plan_hash: hash(6),
            planned_credential_steps: 1,
            authorization: CredentialMutationAuthorizationV1::Unarmed,
        }),
        credential_references: refs,
    }
}
fn snapshot(request: PrepareManagementEffectV2) -> ManagementEffectSnapshotV2 {
    ManagementEffectSnapshotV2 {
        request,
        state: ManagementEffectStateV2::OutcomeUnknown {
            reason: ManagementUnknownReasonV2::ProcessRestart,
        },
        last_sequence: 3,
        credential_authorization: Some(CredentialMutationAuthorizationV1::Armed {
            ledger_epoch_id: hash(7),
            reserved_entry_hash: hash(8),
        }),
        unknown_reason: Some(ManagementUnknownReasonV2::ProcessRestart),
    }
}
fn successor(n: u8, predecessor: u8) -> ManagementEffectSnapshotV2 {
    let mut next = request(n);
    let extension = next.credential.as_mut().unwrap();
    extension.root_operation_id = id(1);
    extension.predecessor_operation_id = Some(id(predecessor));
    extension.chain_depth = n;
    snapshot(next)
}

#[test]
fn prepared_event_preserves_the_existing_json_bytes() {
    let request = request(1);
    let expected = format!(
        "{{\"event\":\"prepared\",\"request\":{}}}",
        serde_json::to_string(&request).unwrap()
    );
    let event = ManagementEffectEventV2::Prepared {
        request: Box::new(request),
    };
    assert_eq!(serde_json::to_string(&event).unwrap(), expected);
    assert_eq!(
        serde_json::from_str::<ManagementEffectEventV2>(&expected).unwrap(),
        event
    );
}

#[test]
fn current_request_binds_canonical_references_and_prepared_unarmed() {
    let valid = request(1);
    valid.validate_new(hash(1)).unwrap();
    let bytes = serde_json::to_vec(&valid).unwrap();
    let restored: PrepareManagementEffectV2 = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(restored, valid);
    for changed in 0..5 {
        let mut bad = valid.clone();
        let extension = bad.credential.as_mut().unwrap();
        match changed {
            0 => extension.reference_set_hash = hash(99),
            1 => {
                extension.authorization = CredentialMutationAuthorizationV1::Armed {
                    ledger_epoch_id: hash(7),
                    reserved_entry_hash: hash(8),
                }
            }
            2 => extension.before_token = None,
            3 => extension.planned_credential_steps = 2,
            _ => bad.base.inventory = ManagementInventoryIdV1::parse("D32").unwrap(),
        }
        assert!(
            bad.validate_new(hash(1)).is_err(),
            "changed field {changed}"
        );
    }
    let mut omitted = valid;
    omitted.credential = None;
    assert!(omitted.validate_new(hash(1)).is_err());
}

#[test]
fn authorization_arm_is_one_way_and_legacy_cannot_be_armed() {
    let unarmed = CredentialMutationAuthorizationV1::Unarmed;
    let armed = unarmed.arm(hash(7), hash(8)).unwrap();
    assert_eq!(
        armed,
        CredentialMutationAuthorizationV1::Armed {
            ledger_epoch_id: hash(7),
            reserved_entry_hash: hash(8),
        }
    );
    assert!(armed.arm(hash(7), hash(8)).is_err());
    assert!(
        unarmed
            .arm(ManagementEffectHashV1::zero(), hash(8))
            .is_err()
    );
    let legacy = CredentialMutationAuthorizationV1::LegacyV1 {
        source_effect_hash: hash(9),
        verified_descriptor: None,
    };
    assert!(legacy.arm(hash(7), hash(8)).is_err());
}

#[test]
fn eight_operation_chain_passes_and_fanout_cross_project_cycle_ninth_fail() {
    let mut chain = BTreeMap::new();
    chain.insert(id(1), snapshot(request(1)));
    for n in 2..=8 {
        chain.insert(id(n), successor(n, n - 1));
    }
    validate_recovery_chains(&chain).unwrap();
    let mut fanout = chain.clone();
    let mut next = successor(9, 1);
    next.request.credential.as_mut().unwrap().chain_depth = 2;
    fanout.insert(id(9), next);
    assert!(validate_recovery_chains(&fanout).is_err());
    let mut foreign = chain.clone();
    foreign.get_mut(&id(2)).unwrap().request.base.scope.project = hash(100);
    assert!(validate_recovery_chains(&foreign).is_err());
    let mut cycle = chain.clone();
    cycle
        .get_mut(&id(1))
        .unwrap()
        .request
        .credential
        .as_mut()
        .unwrap()
        .predecessor_operation_id = Some(id(8));
    assert!(validate_recovery_chains(&cycle).is_err());
    let mut ninth = chain;
    ninth.insert(id(9), successor(9, 8));
    assert!(validate_recovery_chains(&ninth).is_err());
}

#[test]
fn opaque_legacy_remains_unknown_and_terminal_authorization_is_retained() {
    let legacy = ManagementEffectSnapshotV1 {
        request: request(1).base,
        state: ManagementEffectStateV1::Prepared,
        last_sequence: 1,
    };
    let migrated = ManagementEffectSnapshotV2::from_legacy(&legacy).unwrap();
    assert!(matches!(
        migrated.state,
        ManagementEffectStateV2::OutcomeUnknown {
            reason: ManagementUnknownReasonV2::LegacyAuthorizationUnavailable,
        }
    ));
    assert!(matches!(
        migrated.credential_authorization,
        Some(CredentialMutationAuthorizationV1::LegacyV1 {
            verified_descriptor: None,
            ..
        })
    ));
    assert!(migrated.request.credential.is_none());
    let mut opaque_superseded = migrated.clone();
    opaque_superseded.state = ManagementEffectStateV2::ReconciledSupersededV1 {
        resolution_operation_id: id(2),
        certificate_id: hash(20),
    };
    assert!(validate_recovery_chains(&BTreeMap::from([(id(1), opaque_superseded)])).is_err());
    let mut chain = BTreeMap::from([(id(1), migrated)]);
    let mut replacement = successor(2, 1);
    replacement.request.credential.as_mut().unwrap().chain_depth = 2;
    chain.insert(id(2), replacement);
    assert!(validate_recovery_chains(&chain).is_err());

    let completed = ManagementEffectSnapshotV1 {
        state: ManagementEffectStateV1::Completed {
            evidence_hash: hash(10),
        },
        ..legacy
    };
    let migrated = ManagementEffectSnapshotV2::from_legacy(&completed).unwrap();
    assert_eq!(
        migrated.state,
        ManagementEffectStateV2::Completed {
            evidence_hash: hash(10)
        }
    );
    assert!(migrated.state.is_terminal());
    assert!(matches!(
        migrated.credential_authorization,
        Some(CredentialMutationAuthorizationV1::LegacyV1 {
            verified_descriptor: None,
            ..
        })
    ));
}

#[test]
fn verified_legacy_allows_only_exact_explicit_replacement_descriptor() {
    let legacy = ManagementEffectSnapshotV1 {
        request: request(1).base,
        state: ManagementEffectStateV1::EffectStarted,
        last_sequence: 2,
    };
    let mut migrated = ManagementEffectSnapshotV2::from_legacy(&legacy).unwrap();
    let next = successor(2, 1);
    let ext = next.request.credential.as_ref().unwrap();
    let descriptor = LegacyCredentialDescriptorV1 {
        store_identity: ext.store_identity,
        origin_project_identity: hash(1),
        inventory: legacy.request.inventory,
        kind: ext.kind,
        references: next.request.credential_references.clone(),
        reference_set_hash: ext.reference_set_hash,
    };
    if let Some(CredentialMutationAuthorizationV1::LegacyV1 {
        verified_descriptor,
        ..
    }) = &mut migrated.credential_authorization
    {
        *verified_descriptor = Some(descriptor);
    } else {
        panic!("migration must preserve legacy authorization");
    }
    let valid = BTreeMap::from([(id(1), migrated), (id(2), next)]);
    validate_recovery_chains(&valid).unwrap();
    let mut restorable = valid.clone();
    restorable.get_mut(&id(1)).unwrap().request.base.rewind =
        ManagementRewindDispositionV1::CapturedRestorable;
    assert!(validate_recovery_chains(&restorable).is_err());
    for changed in 0..4 {
        let mut invalid = valid.clone();
        let Some(CredentialMutationAuthorizationV1::LegacyV1 {
            verified_descriptor: Some(descriptor),
            ..
        }) = &mut invalid.get_mut(&id(1)).unwrap().credential_authorization
        else {
            panic!("expected verified source");
        };
        match changed {
            0 => descriptor.store_identity = hash(100),
            1 => descriptor.reference_set_hash = hash(100),
            2 => descriptor.origin_project_identity = hash(100),
            _ => descriptor.kind = CredentialMutationKindV1::Delete,
        }
        assert!(validate_recovery_chains(&invalid).is_err());
    }
}

#[test]
fn serialized_contract_rejects_invalid_reference_and_unknown_field() {
    let request = request(1);
    let mut invalid = serde_json::to_value(&request).unwrap();
    invalid["credential_references"] = serde_json::json!(["bad reference"]);
    assert!(serde_json::from_value::<PrepareManagementEffectV2>(invalid).is_err());
    let mut invalid = serde_json::to_value(&request).unwrap();
    invalid["unapproved_fallback"] = serde_json::json!(true);
    assert!(serde_json::from_value::<PrepareManagementEffectV2>(invalid).is_err());
    let refs = vec![
        McpSecretReferenceV1::parse("two").unwrap(),
        McpSecretReferenceV1::parse("one").unwrap(),
    ];
    assert!(reference_set_hash(&refs).is_err());
}

#[derive(Default)]
struct Proofs {
    deny: bool,
}
impl reducer::CredentialEvidenceVerifierV1 for Proofs {
    fn reserved(
        &self,
        _: &ManagementEffectSnapshotV2,
        _: ManagementEffectHashV1,
        _: ManagementEffectHashV1,
    ) -> Result<(), ManagementEffectStoreError> {
        self.verify()
    }
    fn zero_match(&self, _: &ManagementEffectSnapshotV2) -> Result<(), ManagementEffectStoreError> {
        self.verify()
    }
    fn completed(
        &self,
        _: &ManagementEffectSnapshotV2,
        _: ManagementEffectHashV1,
    ) -> Result<(), ManagementEffectStoreError> {
        self.verify()
    }
    fn abandoned(
        &self,
        _: &ManagementEffectSnapshotV2,
        _: ManagementEffectHashV1,
    ) -> Result<(), ManagementEffectStoreError> {
        self.verify()
    }
    fn superseded(
        &self,
        _: &ManagementEffectSnapshotV2,
        _: &ManagementEffectSnapshotV2,
        _: &BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV2>,
        _: ManagementEffectHashV1,
    ) -> Result<(), ManagementEffectStoreError> {
        self.verify()
    }
}
impl Proofs {
    fn verify(&self) -> Result<(), ManagementEffectStoreError> {
        if self.deny {
            Err(ManagementEffectStoreError::InvalidTransition)
        } else {
            Ok(())
        }
    }
}
fn apply(
    effects: &mut BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV2>,
    sequence: u64,
    event: ManagementEffectEventV2,
    proofs: &Proofs,
) -> Result<(), ManagementEffectStoreError> {
    let previous = effects
        .values()
        .map(|effect| effect.last_sequence)
        .max()
        .unwrap_or(0);
    let mut replay = reducer::ManagementEffectReplayV2::from_checkpoint(effects.clone(), previous)?;
    reducer::apply_event(&mut replay, sequence, &event, proofs)?;
    *effects = replay.effects;
    Ok(())
}

#[test]
fn lifecycle_requires_reserved_and_receipt_proofs_and_retains_unknown_authorization() {
    let mut effects = BTreeMap::new();
    let good = Proofs::default();
    let bad = Proofs { deny: true };
    apply(
        &mut effects,
        1,
        ManagementEffectEventV2::Prepared {
            request: Box::new(request(1)),
        },
        &good,
    )
    .unwrap();
    assert_eq!(
        effects[&id(1)].credential_authorization,
        Some(CredentialMutationAuthorizationV1::Unarmed)
    );
    apply(
        &mut effects,
        2,
        ManagementEffectEventV2::EffectStarted {
            operation_id: id(1),
        },
        &good,
    )
    .unwrap();
    let arm = ManagementEffectEventV2::CredentialAuthorizationArmedV1 {
        operation_id: id(1),
        ledger_epoch_id: hash(7),
        reserved_entry_hash: hash(8),
    };
    let before = effects.clone();
    assert!(apply(&mut effects, 3, arm.clone(), &bad).is_err());
    assert_eq!(effects, before);
    assert!(
        apply(
            &mut effects,
            3,
            ManagementEffectEventV2::Completed {
                operation_id: id(1),
                evidence_hash: hash(10),
            },
            &good
        )
        .is_err()
    );
    apply(&mut effects, 3, arm.clone(), &good).unwrap();
    assert!(apply(&mut effects, 4, arm, &good).is_err());
    apply(
        &mut effects,
        4,
        ManagementEffectEventV2::OutcomeUnknown {
            operation_id: id(1),
            reason: ManagementUnknownReasonV2::ProcessRestart,
        },
        &good,
    )
    .unwrap();
    let before = effects.clone();
    let done = ManagementEffectEventV2::Completed {
        operation_id: id(1),
        evidence_hash: hash(10),
    };
    assert!(apply(&mut effects, 5, done.clone(), &bad).is_err());
    assert_eq!(effects, before);
    apply(&mut effects, 5, done, &good).unwrap();
    assert_eq!(
        effects[&id(1)].unknown_reason,
        Some(ManagementUnknownReasonV2::ProcessRestart)
    );
    assert!(matches!(
        effects[&id(1)].credential_authorization,
        Some(CredentialMutationAuthorizationV1::Armed { .. })
    ));
    assert!(effects[&id(1)].state.is_terminal());
    assert!(
        apply(
            &mut effects,
            6,
            ManagementEffectEventV2::EffectStarted {
                operation_id: id(1)
            },
            &good
        )
        .is_err()
    );
}

#[test]
fn unarmed_restart_zero_match_failure_preserves_prepared_and_unknown() {
    let good = Proofs::default();
    for _contradiction in ["matching", "duplicate", "same_id_mismatch", "unreadable"] {
        let bad = Proofs { deny: true };
        let mut effects = BTreeMap::new();
        apply(
            &mut effects,
            1,
            ManagementEffectEventV2::Prepared {
                request: Box::new(request(1)),
            },
            &good,
        )
        .unwrap();
        let before = effects.clone();
        let deny = ManagementEffectEventV2::Denied {
            operation_id: id(1),
            reason: ManagementDenialReasonV1::ResourceUnavailable,
        };
        assert!(apply(&mut effects, 2, deny.clone(), &bad).is_err());
        assert_eq!(effects, before);
        apply(&mut effects, 2, deny, &good).unwrap();
        assert!(matches!(
            effects[&id(1)].state,
            ManagementEffectStateV2::Denied { .. }
        ));

        let mut effects = BTreeMap::new();
        apply(
            &mut effects,
            1,
            ManagementEffectEventV2::Prepared {
                request: Box::new(request(2)),
            },
            &good,
        )
        .unwrap();
        apply(
            &mut effects,
            2,
            ManagementEffectEventV2::EffectStarted {
                operation_id: id(2),
            },
            &good,
        )
        .unwrap();
        let abandon = ManagementEffectEventV2::ReconciledAbandon {
            operation_id: id(2),
            evidence_hash: hash(11),
        };
        assert!(apply(&mut effects, 3, abandon.clone(), &good).is_err());
        apply(
            &mut effects,
            3,
            ManagementEffectEventV2::OutcomeUnknown {
                operation_id: id(2),
                reason: ManagementUnknownReasonV2::ProcessRestart,
            },
            &good,
        )
        .unwrap();
        let before = effects.clone();
        assert!(apply(&mut effects, 4, abandon.clone(), &bad).is_err());
        assert_eq!(effects, before);
        apply(&mut effects, 4, abandon, &good).unwrap();
        assert!(matches!(
            effects[&id(2)].state,
            ManagementEffectStateV2::ReconciledAbandon { .. }
        ));
    }
}

#[test]
fn supersession_closes_tail_to_root_with_exact_proofs_only() {
    let good = Proofs::default();
    let bad = Proofs { deny: true };
    let mut effects = BTreeMap::from([
        (id(1), snapshot(request(1))),
        (id(2), successor(2, 1)),
        (id(3), successor(3, 2)),
    ]);
    apply(
        &mut effects,
        4,
        ManagementEffectEventV2::Completed {
            operation_id: id(3),
            evidence_hash: hash(10),
        },
        &good,
    )
    .unwrap();
    let root = ManagementEffectEventV2::ReconciledSupersededV1 {
        operation_id: id(1),
        resolution_operation_id: id(3),
        certificate_id: hash(12),
    };
    assert!(apply(&mut effects, 5, root.clone(), &good).is_err());
    let tail = ManagementEffectEventV2::ReconciledSupersededV1 {
        operation_id: id(2),
        resolution_operation_id: id(3),
        certificate_id: hash(13),
    };
    let before = effects.clone();
    assert!(apply(&mut effects, 5, tail.clone(), &bad).is_err());
    assert_eq!(effects, before);
    apply(&mut effects, 5, tail, &good).unwrap();
    apply(&mut effects, 6, root.clone(), &good).unwrap();
    assert!(apply(&mut effects, 7, root, &good).is_err());
    validate_recovery_chains(&effects).unwrap();
    let mut lost_history = effects.clone();
    lost_history.get_mut(&id(1)).unwrap().unknown_reason = None;
    assert!(reducer::ManagementEffectReplayV2::from_checkpoint(lost_history, 6).is_err());
    for n in [1, 2] {
        assert!(effects[&id(n)].state.is_terminal());
        assert_eq!(
            effects[&id(n)].unknown_reason,
            Some(ManagementUnknownReasonV2::ProcessRestart)
        );
        assert!(matches!(
            effects[&id(n)].credential_authorization,
            Some(CredentialMutationAuthorizationV1::Armed { .. })
        ));
    }
}

#[test]
fn legacy_cannot_be_denied_abandoned_or_completed_by_live_replay() {
    let source = ManagementEffectSnapshotV1 {
        request: request(1).base,
        state: ManagementEffectStateV1::EffectStarted,
        last_sequence: 2,
    };
    let effect = ManagementEffectSnapshotV2::from_legacy(&source).unwrap();
    let mut effects = BTreeMap::from([(id(1), effect)]);
    let before = effects.clone();
    for event in [
        ManagementEffectEventV2::Denied {
            operation_id: id(1),
            reason: ManagementDenialReasonV1::UserDeclined,
        },
        ManagementEffectEventV2::ReconciledAbandon {
            operation_id: id(1),
            evidence_hash: hash(10),
        },
        ManagementEffectEventV2::Completed {
            operation_id: id(1),
            evidence_hash: hash(10),
        },
        ManagementEffectEventV2::CredentialAuthorizationArmedV1 {
            operation_id: id(1),
            ledger_epoch_id: hash(7),
            reserved_entry_hash: hash(8),
        },
    ] {
        assert!(apply(&mut effects, 3, event, &Proofs::default()).is_err());
        assert_eq!(effects, before);
    }
}

#[test]
fn global_cursor_rejects_gap_and_out_of_order_events_across_operations() {
    let proofs = Proofs::default();
    let mut replay = reducer::ManagementEffectReplayV2::default();
    reducer::apply_event(
        &mut replay,
        1,
        &ManagementEffectEventV2::Prepared {
            request: Box::new(request(1)),
        },
        &proofs,
    )
    .unwrap();
    let before = replay.clone();
    assert!(
        reducer::apply_event(
            &mut replay,
            3,
            &ManagementEffectEventV2::Prepared {
                request: Box::new(request(2)),
            },
            &proofs
        )
        .is_err()
    );
    assert_eq!(replay, before);
    reducer::apply_event(
        &mut replay,
        2,
        &ManagementEffectEventV2::Prepared {
            request: Box::new(request(2)),
        },
        &proofs,
    )
    .unwrap();
    let before = replay.clone();
    assert!(
        reducer::apply_event(
            &mut replay,
            2,
            &ManagementEffectEventV2::EffectStarted {
                operation_id: id(1),
            },
            &proofs
        )
        .is_err()
    );
    assert_eq!(replay, before);
    reducer::apply_event(
        &mut replay,
        3,
        &ManagementEffectEventV2::EffectStarted {
            operation_id: id(1),
        },
        &proofs,
    )
    .unwrap();
    assert_eq!(replay.sequence(), 3);
}

#[test]
fn terminal_transition_cannot_break_an_unresolved_successor_chain() {
    let good = Proofs::default();
    let mut effects = BTreeMap::from([(id(1), snapshot(request(1)))]);
    apply(
        &mut effects,
        4,
        ManagementEffectEventV2::Prepared {
            request: Box::new(successor(2, 1).request),
        },
        &good,
    )
    .unwrap();
    validate_recovery_chains(&effects).unwrap();
    let before = effects.clone();
    for event in [
        ManagementEffectEventV2::Completed {
            operation_id: id(1),
            evidence_hash: hash(10),
        },
        ManagementEffectEventV2::ReconciledAbandon {
            operation_id: id(1),
            evidence_hash: hash(11),
        },
    ] {
        assert!(apply(&mut effects, 5, event, &good).is_err());
        assert_eq!(effects, before);
    }
}

#[test]
fn checkpoint_rejects_missing_resolver_zero_evidence_and_inconsistent_history() {
    let valid = snapshot(request(1));
    reducer::ManagementEffectReplayV2::from_checkpoint(BTreeMap::from([(id(1), valid.clone())]), 3)
        .unwrap();
    for corrupt in 0..5 {
        let mut bad = valid.clone();
        match corrupt {
            0 => {
                bad.state = ManagementEffectStateV2::ReconciledSupersededV1 {
                    resolution_operation_id: id(99),
                    certificate_id: hash(20),
                }
            }
            1 => {
                bad.state = ManagementEffectStateV2::ReconciledSupersededV1 {
                    resolution_operation_id: id(99),
                    certificate_id: ManagementEffectHashV1::zero(),
                }
            }
            2 => {
                bad.state = ManagementEffectStateV2::Completed {
                    evidence_hash: ManagementEffectHashV1::zero(),
                }
            }
            3 => bad.last_sequence = 0,
            _ => bad.unknown_reason = Some(ManagementUnknownReasonV2::DetachedEffect),
        }
        assert!(
            reducer::ManagementEffectReplayV2::from_checkpoint(BTreeMap::from([(id(1), bad)]), 3,)
                .is_err(),
            "corrupt fixture {corrupt}"
        );
    }
}

#[test]
fn recovery_cannot_change_set_to_delete() {
    let mut effects = BTreeMap::from([(id(1), snapshot(request(1)))]);
    let mut next = successor(2, 1).request;
    next.base.inventory = ManagementInventoryIdV1::parse("C25").unwrap();
    next.credential.as_mut().unwrap().kind = CredentialMutationKindV1::Delete;
    next.credential.as_mut().unwrap().credential_plan_hash = hash(30);
    next.validate_new(hash(1)).unwrap();
    let before = effects.clone();
    assert!(
        apply(
            &mut effects,
            4,
            ManagementEffectEventV2::Prepared {
                request: Box::new(next)
            },
            &Proofs::default()
        )
        .is_err()
    );
    assert_eq!(effects, before);
}
