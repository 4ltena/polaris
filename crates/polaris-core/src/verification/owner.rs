//! Host-owned production port for the verification pipeline.
//!
//! Session, legacy and Desktop adapters call this module with exact artifact
//! bodies and their already-owned hashes.  It deliberately has no session,
//! UI, provider, or process ownership: each surface supplies those through a
//! narrow port and persists only the immutable references returned here.

use polaris_provider::decision::Decider;

use crate::{
    common_tests::RunReport,
    workflow::{
        EVIDENCE_SCHEMA_VERSION, EvidenceBindingV1, EvidenceRecordV1,
        ImplementationReceiptAuthorityV1, ImplementationReceiptV1, ReviewRecordV1,
    },
};

use super::{
    AllReasonV1, AllTestsScopeV1, CandidateHintsV1, RustTestExecutionOutcomeV1,
    RustTestExecutionPlanV1, StaticCapabilityCatalogV1, TestEntryV1, TestSelectionV1,
    adapters::{
        RustInventoryListingV1, StaticAdapterInvocationV1, StaticAdapterOutcomeV1,
        StaticAdapterParserV1, collect_complete_rust_inventory, parse_registered_static_evidence,
    },
    pipeline::{
        ActiveVerificationArtifactsV1, JevSelectionResultV1, aggregate_criterion_evidence,
        build_complete_rust_inventory, common_case_evidence_records, derive_verification_needs,
        existing_test_evidence_record, persist_inventory_and_candidates, select_with_advisory_jev,
        static_satisfied_evidence_record,
    },
    store::{VerificationBindingV1, VerificationRecordRefV1, VerificationStore},
};

/// Immutable references the session checkpoint retains after preparation.
pub struct PreparedVerificationV1 {
    pub need_refs: Vec<VerificationRecordRefV1>,
    pub inventory_ref: VerificationRecordRefV1,
    pub candidate_ref: VerificationRecordRefV1,
    pub selection: JevSelectionResultV1,
}

/// Evidence may leave this module only as an opaque, owner-verified token.
/// Its raw record is deliberately inaccessible to conversational tools.
pub struct VerifiedEvidenceRecordV1 {
    record: EvidenceRecordV1,
}

impl VerifiedEvidenceRecordV1 {
    pub(crate) fn record(&self) -> &EvidenceRecordV1 {
        &self.record
    }
}

/// The only convergence payload a Session/Desktop/legacy owner may accept
/// from this bridge.  It is built from sealed records whose binding and
/// criterion were verified here.
pub struct VerifiedConvergenceBundleV1 {
    binding: EvidenceBindingV1,
    implementation_receipts: Vec<ImplementationReceiptV1>,
    reviews: Vec<ReviewRecordV1>,
    evidence: Vec<EvidenceRecordV1>,
}

impl VerifiedConvergenceBundleV1 {
    pub(crate) fn binding(&self) -> &EvidenceBindingV1 {
        &self.binding
    }

    pub(crate) fn evidence(&self) -> &[EvidenceRecordV1] {
        &self.evidence
    }

    pub(crate) fn implementation_receipts(&self) -> &[ImplementationReceiptV1] {
        &self.implementation_receipts
    }

    pub(crate) fn reviews(&self) -> &[ReviewRecordV1] {
        &self.reviews
    }
}

/// A small owner facade shared by all product entry points.  It creates no
/// process and does not authorise a workflow phase; convergence remains the
/// workflow owner's separate authority.
pub struct VerificationOwnerV1<'a> {
    store: &'a VerificationStore,
    static_catalog: &'a StaticCapabilityCatalogV1,
}

impl<'a> VerificationOwnerV1<'a> {
    pub fn new(
        store: &'a VerificationStore,
        static_catalog: &'a StaticCapabilityCatalogV1,
    ) -> Self {
        Self {
            store,
            static_catalog,
        }
    }

    /// Stage 1: project exact active artifacts to needs, persist a complete
    /// inventory/candidates, then make one advisory judge batch for one need.
    /// The caller chooses the active need deterministically by index after
    /// persisting all of them; no model text selects an artifact ID.
    #[expect(
        clippy::too_many_arguments,
        reason = "host-observed verification inputs remain explicit at the owner boundary"
    )]
    pub async fn prepare_current(
        &self,
        active: ActiveVerificationArtifactsV1<'_>,
        binding: &VerificationBindingV1,
        inventory_entries: Vec<TestEntryV1>,
        hints: &CandidateHintsV1,
        selected_need_index: usize,
        decider: Option<&dyn Decider>,
        all_scope: AllTestsScopeV1,
    ) -> Result<PreparedVerificationV1, String> {
        binding.validate().map_err(|error| error.to_string())?;
        let needs = derive_verification_needs(active)?;
        let need_refs = needs
            .iter()
            .map(|need| {
                self.store
                    .put_need(binding, need)
                    .map_err(|error| error.to_string())
            })
            .collect::<Result<Vec<_>, _>>()?;
        let need = needs
            .get(selected_need_index)
            .ok_or("selected verification need index is outside the exact active set")?;
        let inventory = build_complete_rust_inventory(inventory_entries)?;
        let (inventory_ref, candidates, candidate_ref) =
            persist_inventory_and_candidates(self.store, binding, &inventory, hints)?;
        let selection = select_with_advisory_jev(
            decider,
            self.store,
            binding,
            need,
            &inventory,
            &candidates,
            all_scope,
        )
        .await;
        Ok(PreparedVerificationV1 {
            need_refs,
            inventory_ref,
            candidate_ref,
            selection,
        })
    }

    /// Stage 2a: expose the hash-bound existing-test selection to the host's
    /// existing test executor. `All` remains explicit and cannot be replaced
    /// by a stale narrow selection.
    pub fn existing_test_selection(prepared: &PreparedVerificationV1) -> &TestSelectionV1 {
        &prepared.selection.selection
    }

    /// The host runs only `rust_inventory_argv()` and supplies its bounded
    /// output here. `None` is intentionally not repaired with a partial
    /// listing; callers must use an explicit All-test selection.
    pub fn collect_rust_inventory(
        &self,
        listing: &RustInventoryListingV1,
    ) -> Option<super::TestInventoryV1> {
        collect_complete_rust_inventory(listing)
    }

    pub fn unavailable_inventory_selection(scope: AllTestsScopeV1) -> TestSelectionV1 {
        TestSelectionV1::All {
            scope,
            reason: AllReasonV1::IncompleteInventory,
        }
    }

    /// Parse and persist a static adapter result only through the registered
    /// catalog and the exact verification binding. The adapter cannot bring
    /// its own capability object or select a looser request identity.
    pub(crate) fn record_registered_static_outcome(
        &self,
        verification_binding: &VerificationBindingV1,
        invocation: &StaticAdapterInvocationV1,
        outcome: &StaticAdapterOutcomeV1,
        parser: &dyn StaticAdapterParserV1,
    ) -> Result<VerificationRecordRefV1, String> {
        let evidence = parse_registered_static_evidence(invocation, outcome, parser)?;
        self.store
            .put_registered_static_evidence(verification_binding, self.static_catalog, &evidence)
            .map_err(|error| error.to_string())
    }

    /// Stage 2b: map a common-runner outcome into one exact evidence record.
    pub(crate) fn common_case_evidence(
        &self,
        binding: &EvidenceBindingV1,
        criterion_id: &str,
        report: &RunReport,
        observed_at_unix_secs: u64,
    ) -> Result<VerifiedEvidenceRecordV1, String> {
        let record =
            common_case_evidence_records(binding, criterion_id, report, observed_at_unix_secs)?
                .into_iter()
                .next()
                .ok_or("common case report did not produce evidence")?;
        Ok(VerifiedEvidenceRecordV1 { record })
    }

    /// Convert the only permitted external exact-test output into opaque,
    /// convergence-ready evidence. The executor cannot supply a criterion,
    /// binding, or raw output independently.
    pub(crate) fn existing_test_evidence(
        &self,
        binding: &EvidenceBindingV1,
        criterion_id: &str,
        plan: &RustTestExecutionPlanV1,
        outcome: &RustTestExecutionOutcomeV1,
        observed_at_unix_secs: u64,
    ) -> Result<VerifiedEvidenceRecordV1, String> {
        let record = existing_test_evidence_record(
            binding,
            criterion_id,
            plan,
            outcome,
            observed_at_unix_secs,
        )?;
        Ok(VerifiedEvidenceRecordV1 { record })
    }

    /// Stage 2c: resolve a static analyzer solely from the registered host
    /// catalog and project a complete satisfied result to evidence.
    pub(crate) fn static_evidence(
        &self,
        binding: &EvidenceBindingV1,
        criterion_id: &str,
        reference: &VerificationRecordRefV1,
        verification_binding: &VerificationBindingV1,
        observed_at_unix_secs: u64,
    ) -> Result<VerifiedEvidenceRecordV1, String> {
        let evidence = self
            .store
            .load_registered_static_evidence(reference, verification_binding, self.static_catalog)
            .map_err(|error| error.to_string())?;
        let record = static_satisfied_evidence_record(
            binding,
            criterion_id,
            &evidence,
            self.static_catalog,
            observed_at_unix_secs,
        )?;
        Ok(VerifiedEvidenceRecordV1 { record })
    }

    /// Stage 3: reduce all sources to the one exact record that convergence
    /// accepts. The session/legacy/Desktop workflow owner then passes this
    /// record through `validate_owner_convergence` before checkpointing it.
    pub(crate) fn converge_criterion(
        &self,
        binding: &EvidenceBindingV1,
        criterion_id: &str,
        sources: &[VerifiedEvidenceRecordV1],
        observed_at_unix_secs: u64,
    ) -> Result<VerifiedConvergenceBundleV1, String> {
        let records = sources
            .iter()
            .map(|source| source.record.clone())
            .collect::<Vec<_>>();
        let record =
            aggregate_criterion_evidence(binding, criterion_id, &records, observed_at_unix_secs)?;
        Ok(VerifiedConvergenceBundleV1 {
            binding: binding.clone(),
            implementation_receipts: Vec::new(),
            reviews: Vec::new(),
            evidence: vec![record],
        })
    }

    /// Add one receipt whose implementation identity is the host-observed
    /// current subject hash already bound to this bundle. No caller may supply
    /// an implementation hash independently.
    pub(crate) fn add_current_implementation_receipt(
        &self,
        mut bundle: VerifiedConvergenceBundleV1,
        observed_at_unix_secs: u64,
    ) -> Result<VerifiedConvergenceBundleV1, String> {
        bundle.binding.validate()?;
        let receipt = ImplementationReceiptV1 {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            receipt_id: stable_owner_id("receipt", &bundle.binding),
            binding: bundle.binding.clone(),
            implementation_hash: bundle.binding.subject_hash.clone(),
            authority: ImplementationReceiptAuthorityV1::TrustedOwner,
            issued_at_unix_secs: observed_at_unix_secs,
        };
        receipt.validate()?;
        if bundle
            .implementation_receipts
            .iter()
            .all(|current| current.receipt_id != receipt.receipt_id)
        {
            bundle.implementation_receipts.push(receipt);
        }
        Ok(bundle)
    }

    /// Resolve an immutable review record from the trusted store.  Raw review
    /// bodies never enter a bundle and cannot be surfaced through agent tools.
    pub(crate) fn add_review_reference(
        &self,
        mut bundle: VerifiedConvergenceBundleV1,
        verification_binding: &VerificationBindingV1,
        reference: &VerificationRecordRefV1,
    ) -> Result<VerifiedConvergenceBundleV1, String> {
        let review = self
            .store
            .load_review_record(reference, verification_binding, &bundle.binding)
            .map_err(|error| error.to_string())?;
        if bundle
            .reviews
            .iter()
            .all(|current| current.record_id != review.record_id)
        {
            bundle.reviews.push(review);
        }
        Ok(bundle)
    }

    /// Rebuild the sealed convergence input from final immutable evidence.
    /// This is used after a terminal checkpoint is restored; no runner output
    /// or model-provided result is needed or accepted on that path.
    #[expect(dead_code, reason = "focused evidence tests retain this helper")]
    pub(crate) fn bundle_from_final_evidence(
        &self,
        binding: &EvidenceBindingV1,
        criterion_id: &str,
        evidence: EvidenceRecordV1,
        observed_at_unix_secs: u64,
    ) -> Result<VerifiedConvergenceBundleV1, String> {
        if evidence.binding != *binding || evidence.criterion_id != criterion_id {
            return Err("final evidence does not match the terminal checkpoint".into());
        }
        let bundle = self.converge_criterion(
            binding,
            criterion_id,
            &[VerifiedEvidenceRecordV1 { record: evidence }],
            observed_at_unix_secs,
        )?;
        self.add_current_implementation_receipt(bundle, observed_at_unix_secs)
    }

    /// Rebuild one sealed bundle containing exactly one immutable final record
    /// for every criterion. Duplicate or mismatched criteria are rejected
    /// before the workflow owner sees the convergence input.
    pub(crate) fn bundle_from_final_evidence_records(
        &self,
        binding: &EvidenceBindingV1,
        evidence: &[EvidenceRecordV1],
        observed_at_unix_secs: u64,
    ) -> Result<VerifiedConvergenceBundleV1, String> {
        binding.validate()?;
        if evidence.is_empty() {
            return Err("final evidence set is empty".into());
        }
        let mut by_criterion = std::collections::BTreeMap::new();
        for record in evidence {
            record.validate()?;
            if record.binding != *binding
                || by_criterion
                    .insert(record.criterion_id.clone(), record.clone())
                    .is_some()
            {
                return Err("final evidence must contain one current record per criterion".into());
            }
        }
        let mut bundle = VerifiedConvergenceBundleV1 {
            binding: binding.clone(),
            implementation_receipts: Vec::new(),
            reviews: Vec::new(),
            evidence: by_criterion.into_values().collect(),
        };
        bundle = self.add_current_implementation_receipt(bundle, observed_at_unix_secs)?;
        Ok(bundle)
    }
}

fn stable_owner_id(kind: &str, binding: &EvidenceBindingV1) -> String {
    let bytes = serde_json::to_vec(&(kind, binding)).expect("bounded owner receipt serializes");
    let hash = crate::conversation_state::content_hash(&bytes);
    format!("{kind}-{}", &hash[..24])
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workflow::{
        ConvergenceInputV1, EvidenceProvenanceV1, EvidenceResultV1, EvidenceSourceKindV1,
        PhaseGateEvaluationV1, PhaseGateInputV1, ReviewRecordV1, WorkflowPhaseGateV1,
        evaluate_phase_gate,
    };

    fn hash(byte: char) -> String {
        std::iter::repeat_n(byte, 64).collect()
    }

    fn evidence_binding() -> EvidenceBindingV1 {
        EvidenceBindingV1 {
            task_id: "task-1".into(),
            task_definition_hash: hash('a'),
            subject_hash: hash('b'),
            source_hash: hash('c'),
            diff_hash: hash('d'),
            artifact_graph_basis_hash: hash('e'),
        }
    }

    fn verification_binding() -> VerificationBindingV1 {
        VerificationBindingV1 {
            project_graph_hash: hash('1'),
            analyzer_hash: hash('2'),
            tool_hash: hash('3'),
            source_hash: hash('c'),
            diff_hash: hash('d'),
            config_hash: hash('4'),
            analysis_request_hash: hash('5'),
        }
    }

    #[test]
    fn sealed_bundle_with_current_receipt_persisted_review_and_evidence_passes_all_gates() {
        let root = tempfile::tempdir().unwrap();
        let store = VerificationStore::open(root.path(), "owner-test").unwrap();
        let catalog = StaticCapabilityCatalogV1::default();
        let owner = VerificationOwnerV1::new(&store, &catalog);
        let binding = evidence_binding();
        let verification = verification_binding();
        let evidence = VerifiedEvidenceRecordV1 {
            record: EvidenceRecordV1 {
                schema_version: EVIDENCE_SCHEMA_VERSION,
                record_id: "evidence-source-1".into(),
                binding: binding.clone(),
                criterion_id: "criterion-1".into(),
                result: EvidenceResultV1::Passed,
                provenance: EvidenceProvenanceV1 {
                    source_kind: EvidenceSourceKindV1::TestExecution,
                    source_ref: "test:criterion-1".into(),
                    observed_at_unix_secs: 1,
                },
            },
        };
        let bundle = owner
            .converge_criterion(&binding, "criterion-1", &[evidence], 2)
            .unwrap();
        let bundle = owner.add_current_implementation_receipt(bundle, 3).unwrap();
        let review = ReviewRecordV1 {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            record_id: "review-1".into(),
            binding: binding.clone(),
            findings: Vec::new(),
        };
        let review_ref = store
            .put_review_record(&verification, &binding, &review)
            .unwrap();
        let bundle = owner
            .add_review_reference(bundle, &verification, &review_ref)
            .unwrap();
        let convergence = ConvergenceInputV1 {
            schema_version: EVIDENCE_SCHEMA_VERSION,
            binding: bundle.binding().clone(),
            required_criterion_ids: vec!["criterion-1".into()],
            implementation_receipts: bundle.implementation_receipts().to_vec(),
            reviews: bundle.reviews().to_vec(),
            evidence: bundle.evidence().to_vec(),
        };
        for transition in [
            WorkflowPhaseGateV1::ImplementToReview,
            WorkflowPhaseGateV1::ReviewToVerify,
            WorkflowPhaseGateV1::VerifyToDeliver,
        ] {
            assert!(matches!(
                evaluate_phase_gate(&PhaseGateInputV1 {
                    schema_version: EVIDENCE_SCHEMA_VERSION,
                    transition,
                    convergence: convergence.clone(),
                }),
                PhaseGateEvaluationV1::Allowed(_)
            ));
        }
    }
}
