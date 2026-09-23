//! Regression tests for verification selection, execution, evidence, and convergence.

use std::collections::{BTreeMap, BTreeSet};

use super::*;

fn hash(byte: char) -> String {
    std::iter::repeat_n(byte, 64).collect()
}

fn entry(id: &str) -> TestEntryV1 {
    TestEntryV1 {
        id: id.into(),
        framework: "rust".into(),
        package: "core".into(),
        target: "lib".into(),
        exact_name: format!("suite::{id}"),
        source_location: Some(SourceLocationV1 {
            path: "src/lib.rs".into(),
            line: 1,
        }),
        dynamic_or_unknown: false,
        cost: TestCostClassV1::Fast,
        symbols: BTreeSet::new(),
        common_case_ids: BTreeSet::new(),
    }
}
fn inventory(entries: Vec<TestEntryV1>) -> TestInventoryV1 {
    TestInventoryV1::complete_rust(entries).unwrap()
}
fn record(ids: &[&str]) -> TestDecisionRecordV1 {
    let candidate_ids: Vec<String> = ids.iter().map(|id| (*id).into()).collect();
    TestDecisionRecordV1 {
        schema_version: SCHEMA_VERSION,
        record_id: "r".into(),
        provider: "jev".into(),
        model: "latest".into(),
        question_hash: "q".into(),
        diff_hash: "d".into(),
        inventory_hash: inventory(candidate_ids.iter().map(|id| entry(id)).collect())
            .inventory_hash,
        candidate_hash: stable_id_hash(&candidate_ids),
        answers: candidate_ids
            .into_iter()
            .map(|candidate_id| TestScoreAnswerV1 {
                candidate_id,
                score: 3.0,
                probabilities: [
                    ("0".into(), 0.0),
                    ("1".into(), 0.0),
                    ("2".into(), 0.0),
                    ("3".into(), 1.0),
                ]
                .into_iter()
                .collect(),
                confidence: Some(1.0),
            })
            .collect(),
        usage: DecisionUsageV1::default(),
    }
}

#[test]
fn changed_test_is_never_dropped_by_bm25() {
    let inventory = inventory(vec![entry("changed"), entry("ranked")]);
    let candidates = candidate_union(
        &inventory,
        &CandidateHintsV1 {
            changed_test_ids: ["changed".into()].into_iter().collect(),
            bm25_ranked_ids: vec!["ranked".into()],
            ..Default::default()
        },
    );
    assert_eq!(candidates.ids, vec!["changed"]);
    assert_eq!(
        candidates.mandatory_ids,
        ["changed".into()].into_iter().collect()
    );
}

#[test]
fn changed_test_bypasses_low_jev_score() {
    let inventory = inventory(vec![entry("changed"), entry("ranked")]);
    let candidates = CandidateSetV1 {
        ids: vec!["changed".into(), "ranked".into()],
        mandatory_ids: ["changed".into()].into_iter().collect(),
        inventory_complete: true,
    };
    let mut decision = record(&["changed", "ranked"]);
    decision.inventory_hash = inventory.inventory_hash.clone();
    decision.answers[0].score = 0.0;
    decision.answers[0].probabilities = [
        ("0".into(), 1.0),
        ("1".into(), 0.0),
        ("2".into(), 0.0),
        ("3".into(), 0.0),
    ]
    .into_iter()
    .collect();
    assert_eq!(
        select_tests_offline(
            Some(&decision),
            &inventory,
            &candidates,
            "q",
            "d",
            SCORE_CUTOFF,
            AllTestsScopeV1::Workspace,
        ),
        TestSelectionV1::Exact {
            ids: vec!["changed".into(), "ranked".into()]
        }
    );
}

#[test]
fn dynamic_tests_remain_candidates_without_promoting_locationless_tests() {
    let mut dynamic = entry("dynamic");
    dynamic.dynamic_or_unknown = true;
    let mut locationless = entry("locationless");
    locationless.source_location = None;
    let candidates = candidate_union(
        &inventory(vec![dynamic, locationless]),
        &CandidateHintsV1::default(),
    );
    assert_eq!(candidates.ids, vec!["dynamic"]);
}

#[test]
fn inventory_hash_rejects_tampered_entries() {
    let mut inventory = inventory(vec![entry("one")]);
    inventory.entries[0].exact_name = "suite::tampered".into();
    assert!(inventory.validate().is_err());
}

#[test]
fn weighted_score_is_preserved_for_offline_cutoff() {
    let inventory = inventory(vec![entry("one")]);
    let candidates = CandidateSetV1 {
        ids: vec!["one".into()],
        mandatory_ids: BTreeSet::new(),
        inventory_complete: true,
    };
    let mut decision = record(&["one"]);
    decision.inventory_hash = inventory.inventory_hash.clone();
    decision.answers[0].score = 1.99;
    decision.answers[0].probabilities = [
        ("0".into(), 0.0),
        ("1".into(), 0.01),
        ("2".into(), 0.99),
        ("3".into(), 0.0),
    ]
    .into_iter()
    .collect();
    assert!(matches!(
        select_tests_offline(
            Some(&decision),
            &inventory,
            &candidates,
            "q",
            "d",
            SCORE_CUTOFF,
            AllTestsScopeV1::Package
        ),
        TestSelectionV1::All {
            reason: AllReasonV1::NoSelectedTests,
            ..
        }
    ));
}

#[test]
fn invalid_judge_expands_to_explicit_all() {
    let inventory = inventory(vec![entry("one")]);
    let candidates = CandidateSetV1 {
        ids: vec!["one".into()],
        mandatory_ids: BTreeSet::new(),
        inventory_complete: true,
    };
    let mut decision = record(&["one"]);
    decision.answers[0].score = 4.0;
    assert!(matches!(
        select_tests_offline(
            Some(&decision),
            &inventory,
            &candidates,
            "q",
            "d",
            SCORE_CUTOFF,
            AllTestsScopeV1::Package
        ),
        TestSelectionV1::All {
            reason: AllReasonV1::InvalidJudge,
            ..
        }
    ));
}

#[test]
fn hash_mismatch_refuses_reuse() {
    let inventory = inventory(vec![entry("one")]);
    let candidates = CandidateSetV1 {
        ids: vec!["one".into()],
        mandatory_ids: BTreeSet::new(),
        inventory_complete: true,
    };
    assert!(matches!(
        select_tests_offline(
            Some(&record(&["one"])),
            &inventory,
            &candidates,
            "q",
            "changed",
            SCORE_CUTOFF,
            AllTestsScopeV1::Workspace
        ),
        TestSelectionV1::All {
            reason: AllReasonV1::HashMismatch,
            ..
        }
    ));
}

fn evidence(id: &str, state: StaticEvidenceStateV1) -> StaticEvidenceV1 {
    StaticEvidenceV1 {
        schema_version: SCHEMA_VERSION,
        evidence_id: id.into(),
        obligation_id: "criterion".into(),
        obligation_kind: "boundary".into(),
        analyzer_id: "a".into(),
        rule_id: "r".into(),
        tool_hash: hash('a'),
        config_hash: hash('c'),
        source_hash: hash('b'),
        diff_hash: hash('d'),
        scope: AnalysisScopeV1::Workspace,
        scope_completeness: ScopeCompletenessV1::Complete,
        project_graph_basis: None,
        state,
        findings: vec![],
        error_kind: None,
        duration_ms: None,
        cache_hit: None,
    }
}

fn capability(requires_project_graph: bool) -> StaticCapabilityV1 {
    StaticCapabilityV1 {
        schema_version: SCHEMA_VERSION,
        analyzer_id: "a".into(),
        executable_hash: hash('a'),
        version: "1".into(),
        output_schema_hash: hash('e'),
        rules: vec![StaticRuleCapabilityV1 {
            rule_id: "r".into(),
            obligation_kinds: ["boundary".into()].into_iter().collect(),
            maximum_scope: AnalysisScopeV1::Workspace,
            clean_proves_satisfied: true,
            requires_project_graph,
        }],
    }
}

#[test]
fn conflict_is_unknown_and_keeps_sources() {
    let capability = StaticCapabilityV1 {
        schema_version: SCHEMA_VERSION,
        analyzer_id: "a".into(),
        executable_hash: hash('x'),
        version: "1".into(),
        output_schema_hash: hash('o'),
        rules: vec![StaticRuleCapabilityV1 {
            rule_id: "r".into(),
            obligation_kinds: ["boundary".into()].into_iter().collect(),
            maximum_scope: AnalysisScopeV1::Workspace,
            clean_proves_satisfied: true,
            requires_project_graph: false,
        }],
    };
    let merged = merge_static_evidence(
        vec![
            evidence("one", StaticEvidenceStateV1::Satisfied),
            evidence("two", StaticEvidenceStateV1::Violated),
        ],
        &[('a'.to_string(), capability)]
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
    );
    assert_eq!(merged[0].state, StaticEvidenceStateV1::Unknown);
    assert_eq!(merged[0].sources.len(), 2);
}

#[test]
fn clean_without_complete_registered_scope_is_not_satisfied() {
    let mut raw = evidence("one", StaticEvidenceStateV1::Satisfied);
    raw.scope_completeness = ScopeCompletenessV1::Partial;
    let capability = StaticCapabilityV1 {
        schema_version: SCHEMA_VERSION,
        analyzer_id: "a".into(),
        executable_hash: hash('x'),
        version: "1".into(),
        output_schema_hash: hash('o'),
        rules: vec![StaticRuleCapabilityV1 {
            rule_id: "r".into(),
            obligation_kinds: ["boundary".into()].into_iter().collect(),
            maximum_scope: AnalysisScopeV1::Workspace,
            clean_proves_satisfied: true,
            requires_project_graph: false,
        }],
    };
    assert_eq!(
        raw.effective_state(Some(&capability)),
        StaticEvidenceStateV1::Unknown
    );
}

#[test]
fn clean_static_evidence_requires_a_valid_record_and_exact_tool_hash() {
    let raw = evidence("one", StaticEvidenceStateV1::Satisfied);
    assert_eq!(
        raw.effective_state(Some(&capability(false))),
        StaticEvidenceStateV1::Satisfied
    );

    let mut wrong_hash = raw.clone();
    wrong_hash.tool_hash = hash('z');
    assert_eq!(
        wrong_hash.effective_state(Some(&capability(false))),
        StaticEvidenceStateV1::Unknown
    );

    let mut malformed = raw;
    malformed.findings.push(FindingV1 {
        fingerprint: String::new(),
        rule_id: "r".into(),
        path: None,
        line: None,
        symbol: None,
        introduced_by_current_diff: false,
        baseline: false,
    });
    assert_eq!(
        malformed.effective_state(Some(&capability(false))),
        StaticEvidenceStateV1::Unknown
    );
}

#[test]
fn static_scope_and_error_must_fit_registered_capability() {
    let mut file_only = capability(false);
    file_only.rules[0].maximum_scope = AnalysisScopeV1::File;
    let mut workspace = evidence("workspace", StaticEvidenceStateV1::Satisfied);
    workspace.scope = AnalysisScopeV1::Workspace;
    assert_eq!(
        workspace.effective_state(Some(&file_only)),
        StaticEvidenceStateV1::Unknown
    );

    let mut errored = evidence("errored", StaticEvidenceStateV1::Satisfied);
    errored.error_kind = Some("timeout".into());
    assert_eq!(
        errored.effective_state(Some(&capability(false))),
        StaticEvidenceStateV1::Unknown
    );
}

#[test]
fn static_binding_identity_is_derived_from_registered_capability() {
    let capability = capability(false);
    let binding = store::VerificationBindingV1::for_registered_static(
        &capability,
        hash('1'),
        hash('2'),
        hash('d'),
        hash('c'),
        hash('3'),
    )
    .unwrap();
    assert_eq!(binding.tool_hash, capability.executable_hash);
    assert_eq!(
        binding.analyzer_hash,
        crate::conversation_state::content_hash(&serde_json::to_vec(&capability).unwrap())
    );
    assert_ne!(binding.analyzer_hash, binding.project_graph_hash);
}

#[test]
fn capability_catalog_rejects_forged_or_duplicate_capabilities() {
    let mut catalog = StaticCapabilityCatalogV1::default();
    let mut forged = capability(false);
    forged.executable_hash = "not-a-hash".into();
    assert!(catalog.register(forged).is_err());
    catalog.register(capability(false)).unwrap();
    assert!(catalog.register(capability(false)).is_err());
}

#[test]
fn graph_rules_require_complete_graph_basis() {
    let raw = evidence("one", StaticEvidenceStateV1::Satisfied);
    assert_eq!(
        raw.effective_state(Some(&capability(true))),
        StaticEvidenceStateV1::Unknown
    );

    let mut partial = raw.clone();
    partial.project_graph_basis = Some(ProjectGraphBasisV1 {
        graph_hash: "graph".into(),
        complete: false,
    });
    assert_eq!(
        partial.effective_state(Some(&capability(true))),
        StaticEvidenceStateV1::Unknown
    );

    let mut complete = raw;
    complete.project_graph_basis = Some(ProjectGraphBasisV1 {
        graph_hash: "graph".into(),
        complete: true,
    });
    assert_eq!(
        complete.effective_state(Some(&capability(true))),
        StaticEvidenceStateV1::Satisfied
    );
}

#[test]
fn every_finding_is_retained_in_deterministic_merge_groups() {
    let mut raw = evidence("one", StaticEvidenceStateV1::Violated);
    raw.findings = vec![
        FindingV1 {
            fingerprint: "zeta".into(),
            rule_id: "r".into(),
            path: Some("src/z.rs".into()),
            line: Some(1),
            symbol: None,
            introduced_by_current_diff: true,
            baseline: false,
        },
        FindingV1 {
            fingerprint: "alpha".into(),
            rule_id: "r".into(),
            path: Some("src/a.rs".into()),
            line: Some(1),
            symbol: None,
            introduced_by_current_diff: true,
            baseline: false,
        },
    ];
    let merged = merge_static_evidence(
        vec![raw],
        &[("a".to_string(), capability(false))]
            .into_iter()
            .collect::<BTreeMap<_, _>>(),
    );
    assert_eq!(
        merged
            .iter()
            .map(|item| item.fingerprint.as_str())
            .collect::<Vec<_>>(),
        vec!["alpha", "zeta"]
    );
    assert!(merged.iter().all(|item| item.sources.len() == 1));
}

#[test]
fn clean_and_finding_for_same_obligation_conflict_in_same_group() {
    let clean = evidence("clean", StaticEvidenceStateV1::Satisfied);
    let mut finding = evidence("finding", StaticEvidenceStateV1::Violated);
    finding.findings = vec![FindingV1 {
        fingerprint: "same-obligation-finding".into(),
        rule_id: "r".into(),
        path: Some("src/lib.rs".into()),
        line: Some(1),
        symbol: None,
        introduced_by_current_diff: true,
        baseline: false,
    }];
    let merged = merge_static_evidence(
        vec![clean, finding],
        &[("a".into(), capability(false))].into_iter().collect(),
    );
    assert_eq!(merged.len(), 1);
    assert_eq!(merged[0].state, StaticEvidenceStateV1::Unknown);
    assert_eq!(merged[0].sources.len(), 2);
}

#[test]
fn finding_cannot_be_both_baseline_and_introduced() {
    let mut raw = evidence("invalid-baseline", StaticEvidenceStateV1::Violated);
    raw.findings = vec![FindingV1 {
        fingerprint: "conflict".into(),
        rule_id: "r".into(),
        path: Some("src/lib.rs".into()),
        line: Some(1),
        symbol: None,
        introduced_by_current_diff: true,
        baseline: true,
    }];
    assert!(raw.validate().is_err());
}

#[test]
fn invalid_inventory_and_candidate_sets_expand_to_all() {
    let mut invalid_inventory = inventory(vec![entry("one")]);
    invalid_inventory.entries.push(entry("one"));
    let valid_candidates = CandidateSetV1 {
        ids: vec!["one".into()],
        mandatory_ids: BTreeSet::new(),
        inventory_complete: true,
    };
    assert!(matches!(
        select_tests_offline(
            Some(&record(&["one"])),
            &invalid_inventory,
            &valid_candidates,
            "q",
            "d",
            SCORE_CUTOFF,
            AllTestsScopeV1::Package,
        ),
        TestSelectionV1::All {
            reason: AllReasonV1::InvalidInventory,
            ..
        }
    ));

    let valid_inventory = inventory(vec![entry("one")]);
    for ids in [vec!["one".into(), "one".into()], vec!["missing".into()]] {
        let invalid_candidates = CandidateSetV1 {
            ids,
            mandatory_ids: BTreeSet::new(),
            inventory_complete: true,
        };
        assert!(matches!(
            select_tests_offline(
                Some(&record(&["one"])),
                &valid_inventory,
                &invalid_candidates,
                "q",
                "d",
                SCORE_CUTOFF,
                AllTestsScopeV1::Package,
            ),
            TestSelectionV1::All {
                reason: AllReasonV1::InvalidCandidates,
                ..
            }
        ));
    }
}
