//! Ledger epoch, baseline and operation-record validation tests.
use super::*;

fn hex(byte: char) -> String {
    byte.to_string().repeat(64)
}

fn operation_id(index: usize) -> String {
    format!("{index:032x}")
}

fn anchor(inode: u64, mode: u32, links: u64) -> EntryAnchorV2 {
    EntryAnchorV2 {
        device: 7,
        inode,
        uid: unsafe { libc::geteuid() },
        mode,
        links,
    }
}

fn epoch() -> LedgerEpochV1 {
    LedgerEpochV1 {
        protocol_version: 2,
        store_identity: hex('1'),
        random_epoch_id: hex('2'),
        migration_id: hex('3'),
        canonical_parent_anchor: anchor(11, 0o755, 3),
        tombstone_anchor: TombstoneLedgerAnchorV1 {
            anchor: anchor(12, 0o600, 1),
            file_hash: hex('4'),
        },
        retired_source_anchor: RetiredLedgerAnchorV1 {
            anchor: anchor(13, 0o700, 2),
            non_secret_tree_hash: hex('5'),
        },
        active_v2_root_anchor: anchor(14, 0o700, 2),
        activation_tree_hash: hex('6'),
    }
}

fn baseline(kind: DocumentKindV2) -> DocumentMetadataV2 {
    DocumentMetadataV2 {
        kind,
        presence: DocumentPresenceV2::Present,
        revision: 4,
        token: hex('7'),
        receipt_hash: hex('8'),
    }
}

fn step(kind: CredentialLedgerStepKindV1, references: &[&str]) -> CredentialLedgerStepV1 {
    let references: Vec<_> = references.iter().map(|value| (*value).to_owned()).collect();
    CredentialLedgerStepV1 {
        operation_reference_set_hash: operation_reference_hash(&references).unwrap(),
        references,
        kind,
    }
}

fn single(index: usize, kind: CredentialLedgerStepKindV1) -> CredentialLedgerDescriptorV1 {
    let step = step(kind, &["alpha"]);
    CredentialLedgerDescriptorV1 {
        references: step.references.clone(),
        operation_id: operation_id(index),
        origin_project_identity: hex('9'),
        store_identity: epoch().store_identity,
        inventory: if kind == CredentialLedgerStepKindV1::Set {
            CredentialLedgerInventoryV1::C24
        } else {
            CredentialLedgerInventoryV1::C25
        },
        root_operation_id: operation_id(index),
        predecessor_operation_id: None,
        chain_depth: 1,
        plan_hash: hex('a'),
        operation_reference_set_hash: step.operation_reference_set_hash.clone(),
        authorization_anchor_hash: hex('b'),
        credential_before: baseline(DocumentKindV2::Credential),
        plan: CredentialLedgerPlanV1::Single { step },
    }
}

fn setup(index: usize, with_add: bool, with_prune: bool) -> CredentialLedgerDescriptorV1 {
    let add = with_add.then(|| step(CredentialLedgerStepKindV1::ApplyBatchAdd, &["alpha"]));
    let prune = with_prune.then(|| step(CredentialLedgerStepKindV1::PruneStale, &["beta"]));
    let references: Vec<_> = add
        .iter()
        .chain(prune.iter())
        .flat_map(|value| value.references.iter().cloned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    CredentialLedgerDescriptorV1 {
        references: references.clone(),
        operation_id: operation_id(index),
        origin_project_identity: hex('9'),
        store_identity: epoch().store_identity,
        inventory: CredentialLedgerInventoryV1::C27,
        root_operation_id: operation_id(index),
        predecessor_operation_id: None,
        chain_depth: 1,
        plan_hash: hex('a'),
        operation_reference_set_hash: operation_reference_hash(&references).unwrap(),
        authorization_anchor_hash: hex('b'),
        credential_before: baseline(DocumentKindV2::Credential),
        plan: CredentialLedgerPlanV1::Setup {
            add,
            prune,
            registry_before: baseline(DocumentKindV2::Registry),
        },
    }
}

fn project(sequence: u64) -> LedgerProjectEvidenceV1 {
    LedgerProjectEvidenceV1 {
        origin_project_identity: hex('9'),
        cursor: LedgerProjectCursorV1 {
            generation: 1,
            chain_hash: hex('c'),
            sequence,
            journal_hash: hex('d'),
        },
        state_hash: hex('e'),
    }
}

fn document_after(
    descriptor: &CredentialLedgerDescriptorV1,
    before: &DocumentMetadataV2,
) -> (DocumentMetadataV2, ManagedDocumentReceiptV1) {
    let token = if before.token == hex('f') {
        hex('0')
    } else {
        hex('f')
    };
    let receipt = ManagedDocumentReceiptV1 {
        operation_id: descriptor.operation_id.clone(),
        plan_hash: descriptor.plan_hash.clone(),
        document_kind: before.kind,
        before: before.clone(),
        after_token: token.clone(),
        after_revision: before.revision + 1,
        // This is the complete document's non-secret reference projection. It
        // need not equal the operation's reference subset.
        reference_set_hash: hex('c'),
        registry_payload_hash: (before.kind == DocumentKindV2::Registry).then(|| hex('d')),
    };
    let document = DocumentMetadataV2 {
        kind: before.kind,
        presence: DocumentPresenceV2::Present,
        revision: receipt.after_revision,
        token,
        receipt_hash: DocumentReceiptV2::Managed {
            receipt: receipt.clone(),
        }
        .canonical_hash()
        .unwrap(),
    };
    (document, receipt)
}

fn credential_receipt(
    descriptor: &CredentialLedgerDescriptorV1,
    before: &DocumentMetadataV2,
    index: u8,
    kind: CredentialLedgerStepKindV1,
    references: &[&str],
) -> LedgerCredentialReceiptV1 {
    let (document, managed_receipt) = document_after(descriptor, before);
    LedgerCredentialReceiptV1 {
        step_index: index,
        step_kind: kind,
        operation_reference_set_hash: step(kind, references).operation_reference_set_hash,
        document,
        managed_receipt,
    }
}

struct Chain {
    snapshot: CredentialLedgerSnapshotV1,
    epoch_bytes: Vec<u8>,
    history: Vec<u8>,
}
impl Chain {
    fn new() -> Self {
        let epoch_bytes = epoch().encode().unwrap();
        Self {
            snapshot: CredentialLedgerSnapshotV1::decode(&epoch_bytes, &[]).unwrap(),
            epoch_bytes,
            history: Vec::new(),
        }
    }
    fn append(&mut self, event: CredentialLedgerEventV1) -> CredentialLedgerRecordV1 {
        let record = self.snapshot.next_record(event).unwrap();
        let bytes = record.encode().unwrap();
        self.snapshot.apply(&record, bytes.len() + 1).unwrap();
        self.history.extend_from_slice(&bytes);
        self.history.push(b'\n');
        record
    }
    fn reserved(&mut self, descriptor: &CredentialLedgerDescriptorV1) {
        self.append(CredentialLedgerEventV1::Reserved {
            descriptor: descriptor.clone(),
        });
    }
    fn armed(&mut self, descriptor: &CredentialLedgerDescriptorV1) {
        self.append(CredentialLedgerEventV1::Armed {
            operation_id: descriptor.operation_id.clone(),
            project_authorization_anchor_hash: hex('c'),
        });
    }
    fn unknown(&mut self, descriptor: &CredentialLedgerDescriptorV1) {
        self.append(CredentialLedgerEventV1::OutcomeUnknown {
            operation_id: descriptor.operation_id.clone(),
            reason: LedgerUnknownReasonV1::ProcessRestart,
        });
    }
    fn committed(
        &mut self,
        descriptor: &CredentialLedgerDescriptorV1,
    ) -> LedgerCredentialReceiptV1 {
        let CredentialLedgerPlanV1::Single { step } = &descriptor.plan else {
            panic!("single fixture");
        };
        let receipt = credential_receipt(
            descriptor,
            &descriptor.credential_before,
            0,
            step.kind,
            &["alpha"],
        );
        self.append(CredentialLedgerEventV1::CredentialCommitted {
            operation_id: descriptor.operation_id.clone(),
            receipt: receipt.clone(),
        });
        receipt
    }
    fn acknowledge(
        &mut self,
        descriptor: &CredentialLedgerDescriptorV1,
        terminal: LedgerProjectTerminalV1,
    ) {
        self.append(CredentialLedgerEventV1::ProjectAcknowledged {
            operation_id: descriptor.operation_id.clone(),
            terminal,
            evidence: project(20),
        });
    }
    fn assert_roundtrip(&self) {
        let decoded = CredentialLedgerSnapshotV1::decode(&self.epoch_bytes, &self.history).unwrap();
        assert_eq!(decoded.sequence, self.snapshot.sequence);
        assert_eq!(decoded.head_hash, self.snapshot.head_hash);
        assert_eq!(decoded.operations, self.snapshot.operations);
        assert_eq!(decoded.certificates, self.snapshot.certificates);
        assert_eq!(
            decoded.encoded_bytes,
            self.epoch_bytes.len() + self.history.len()
        );
    }
}

fn rejects(snapshot: &CredentialLedgerSnapshotV1, event: CredentialLedgerEventV1) {
    let before = (
        snapshot.sequence,
        snapshot.head_hash.clone(),
        snapshot.operations.clone(),
        snapshot.certificates.clone(),
    );
    assert!(snapshot.next_record(event).is_err());
    assert_eq!(
        before,
        (
            snapshot.sequence,
            snapshot.head_hash.clone(),
            snapshot.operations.clone(),
            snapshot.certificates.clone()
        )
    );
}

fn forged(
    snapshot: &CredentialLedgerSnapshotV1,
    event: CredentialLedgerEventV1,
) -> CredentialLedgerRecordV1 {
    let mut record = CredentialLedgerRecordV1 {
        schema_version: 1,
        ledger_epoch_id: snapshot.epoch.random_epoch_id.clone(),
        sequence: snapshot.sequence + 1,
        previous_hash: snapshot.head_hash.clone(),
        event,
        record_hash: String::new(),
    };
    record.record_hash = record.computed_hash().unwrap();
    record
}

#[test]
fn ledger_empty_history_requires_the_complete_canonical_epoch() {
    let chain = Chain::new();
    assert_eq!(chain.snapshot.sequence, 0);
    assert!(chain.snapshot.operations.is_empty());
    assert_eq!(chain.snapshot.head_hash, chain.snapshot.epoch_hash);
    assert!(
        chain
            .snapshot
            .operation_is_absent(&operation_id(1))
            .unwrap()
    );
    assert!(
        chain
            .snapshot
            .operation_is_absent("not-an-operation")
            .is_err()
    );
    assert!(matches!(
        CredentialLedgerSnapshotV1::decode(&[], &[]),
        Err(Error::Limit)
    ));
    assert!(CredentialLedgerSnapshotV1::decode(b"{}", &[]).is_err());
    let mut noncanonical = chain.epoch_bytes.clone();
    noncanonical.push(b' ');
    assert!(matches!(
        CredentialLedgerSnapshotV1::decode(&noncanonical, &[]),
        Err(Error::Protocol)
    ));
    let mut value = serde_json::to_value(epoch()).unwrap();
    value["unexpected"] = true.into();
    assert!(CredentialLedgerSnapshotV1::decode(&serde_json::to_vec(&value).unwrap(), &[]).is_err());
    chain.assert_roundtrip();
}

#[test]
fn ledger_parent_0755_is_allowed_but_writable_or_replaced_anchors_are_rejected() {
    let good = epoch();
    good.validate().unwrap();
    for mode in [0o777, 0o775, 0o757, 0o1755, 0o2755, 0o4755] {
        let mut bad = good.clone();
        bad.canonical_parent_anchor.mode = mode;
        assert!(bad.validate().is_err(), "mode {mode:o}");
    }
    let mut bad = good.clone();
    bad.canonical_parent_anchor.uid = bad.canonical_parent_anchor.uid.wrapping_add(1);
    assert!(bad.validate().is_err());
    let mut bad = good.clone();
    bad.active_v2_root_anchor.mode = 0o755;
    assert!(bad.validate().is_err());
    let mut bad = good.clone();
    bad.tombstone_anchor.anchor.links = 2;
    assert!(bad.validate().is_err());
    let mut bad = good.clone();
    bad.active_v2_root_anchor.inode = bad.retired_source_anchor.anchor.inode;
    assert!(bad.validate().is_err());
    let mut bad = good;
    bad.protocol_version = 3;
    assert!(bad.validate().is_err());
}

#[test]
fn ledger_operation_references_are_sorted_unique_and_domain_separated() {
    use sha2::{Digest, Sha256};
    let expected = format!(
        "{:x}",
        Sha256::digest(b"polaris-mcp-reference-set-v1\0[\"alpha\",\"beta\"]")
    );
    assert_eq!(
        operation_reference_hash(&["alpha".into(), "beta".into()]).unwrap(),
        expected
    );
    for references in [
        vec![],
        vec!["beta".into(), "alpha".into()],
        vec!["alpha".into(), "alpha".into()],
        vec!["../alpha".into()],
        vec!["a".repeat(65)],
    ] {
        assert!(operation_reference_hash(&references).is_err());
    }
    let at_limit: Vec<_> = (0..1024).map(|i| format!("r{i:04}")).collect();
    operation_reference_hash(&at_limit).unwrap();
    let mut excessive = at_limit;
    excessive.push("r1024".into());
    assert!(operation_reference_hash(&excessive).is_err());
}

#[test]
fn ledger_single_set_delete_and_ensure_absent_commit_then_origin_acknowledge() {
    for (index, kind) in [
        (1, CredentialLedgerStepKindV1::Set),
        (2, CredentialLedgerStepKindV1::Delete),
        (3, CredentialLedgerStepKindV1::EnsureAbsent),
    ] {
        let mut descriptor = single(index, kind);
        if kind == CredentialLedgerStepKindV1::EnsureAbsent {
            descriptor.credential_before.presence = DocumentPresenceV2::Absent;
            descriptor.credential_before.revision = 0;
        }
        let mut chain = Chain::new();
        chain.reserved(&descriptor);
        chain.armed(&descriptor);
        let receipt = chain.committed(&descriptor);
        assert_eq!(
            chain.snapshot.operations[&descriptor.operation_id].credential_after(),
            &receipt.document
        );
        assert_eq!(
            chain.snapshot.operations[&descriptor.operation_id].phase,
            CredentialLedgerPhaseV1::CredentialCommitted
        );
        let mut wrong_origin = project(20);
        wrong_origin.origin_project_identity = hex('1');
        rejects(
            &chain.snapshot,
            CredentialLedgerEventV1::ProjectAcknowledged {
                operation_id: descriptor.operation_id.clone(),
                terminal: LedgerProjectTerminalV1::Completed,
                evidence: wrong_origin,
            },
        );
        chain.acknowledge(&descriptor, LedgerProjectTerminalV1::Completed);
        assert!(
            chain.snapshot.operations[&descriptor.operation_id]
                .acknowledgement
                .is_some()
        );
        assert!(
            !chain
                .snapshot
                .operation_is_absent(&descriptor.operation_id)
                .unwrap()
        );
        rejects(
            &chain.snapshot,
            CredentialLedgerEventV1::ProjectAcknowledged {
                operation_id: descriptor.operation_id.clone(),
                terminal: LedgerProjectTerminalV1::Completed,
                evidence: project(21),
            },
        );
        chain.assert_roundtrip();
    }
}

#[test]
fn ledger_duplicate_reservation_arming_and_skipped_commit_preserve_the_blocker() {
    let descriptor = single(1, CredentialLedgerStepKindV1::Set);
    let mut chain = Chain::new();
    chain.reserved(&descriptor);
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::Reserved {
            descriptor: descriptor.clone(),
        },
    );
    let receipt = credential_receipt(
        &descriptor,
        &descriptor.credential_before,
        0,
        CredentialLedgerStepKindV1::Set,
        &["alpha"],
    );
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::CredentialCommitted {
            operation_id: descriptor.operation_id.clone(),
            receipt,
        },
    );
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::ProjectAcknowledged {
            operation_id: descriptor.operation_id.clone(),
            terminal: LedgerProjectTerminalV1::Completed,
            evidence: project(20),
        },
    );
    chain.armed(&descriptor);
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::Armed {
            operation_id: descriptor.operation_id.clone(),
            project_authorization_anchor_hash: hex('d'),
        },
    );
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::Reserved {
            descriptor: single(2, CredentialLedgerStepKindV1::Set),
        },
    );
    chain.assert_roundtrip();
}

#[test]
fn ledger_receipt_requires_exact_operation_before_token_revision_and_hash() {
    let descriptor = single(1, CredentialLedgerStepKindV1::Set);
    let mut chain = Chain::new();
    chain.reserved(&descriptor);
    chain.armed(&descriptor);
    let good = credential_receipt(
        &descriptor,
        &descriptor.credential_before,
        0,
        CredentialLedgerStepKindV1::Set,
        &["alpha"],
    );
    let mutations: [fn(&mut LedgerCredentialReceiptV1); 9] = [
        |r| r.managed_receipt.operation_id = operation_id(2),
        |r| r.managed_receipt.plan_hash = hex('0'),
        |r| r.managed_receipt.before.token = hex('0'),
        |r| r.document.revision += 1,
        |r| r.document.token = hex('0'),
        |r| r.document.receipt_hash = hex('0'),
        |r| r.step_index = 1,
        |r| r.step_kind = CredentialLedgerStepKindV1::Delete,
        |r| r.operation_reference_set_hash = hex('0'),
    ];
    for mutate in mutations {
        let mut receipt = good.clone();
        mutate(&mut receipt);
        rejects(
            &chain.snapshot,
            CredentialLedgerEventV1::CredentialCommitted {
                operation_id: descriptor.operation_id.clone(),
                receipt,
            },
        );
    }
    let mut receipt = good.clone();
    receipt.managed_receipt.after_token = descriptor.credential_before.token.clone();
    receipt.document.token = receipt.managed_receipt.after_token.clone();
    receipt.document.receipt_hash = DocumentReceiptV2::Managed {
        receipt: receipt.managed_receipt.clone(),
    }
    .canonical_hash()
    .unwrap();
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::CredentialCommitted {
            operation_id: descriptor.operation_id.clone(),
            receipt,
        },
    );
    chain.committed(&descriptor);
    chain.assert_roundtrip();
}

#[test]
fn ledger_unknown_cannot_reuse_a_terminal_predecessor() {
    let descriptor = single(1, CredentialLedgerStepKindV1::Set);
    let mut chain = Chain::new();
    chain.reserved(&descriptor);
    chain.armed(&descriptor);
    chain.unknown(&descriptor);
    chain.committed(&descriptor);
    let continuation = successor(&descriptor, 2);
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::Reserved {
            descriptor: continuation,
        },
    );
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::OutcomeUnknown {
            operation_id: descriptor.operation_id.clone(),
            reason: LedgerUnknownReasonV1::EvidenceUnavailable,
        },
    );
    chain.acknowledge(&descriptor, LedgerProjectTerminalV1::Completed);
    chain.reserved(&single(3, CredentialLedgerStepKindV1::Set));
    chain.assert_roundtrip();

    let mut abandoned = Chain::new();
    abandoned.reserved(&descriptor);
    abandoned.armed(&descriptor);
    abandoned.unknown(&descriptor);
    abandoned.append(CredentialLedgerEventV1::NoWriteCertified {
        operation_id: descriptor.operation_id.clone(),
        current: descriptor.credential_before.clone(),
        operation_temp_absent: true,
    });
    rejects(
        &abandoned.snapshot,
        CredentialLedgerEventV1::Reserved {
            descriptor: successor(&descriptor, 2),
        },
    );
    abandoned.acknowledge(&descriptor, LedgerProjectTerminalV1::ReconciledAbandon);
    abandoned.assert_roundtrip();
}

#[test]
fn ledger_no_write_certificate_requires_exact_before_and_absent_operation_temp() {
    let descriptor = single(1, CredentialLedgerStepKindV1::Delete);
    let mut chain = Chain::new();
    chain.reserved(&descriptor);
    let mut changed = descriptor.credential_before.clone();
    changed.token = hex('0');
    for (current, absent) in [
        (changed, true),
        (descriptor.credential_before.clone(), false),
    ] {
        rejects(
            &chain.snapshot,
            CredentialLedgerEventV1::NoWriteCertified {
                operation_id: descriptor.operation_id.clone(),
                current,
                operation_temp_absent: absent,
            },
        );
    }
    chain.append(CredentialLedgerEventV1::NoWriteCertified {
        operation_id: descriptor.operation_id.clone(),
        current: descriptor.credential_before.clone(),
        operation_temp_absent: true,
    });
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::ProjectAcknowledged {
            operation_id: descriptor.operation_id.clone(),
            terminal: LedgerProjectTerminalV1::Completed,
            evidence: project(20),
        },
    );
    let mut evidence = project(20);
    evidence.cursor.journal_hash = "missing".into();
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::ProjectAcknowledged {
            operation_id: descriptor.operation_id.clone(),
            terminal: LedgerProjectTerminalV1::ReconciledAbandon,
            evidence,
        },
    );
    chain.acknowledge(&descriptor, LedgerProjectTerminalV1::ReconciledAbandon);
    chain.assert_roundtrip();
}

fn begin_setup(
    chain: &mut Chain,
    descriptor: &CredentialLedgerDescriptorV1,
) -> Option<LedgerCredentialReceiptV1> {
    chain.reserved(descriptor);
    chain.armed(descriptor);
    let CredentialLedgerPlanV1::Setup { add, .. } = &descriptor.plan else {
        panic!("setup fixture");
    };
    add.as_ref().map(|_| {
        let receipt = credential_receipt(
            descriptor,
            &descriptor.credential_before,
            0,
            CredentialLedgerStepKindV1::ApplyBatchAdd,
            &["alpha"],
        );
        chain.append(CredentialLedgerEventV1::CredentialStepCommitted {
            operation_id: descriptor.operation_id.clone(),
            receipt: receipt.clone(),
        });
        receipt
    })
}

fn registry_started(chain: &mut Chain, descriptor: &CredentialLedgerDescriptorV1) {
    chain.append(CredentialLedgerEventV1::RegistryApplyStarted {
        operation_id: descriptor.operation_id.clone(),
    });
}

fn registry_committed(chain: &mut Chain, descriptor: &CredentialLedgerDescriptorV1) {
    let CredentialLedgerPlanV1::Setup {
        registry_before, ..
    } = &descriptor.plan
    else {
        panic!("setup fixture");
    };
    let (document, managed_receipt) = document_after(descriptor, registry_before);
    chain.append(CredentialLedgerEventV1::RegistryCommitted {
        operation_id: descriptor.operation_id.clone(),
        document,
        managed_receipt,
    });
}

fn registry_not_applied(chain: &mut Chain, descriptor: &CredentialLedgerDescriptorV1) {
    let CredentialLedgerPlanV1::Setup {
        registry_before, ..
    } = &descriptor.plan
    else {
        panic!("setup fixture");
    };
    chain.append(CredentialLedgerEventV1::RegistryNotApplied {
        operation_id: descriptor.operation_id.clone(),
        exact_before: registry_before.clone(),
    });
}

#[test]
fn ledger_setup_add_registry_commit_prune_and_compensation_are_distinct_complete_paths() {
    for inventory in [
        CredentialLedgerInventoryV1::C27,
        CredentialLedgerInventoryV1::D37,
    ] {
        for applied in [true, false] {
            let mut descriptor = setup(1, true, true);
            descriptor.inventory = inventory;
            let mut chain = Chain::new();
            let add = begin_setup(&mut chain, &descriptor).unwrap();
            registry_started(&mut chain, &descriptor);
            if applied {
                registry_committed(&mut chain, &descriptor);
            } else {
                registry_not_applied(&mut chain, &descriptor);
            }
            let (kind, references) = if applied {
                (CredentialLedgerStepKindV1::PruneStale, ["beta"])
            } else {
                (CredentialLedgerStepKindV1::CompensateAdded, ["alpha"])
            };
            let receipt = credential_receipt(&descriptor, &add.document, 1, kind, &references);
            chain.append(CredentialLedgerEventV1::CredentialStepCommitted {
                operation_id: descriptor.operation_id.clone(),
                receipt,
            });
            chain.append(if applied {
                CredentialLedgerEventV1::Applied {
                    operation_id: descriptor.operation_id.clone(),
                }
            } else {
                CredentialLedgerEventV1::CompensatedFailure {
                    operation_id: descriptor.operation_id.clone(),
                }
            });
            assert_eq!(
                chain.snapshot.operations[&descriptor.operation_id]
                    .credential_receipts
                    .len(),
                2
            );
            chain.acknowledge(&descriptor, LedgerProjectTerminalV1::Completed);
            chain.assert_roundtrip();
        }
    }
}

#[test]
fn ledger_setup_optional_add_or_prune_preserve_required_phase_order() {
    for (with_add, with_prune) in [(true, false), (false, true)] {
        let descriptor = setup(1, with_add, with_prune);
        let mut chain = Chain::new();
        let add = begin_setup(&mut chain, &descriptor);
        registry_started(&mut chain, &descriptor);
        registry_committed(&mut chain, &descriptor);
        if with_prune {
            let before = add
                .as_ref()
                .map_or(&descriptor.credential_before, |receipt| &receipt.document);
            chain.append(CredentialLedgerEventV1::CredentialStepCommitted {
                operation_id: descriptor.operation_id.clone(),
                receipt: credential_receipt(
                    &descriptor,
                    before,
                    u8::from(with_add),
                    CredentialLedgerStepKindV1::PruneStale,
                    &["beta"],
                ),
            });
        }
        chain.append(CredentialLedgerEventV1::Applied {
            operation_id: descriptor.operation_id.clone(),
        });
        chain.acknowledge(&descriptor, LedgerProjectTerminalV1::Completed);
        chain.assert_roundtrip();
    }
    let descriptor = setup(1, false, true);
    let mut chain = Chain::new();
    begin_setup(&mut chain, &descriptor);
    registry_started(&mut chain, &descriptor);
    registry_not_applied(&mut chain, &descriptor);
    chain.append(CredentialLedgerEventV1::CompensatedFailure {
        operation_id: descriptor.operation_id.clone(),
    });
    chain.acknowledge(&descriptor, LedgerProjectTerminalV1::Completed);
    chain.assert_roundtrip();
}

#[test]
fn ledger_setup_rejects_skipped_duplicate_wrong_index_and_double_branch_receipts() {
    let descriptor = setup(1, true, true);
    let mut chain = Chain::new();
    chain.reserved(&descriptor);
    chain.armed(&descriptor);
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::RegistryApplyStarted {
            operation_id: descriptor.operation_id.clone(),
        },
    );
    let add = credential_receipt(
        &descriptor,
        &descriptor.credential_before,
        0,
        CredentialLedgerStepKindV1::ApplyBatchAdd,
        &["alpha"],
    );
    let mut bad_index = add.clone();
    bad_index.step_index = 1;
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::CredentialStepCommitted {
            operation_id: descriptor.operation_id.clone(),
            receipt: bad_index,
        },
    );
    chain.append(CredentialLedgerEventV1::CredentialStepCommitted {
        operation_id: descriptor.operation_id.clone(),
        receipt: add.clone(),
    });
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::CredentialStepCommitted {
            operation_id: descriptor.operation_id.clone(),
            receipt: add.clone(),
        },
    );
    let prune = credential_receipt(
        &descriptor,
        &add.document,
        1,
        CredentialLedgerStepKindV1::PruneStale,
        &["beta"],
    );
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::CredentialStepCommitted {
            operation_id: descriptor.operation_id.clone(),
            receipt: prune.clone(),
        },
    );
    let CredentialLedgerPlanV1::Setup {
        registry_before, ..
    } = &descriptor.plan
    else {
        panic!("setup fixture");
    };
    let (document, managed_receipt) = document_after(&descriptor, registry_before);
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::RegistryCommitted {
            operation_id: descriptor.operation_id.clone(),
            document: document.clone(),
            managed_receipt: managed_receipt.clone(),
        },
    );
    registry_started(&mut chain, &descriptor);
    let mut wrong_receipt = managed_receipt.clone();
    wrong_receipt.before.token = hex('0');
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::RegistryCommitted {
            operation_id: descriptor.operation_id.clone(),
            document: document.clone(),
            managed_receipt: wrong_receipt,
        },
    );
    chain.append(CredentialLedgerEventV1::RegistryCommitted {
        operation_id: descriptor.operation_id.clone(),
        document,
        managed_receipt,
    });
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::Applied {
            operation_id: descriptor.operation_id.clone(),
        },
    );
    let compensation = credential_receipt(
        &descriptor,
        &add.document,
        1,
        CredentialLedgerStepKindV1::CompensateAdded,
        &["alpha"],
    );
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::CredentialStepCommitted {
            operation_id: descriptor.operation_id.clone(),
            receipt: compensation,
        },
    );
    chain.append(CredentialLedgerEventV1::CredentialStepCommitted {
        operation_id: descriptor.operation_id.clone(),
        receipt: prune,
    });
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::CredentialStepCommitted {
            operation_id: descriptor.operation_id.clone(),
            receipt: credential_receipt(
                &descriptor,
                chain.snapshot.operations[&descriptor.operation_id].credential_after(),
                2,
                CredentialLedgerStepKindV1::CompensateAdded,
                &["alpha"],
            ),
        },
    );
    chain.append(CredentialLedgerEventV1::Applied {
        operation_id: descriptor.operation_id.clone(),
    });
    chain.assert_roundtrip();
}

#[test]
fn ledger_unknown_registry_outcome_blocks_every_new_reservation_until_exact_resolution() {
    let descriptor = setup(1, true, true);
    let mut chain = Chain::new();
    begin_setup(&mut chain, &descriptor);
    registry_started(&mut chain, &descriptor);
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::Reserved {
            descriptor: single(2, CredentialLedgerStepKindV1::Set),
        },
    );
    chain.append(CredentialLedgerEventV1::RegistryOutcomeUnknown {
        operation_id: descriptor.operation_id.clone(),
    });
    chain.unknown(&descriptor);
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::Reserved {
            descriptor: successor(&descriptor, 2),
        },
    );
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::RegistryOutcomeUnknown {
            operation_id: descriptor.operation_id.clone(),
        },
    );
    let CredentialLedgerPlanV1::Setup {
        registry_before, ..
    } = &descriptor.plan
    else {
        panic!("setup fixture");
    };
    let mut wrong_before = registry_before.clone();
    wrong_before.receipt_hash = hex('0');
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::RegistryNotApplied {
            operation_id: descriptor.operation_id.clone(),
            exact_before: wrong_before,
        },
    );
    registry_not_applied(&mut chain, &descriptor);
    chain.assert_roundtrip();
}

fn successor(
    predecessor: &CredentialLedgerDescriptorV1,
    index: usize,
) -> CredentialLedgerDescriptorV1 {
    let mut descriptor = predecessor.clone();
    descriptor.operation_id = operation_id(index);
    descriptor.predecessor_operation_id = Some(predecessor.operation_id.clone());
    descriptor.chain_depth += 1;
    descriptor
}

#[test]
fn ledger_recovery_rejects_bad_identity_depth_origin_reference_and_fanout() {
    let descriptor = single(1, CredentialLedgerStepKindV1::Set);
    let mut chain = Chain::new();
    chain.reserved(&descriptor);
    chain.armed(&descriptor);
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::Reserved {
            descriptor: successor(&descriptor, 2),
        },
    );
    chain.unknown(&descriptor);
    let good = successor(&descriptor, 2);
    let mutations: [fn(&mut CredentialLedgerDescriptorV1); 7] = [
        |d| d.operation_id = "not-a-32-byte-hex-id".into(),
        |d| d.chain_depth = 3,
        |d| d.root_operation_id = operation_id(9),
        |d| d.predecessor_operation_id = Some(operation_id(9)),
        |d| d.origin_project_identity = hex('0'),
        |d| d.store_identity = hex('0'),
        |d| d.inventory = CredentialLedgerInventoryV1::C25,
    ];
    for mutate in mutations {
        let mut bad = good.clone();
        mutate(&mut bad);
        rejects(
            &chain.snapshot,
            CredentialLedgerEventV1::Reserved { descriptor: bad },
        );
    }
    let mut bad = good.clone();
    let other = step(CredentialLedgerStepKindV1::Set, &["beta"]);
    bad.references = other.references.clone();
    bad.operation_reference_set_hash = other.operation_reference_set_hash.clone();
    bad.plan = CredentialLedgerPlanV1::Single { step: other };
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::Reserved { descriptor: bad },
    );
    chain.reserved(&good);
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::Reserved {
            descriptor: successor(&descriptor, 3),
        },
    );
    chain.assert_roundtrip();
}

#[test]
fn ledger_recovery_depth_eight_is_retained_and_ninth_prepare_is_rejected() {
    let mut chain = Chain::new();
    let mut descriptor = single(1, CredentialLedgerStepKindV1::Set);
    for depth in 1..=8 {
        assert_eq!(descriptor.chain_depth, depth);
        chain.reserved(&descriptor);
        chain.armed(&descriptor);
        chain.unknown(&descriptor);
        if depth != 8 {
            descriptor = successor(&descriptor, usize::from(depth) + 1);
        }
    }
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::Reserved {
            descriptor: successor(&descriptor, 9),
        },
    );
    assert_eq!(chain.snapshot.operations.len(), 8);
    assert!(
        chain
            .snapshot
            .operations
            .values()
            .all(|operation| operation.unknown.is_some() && operation.acknowledgement.is_none())
    );
    chain.assert_roundtrip();
}

fn certificate(
    chain: &Chain,
    predecessor: &CredentialLedgerDescriptorV1,
    resolver: &CredentialLedgerDescriptorV1,
    successor: &CredentialLedgerDescriptorV1,
    downstream: Option<String>,
) -> SupersessionCertificateV1 {
    let predecessor_entry = &chain.snapshot.operations[&predecessor.operation_id];
    let resolver_entry = &chain.snapshot.operations[&resolver.operation_id];
    let current = resolver_entry.credential_after();
    SupersessionCertificateV1 {
        ledger_epoch_id: chain.snapshot.epoch.random_epoch_id.clone(),
        store_identity: chain.snapshot.epoch.store_identity.clone(),
        origin_project_identity: predecessor.origin_project_identity.clone(),
        root_operation_id: predecessor.root_operation_id.clone(),
        predecessor_operation_id: predecessor.operation_id.clone(),
        resolver_operation_id: resolver.operation_id.clone(),
        direct_successor_operation_id: successor.operation_id.clone(),
        downstream_certificate_id: downstream,
        predecessor_depth: predecessor.chain_depth,
        resolver_depth: resolver.chain_depth,
        predecessor_project: project(10),
        predecessor_authorization_anchor_hash: predecessor_entry
            .project_authorization_anchor_hash
            .clone()
            .unwrap_or_else(|| predecessor.authorization_anchor_hash.clone()),
        resolver_project: project(20),
        resolver_managed_receipt_id: current.receipt_hash.clone(),
        resolver_after_token: current.token.clone(),
        resolver_after_revision: current.revision,
        resolver_credential_committed_transition_hash: resolver_entry
            .credential_committed_transition_hash
            .clone()
            .unwrap(),
        previous_hash: chain.snapshot.head_hash.clone(),
    }
}

fn certify(
    chain: &mut Chain,
    certificate: SupersessionCertificateV1,
    current: &DocumentMetadataV2,
) -> String {
    let certificate_id = certificate.canonical_hash().unwrap();
    chain.append(CredentialLedgerEventV1::SupersessionCertified {
        certificate_id: certificate_id.clone(),
        certificate,
        current: current.clone(),
    });
    certificate_id
}

fn acknowledge_certificate(chain: &mut Chain, certificate_id: &str) {
    chain.append(CredentialLedgerEventV1::CertificateAcknowledged {
        certificate_id: certificate_id.to_owned(),
        origin_project_identity: hex('9'),
        predecessor_project: project(21),
    });
}

fn resolved_chain(depth: usize) -> (Chain, Vec<CredentialLedgerDescriptorV1>, DocumentMetadataV2) {
    let mut chain = Chain::new();
    let mut descriptors = vec![single(1, CredentialLedgerStepKindV1::Set)];
    for index in 1..=depth {
        let descriptor = descriptors.last().unwrap().clone();
        chain.reserved(&descriptor);
        chain.armed(&descriptor);
        if index != depth {
            chain.unknown(&descriptor);
            descriptors.push(successor(&descriptor, index + 1));
        }
    }
    let resolver = descriptors.last().unwrap();
    let current = chain.committed(resolver).document;
    chain.acknowledge(resolver, LedgerProjectTerminalV1::Completed);
    (chain, descriptors, current)
}

#[test]
fn ledger_depth_two_certificate_and_ack_release_the_global_blocker() {
    let (mut chain, descriptors, current) = resolved_chain(2);
    let cert = certificate(
        &chain,
        &descriptors[0],
        &descriptors[1],
        &descriptors[1],
        None,
    );
    let id = certify(&mut chain, cert, &current);
    assert_eq!(
        chain.snapshot.operations[&descriptors[0].operation_id].phase,
        CredentialLedgerPhaseV1::Superseded
    );
    assert!(
        chain.snapshot.operations[&descriptors[0].operation_id]
            .unknown
            .is_some()
    );
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::Reserved {
            descriptor: single(3, CredentialLedgerStepKindV1::Set),
        },
    );
    let mut wrong_origin = project(21);
    wrong_origin.origin_project_identity = hex('0');
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::CertificateAcknowledged {
            certificate_id: id.clone(),
            origin_project_identity: hex('0'),
            predecessor_project: wrong_origin,
        },
    );
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::CertificateAcknowledged {
            certificate_id: id.clone(),
            origin_project_identity: hex('9'),
            predecessor_project: project(10),
        },
    );
    acknowledge_certificate(&mut chain, &id);
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::CertificateAcknowledged {
            certificate_id: id,
            origin_project_identity: hex('9'),
            predecessor_project: project(22),
        },
    );
    chain.reserved(&single(3, CredentialLedgerStepKindV1::Set));
    chain.assert_roundtrip();
}

#[test]
fn ledger_depth_three_certifies_from_tail_to_root_with_exact_downstream_evidence() {
    let (mut chain, descriptors, current) = resolved_chain(3);
    let skipped = certificate(
        &chain,
        &descriptors[0],
        &descriptors[2],
        &descriptors[1],
        None,
    );
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::SupersessionCertified {
            certificate_id: skipped.canonical_hash().unwrap(),
            certificate: skipped,
            current: current.clone(),
        },
    );
    let tail = certificate(
        &chain,
        &descriptors[1],
        &descriptors[2],
        &descriptors[2],
        None,
    );
    let tail_id = certify(&mut chain, tail, &current);
    let root = certificate(
        &chain,
        &descriptors[0],
        &descriptors[2],
        &descriptors[1],
        Some(tail_id.clone()),
    );
    let root_id = certify(&mut chain, root, &current);
    assert_eq!(chain.snapshot.certificates.len(), 2);
    assert!(
        chain
            .snapshot
            .operations
            .values()
            .filter(|entry| entry.phase == CredentialLedgerPhaseV1::Superseded)
            .count()
            == 2
    );
    acknowledge_certificate(&mut chain, &tail_id);
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::Reserved {
            descriptor: single(4, CredentialLedgerStepKindV1::Set),
        },
    );
    acknowledge_certificate(&mut chain, &root_id);
    chain.reserved(&single(4, CredentialLedgerStepKindV1::Set));
    chain.assert_roundtrip();
}

#[test]
fn ledger_certificate_rejects_changed_current_receipt_cursor_origin_and_transition() {
    let (chain, descriptors, current) = resolved_chain(2);
    let good = certificate(
        &chain,
        &descriptors[0],
        &descriptors[1],
        &descriptors[1],
        None,
    );
    let mutations: [fn(&mut SupersessionCertificateV1); 9] = [
        |c| c.previous_hash = hex('0'),
        |c| c.ledger_epoch_id = hex('0'),
        |c| c.origin_project_identity = hex('0'),
        |c| c.resolver_depth = 3,
        |c| c.resolver_after_token = hex('0'),
        |c| c.resolver_managed_receipt_id = hex('0'),
        |c| c.resolver_credential_committed_transition_hash = hex('0'),
        |c| c.predecessor_project.cursor.journal_hash = "missing".into(),
        |c| c.downstream_certificate_id = Some(hex('0')),
    ];
    for mutate in mutations {
        let mut bad = good.clone();
        mutate(&mut bad);
        rejects(
            &chain.snapshot,
            CredentialLedgerEventV1::SupersessionCertified {
                certificate_id: bad.canonical_hash().unwrap(),
                certificate: bad,
                current: current.clone(),
            },
        );
    }
    let mut changed = current;
    changed.receipt_hash = hex('0');
    rejects(
        &chain.snapshot,
        CredentialLedgerEventV1::SupersessionCertified {
            certificate_id: good.canonical_hash().unwrap(),
            certificate: good,
            current: changed,
        },
    );
    chain.assert_roundtrip();
}

#[test]
fn ledger_full_jsonl_rejects_torn_changed_noncanonical_unknown_and_oversized_records() {
    let descriptor = single(1, CredentialLedgerStepKindV1::Set);
    let mut chain = Chain::new();
    let initial = chain.snapshot.clone();
    let first = chain.append(CredentialLedgerEventV1::Reserved {
        descriptor: descriptor.clone(),
    });
    chain.armed(&descriptor);
    chain.assert_roundtrip();
    let mut torn = chain.history.clone();
    torn.pop();
    assert!(matches!(
        CredentialLedgerSnapshotV1::decode(&chain.epoch_bytes, &torn),
        Err(Error::RecoveryRequired)
    ));
    let mut broken = first.clone();
    broken.previous_hash = hex('0');
    broken.record_hash = broken.computed_hash().unwrap();
    let mut bytes = broken.encode().unwrap();
    bytes.push(b'\n');
    assert!(CredentialLedgerSnapshotV1::decode(&chain.epoch_bytes, &bytes).is_err());
    let mut wrong_sequence = first.clone();
    wrong_sequence.sequence = 2;
    wrong_sequence.record_hash = wrong_sequence.computed_hash().unwrap();
    let mut bytes = wrong_sequence.encode().unwrap();
    bytes.push(b'\n');
    assert!(CredentialLedgerSnapshotV1::decode(&chain.epoch_bytes, &bytes).is_err());
    let mut wrong_epoch = first.clone();
    wrong_epoch.ledger_epoch_id = hex('0');
    wrong_epoch.record_hash = wrong_epoch.computed_hash().unwrap();
    let mut bytes = wrong_epoch.encode().unwrap();
    bytes.push(b'\n');
    assert!(CredentialLedgerSnapshotV1::decode(&chain.epoch_bytes, &bytes).is_err());
    let mut unknown_schema = first.clone();
    unknown_schema.schema_version = 2;
    unknown_schema.record_hash = unknown_schema.computed_hash().unwrap();
    let mut bytes = unknown_schema.encode().unwrap();
    bytes.push(b'\n');
    assert!(CredentialLedgerSnapshotV1::decode(&chain.epoch_bytes, &bytes).is_err());
    let mut value = serde_json::to_value(&first).unwrap();
    value["unknown_field"] = true.into();
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    assert!(CredentialLedgerSnapshotV1::decode(&chain.epoch_bytes, &bytes).is_err());
    let mut value = serde_json::to_value(&first).unwrap();
    value["event"]["kind"] = "future_transition".into();
    let mut bytes = serde_json::to_vec(&value).unwrap();
    bytes.push(b'\n');
    assert!(CredentialLedgerSnapshotV1::decode(&chain.epoch_bytes, &bytes).is_err());
    let mut bytes = serde_json::to_vec_pretty(&first).unwrap();
    bytes.push(b'\n');
    assert!(CredentialLedgerSnapshotV1::decode(&chain.epoch_bytes, &bytes).is_err());
    let mut duplicate = first.encode().unwrap();
    duplicate.push(b'\n');
    duplicate.extend_from_slice(&first.encode().unwrap());
    duplicate.push(b'\n');
    assert!(CredentialLedgerSnapshotV1::decode(&chain.epoch_bytes, &duplicate).is_err());
    let mut oversized = vec![b' '; MAX_LEDGER_RECORD_BYTES + 1];
    oversized.push(b'\n');
    assert!(matches!(
        CredentialLedgerSnapshotV1::decode(&chain.epoch_bytes, &oversized),
        Err(Error::Limit)
    ));
    let mut at_bound = vec![b' '; MAX_LEDGER_RECORD_BYTES];
    at_bound.push(b'\n');
    assert!(matches!(
        CredentialLedgerSnapshotV1::decode(&chain.epoch_bytes, &at_bound),
        Err(Error::Protocol)
    ));
    assert!(matches!(
        CredentialLedgerSnapshotV1::decode(&chain.epoch_bytes, b"\n"),
        Err(Error::Limit)
    ));
    let invalid_phase = forged(
        &initial,
        CredentialLedgerEventV1::Armed {
            operation_id: descriptor.operation_id,
            project_authorization_anchor_hash: hex('c'),
        },
    );
    let mut bytes = invalid_phase.encode().unwrap();
    bytes.push(b'\n');
    assert!(CredentialLedgerSnapshotV1::decode(&chain.epoch_bytes, &bytes).is_err());
}

#[test]
fn ledger_reservation_capacity_counts_epoch_bytes_records_and_all_operations_inclusively() {
    let chain = Chain::new();
    assert_eq!(chain.snapshot.encoded_bytes, chain.epoch_bytes.len());
    let per_record = MAX_LEDGER_RECORD_BYTES + 1;
    let remaining = (MAX_LEDGER_BYTES - chain.epoch_bytes.len()) / per_record;
    chain.snapshot.reserve_capacity(remaining, 1).unwrap();
    assert!(matches!(
        chain.snapshot.reserve_capacity(remaining + 1, 1),
        Err(Error::Limit)
    ));
    assert!(matches!(
        chain.snapshot.reserve_capacity(0, 0),
        Err(Error::Limit)
    ));
    assert!(matches!(
        chain.snapshot.reserve_capacity(1, 2),
        Err(Error::Limit)
    ));
    let mut bytes = chain.snapshot.clone();
    bytes.encoded_bytes = MAX_LEDGER_BYTES - per_record;
    bytes.reserve_capacity(1, 0).unwrap();
    bytes.encoded_bytes += 1;
    assert!(matches!(bytes.reserve_capacity(1, 0), Err(Error::Limit)));
    let mut records = chain.snapshot.clone();
    records.sequence = MAX_LEDGER_RECORDS as u64 - 2;
    records.reserve_capacity(1, 0).unwrap();
    records.sequence += 1;
    assert!(matches!(records.reserve_capacity(1, 0), Err(Error::Limit)));
    let mut operations = chain.snapshot.clone();
    let mut dummy = Chain::new();
    let descriptor = single(1, CredentialLedgerStepKindV1::Set);
    dummy.reserved(&descriptor);
    let entry = dummy.snapshot.operations[&descriptor.operation_id].clone();
    for index in 0..MAX_LEDGER_OPERATIONS - 1 {
        operations
            .operations
            .insert(operation_id(index), entry.clone());
    }
    operations.reserve_capacity(1, 1).unwrap();
    operations
        .operations
        .insert(operation_id(MAX_LEDGER_OPERATIONS), entry);
    assert!(matches!(
        operations.reserve_capacity(1, 1),
        Err(Error::Limit)
    ));
    operations.reserve_capacity(1, 0).unwrap();
    let excessive = vec![b' '; MAX_LEDGER_BYTES - chain.epoch_bytes.len() + 1];
    assert!(matches!(
        CredentialLedgerSnapshotV1::decode(&chain.epoch_bytes, &excessive),
        Err(Error::Limit)
    ));
}
