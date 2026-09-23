//! Regression tests for answer finalization identities, evidence, and delivery policy.

use super::*;

fn hash(byte: char) -> String {
    byte.to_string().repeat(64)
}

fn subject() -> FinalizationSubjectV1 {
    FinalizationSubjectV1 {
        schema_version: FINALIZATION_SCHEMA_VERSION,
        project_id: "project".into(),
        session_id: "session".into(),
        session_epoch: 2,
        lane_id: "main".into(),
        root_run_id: "run".into(),
        attempt_id: "attempt".into(),
        turn_sequence: 4,
        input_receipt_ids: vec!["receipt-1".into(), "receipt-2".into()],
        attachment_hashes: vec![hash('a')],
        workflow_revision: "workflow-v1".into(),
        exact_prompt_hash: hash('b'),
    }
}

fn candidate(subject: &FinalizationSubjectV1) -> FinalizationCandidateV1 {
    FinalizationCandidateV1::new(
        subject,
        "markdown-v1",
        "A complete answer.",
        vec![StructuredCitationV1 {
            citation_id: "citation-1".into(),
            destination: "https://example.com/source".into(),
            content_hash: hash('c'),
        }],
    )
    .unwrap()
}

fn identity() -> JudgeIdentityV1 {
    JudgeIdentityV1 {
        request_hash: hash('1'),
        profile_hash: hash('2'),
        candidate_hash: hash('3'),
        prompt_hash: hash('4'),
        closed_evidence_hash: hash('5'),
        provider: "fixture".into(),
        model: "fixture-model".into(),
        question_version: "fulfillment-v1".into(),
    }
}

fn expectation(stage: JudgeStageV1) -> JudgeExpectationV1 {
    let identity = identity();
    JudgeExpectationV1 {
        stage,
        request_hash: identity.request_hash,
        profile_hash: identity.profile_hash,
        candidate_hash: identity.candidate_hash,
        prompt_hash: identity.prompt_hash,
        closed_evidence_hash: identity.closed_evidence_hash,
        required_criterion_ids: vec!["content".into(), "language".into()],
        output_language_criterion_id: Some("language".into()),
        presentation_language: LanguageV1::Ja,
        translation: Some(TranslationDirectionV1::EnToJa),
    }
}

fn usage() -> Option<JudgeUsageV1> {
    Some(JudgeUsageV1 {
        input_tokens: 10,
        output_tokens: 2,
        latency_ms: 5,
    })
}

fn source_pass(direction: TranslationDirectionV1) -> SourceJudgeResultV1 {
    SourceJudgeResultV1 {
        verdict: ModelVerdictV1::Pass,
        criterion_results: vec![
            CriterionResultV1 {
                criterion_id: "content".into(),
                status: CriterionStatusV1::Met,
            },
            CriterionResultV1 {
                criterion_id: "language".into(),
                status: CriterionStatusV1::DeferredToDelivery,
            },
        ],
        source_language: direction.source(),
        delivery_requirement: DeliveryRequirementV1::DeferredToDelivery,
        format_match: true,
        evidence_support: true,
        failure_codes: Vec::new(),
        identity: identity(),
        usage: usage(),
    }
}

fn delivered_pass(direction: TranslationDirectionV1) -> DeliveredJudgeResultV1 {
    DeliveredJudgeResultV1 {
        verdict: ModelVerdictV1::Pass,
        criterion_results: vec![
            CriterionResultV1 {
                criterion_id: "content".into(),
                status: CriterionStatusV1::Met,
            },
            CriterionResultV1 {
                criterion_id: "language".into(),
                status: CriterionStatusV1::Met,
            },
        ],
        delivery_language: direction.target(),
        format_match: true,
        evidence_support: true,
        failure_codes: Vec::new(),
        identity: identity(),
        usage: usage(),
    }
}

#[test]
fn subject_and_candidate_hashes_bind_exact_ordered_identity() {
    let original = subject();
    let original_id = original.canonical_id().unwrap();
    let mut changed = original.clone();
    changed.input_receipt_ids.reverse();
    assert_ne!(original_id, changed.canonical_id().unwrap());

    let mut changed_candidate = candidate(&original);
    changed_candidate.body.push('!');
    assert_eq!(
        changed_candidate.validate(),
        Err(FinalizationError::InvalidCandidate)
    );

    let mut stale = subject();
    stale.lane_id = "sidechat".into();
    assert_eq!(
        AnswerFinalizerV1::capture(&stale, &candidate(&original), &hash('d'), &hash('e'))
            .unwrap_err(),
        FinalizationError::StaleBinding
    );
    assert_eq!(
        AnswerFinalizerV1::capture(
            &original,
            &candidate(&original),
            "stale-evidence",
            &hash('e'),
        )
        .unwrap_err(),
        FinalizationError::InvalidCaptureBinding
    );
}

#[test]
fn transition_table_rejects_every_unlisted_edge_and_retries() {
    use FinalizationStateV1 as S;
    let states = [
        S::Captured,
        S::SourceJudging,
        S::SourcePassed,
        S::Protecting,
        S::Translating,
        S::Restoring,
        S::IntegrityChecking,
        S::IntegrityPassed,
        S::DeliveredJudging,
        S::ReadyToPublish,
        S::Published,
        S::Blocked,
        S::Cancelled,
    ];
    let legal = [
        (S::Captured, S::SourceJudging),
        (S::Captured, S::Blocked),
        (S::SourceJudging, S::SourcePassed),
        (S::SourceJudging, S::Blocked),
        (S::SourcePassed, S::Protecting),
        (S::SourcePassed, S::IntegrityPassed),
        (S::SourcePassed, S::Blocked),
        (S::Protecting, S::Translating),
        (S::Protecting, S::Blocked),
        (S::Translating, S::Restoring),
        (S::Translating, S::DeliveredJudging),
        (S::Restoring, S::IntegrityChecking),
        (S::Restoring, S::DeliveredJudging),
        (S::IntegrityChecking, S::IntegrityPassed),
        (S::IntegrityChecking, S::DeliveredJudging),
        (S::IntegrityPassed, S::DeliveredJudging),
        (S::IntegrityPassed, S::Blocked),
        (S::DeliveredJudging, S::ReadyToPublish),
        (S::DeliveredJudging, S::Blocked),
        (S::ReadyToPublish, S::Published),
        (S::ReadyToPublish, S::Blocked),
    ];
    let template =
        AnswerFinalizerV1::capture(&subject(), &candidate(&subject()), &hash('d'), &hash('e'))
            .unwrap();
    for from in states {
        for to in states {
            let mut machine = template.clone();
            machine.state = from;
            machine.visits.clear();
            machine.visits.insert(from, 1);
            let accepted = machine.transition(to).is_ok();
            let expected = !from.terminal()
                && (legal.contains(&(from, to)) || to == S::Cancelled)
                && from != to;
            assert_eq!(accepted, expected, "{from:?} -> {to:?}");
        }
    }

    let mut machine = template;
    machine.transition(S::SourceJudging).unwrap();
    machine.transition(S::SourcePassed).unwrap();
    assert_eq!(
        machine.transition(S::SourceJudging),
        Err(FinalizationError::IllegalTransition {
            from: S::SourcePassed,
            to: S::SourceJudging,
        })
    );
}

#[test]
fn host_requires_the_exact_criterion_set_and_nonmissing_usage() {
    let expected = expectation(JudgeStageV1::Source);
    let valid = FulfillmentJudgeResultV1::Source(source_pass(TranslationDirectionV1::EnToJa));
    assert_eq!(valid.host_verdict(&expected), Ok(HostVerdictV1::Pass));

    let mut missing = source_pass(TranslationDirectionV1::EnToJa);
    missing.criterion_results.pop();
    assert_eq!(
        FulfillmentJudgeResultV1::Source(missing).host_verdict(&expected),
        Err(VerdictError::CriterionSetMismatch)
    );
    let mut extra = source_pass(TranslationDirectionV1::EnToJa);
    extra.criterion_results.push(CriterionResultV1 {
        criterion_id: "invented".into(),
        status: CriterionStatusV1::Met,
    });
    assert_eq!(
        FulfillmentJudgeResultV1::Source(extra).host_verdict(&expected),
        Err(VerdictError::CriterionSetMismatch)
    );
    let mut duplicate = source_pass(TranslationDirectionV1::EnToJa);
    duplicate.criterion_results[1].criterion_id = "content".into();
    assert_eq!(
        FulfillmentJudgeResultV1::Source(duplicate).host_verdict(&expected),
        Err(VerdictError::DuplicateCriterion)
    );
    let mut missing_usage = source_pass(TranslationDirectionV1::EnToJa);
    missing_usage.usage = None;
    assert_eq!(
        FulfillmentJudgeResultV1::Source(missing_usage).host_verdict(&expected),
        Err(VerdictError::UsageMissing)
    );
}

#[test]
fn language_direction_and_deferred_rule_are_symmetric() {
    for direction in [
        TranslationDirectionV1::EnToJa,
        TranslationDirectionV1::JaToEn,
    ] {
        let mut source_expected = expectation(JudgeStageV1::Source);
        source_expected.translation = Some(direction);
        source_expected.presentation_language = direction.target();
        assert_eq!(
            FulfillmentJudgeResultV1::Source(source_pass(direction)).host_verdict(&source_expected),
            Ok(HostVerdictV1::Pass)
        );
        let mut delivered_expected = source_expected.clone();
        delivered_expected.stage = JudgeStageV1::Delivered;
        assert_eq!(
            FulfillmentJudgeResultV1::Delivered(delivered_pass(direction))
                .host_verdict(&delivered_expected),
            Ok(HostVerdictV1::Pass)
        );

        let mut wrong_source = source_pass(direction);
        wrong_source.source_language = direction.target();
        wrong_source.verdict = ModelVerdictV1::Fail;
        assert_eq!(
            FulfillmentJudgeResultV1::Source(wrong_source).host_verdict(&source_expected),
            Ok(HostVerdictV1::Fail)
        );
        let mut illegal_defer = delivered_pass(direction);
        illegal_defer.criterion_results[1].status = CriterionStatusV1::DeferredToDelivery;
        illegal_defer.verdict = ModelVerdictV1::Fail;
        assert_eq!(
            FulfillmentJudgeResultV1::Delivered(illegal_defer).host_verdict(&delivered_expected),
            Err(VerdictError::IllegalDeliveredCriterion)
        );
    }
}

#[test]
fn contradictory_model_verdict_and_identity_drift_are_invalid() {
    let expected = expectation(JudgeStageV1::Source);
    let mut result = source_pass(TranslationDirectionV1::EnToJa);
    result.verdict = ModelVerdictV1::Fail;
    assert_eq!(
        FulfillmentJudgeResultV1::Source(result).host_verdict(&expected),
        Err(VerdictError::ContradictoryModelVerdict)
    );
    let mut drift = source_pass(TranslationDirectionV1::EnToJa);
    drift.identity.candidate_hash = hash('9');
    assert_eq!(
        FulfillmentJudgeResultV1::Source(drift).host_verdict(&expected),
        Err(VerdictError::IdentityMismatch)
    );
}

#[test]
fn untranslated_derivation_requires_identical_bindings_and_no_deferred_result() {
    let mut source_expected = expectation(JudgeStageV1::Source);
    source_expected.translation = None;
    source_expected.output_language_criterion_id = None;
    source_expected.presentation_language = LanguageV1::En;
    let mut delivered_expected = source_expected.clone();
    delivered_expected.stage = JudgeStageV1::Delivered;
    let mut source = source_pass(TranslationDirectionV1::EnToJa);
    source.source_language = LanguageV1::En;
    source.delivery_requirement = DeliveryRequirementV1::NotRequired;
    source.criterion_results[1].status = CriterionStatusV1::Met;
    let delivered =
        derive_untranslated_delivered_result(&source, &source_expected, &delivered_expected)
            .unwrap();
    assert_eq!(delivered.delivery_language, LanguageV1::En);

    let mut changed = delivered_expected;
    changed.candidate_hash = hash('f');
    assert_eq!(
        derive_untranslated_delivered_result(&source, &source_expected, &changed),
        Err(VerdictError::SeparateDeliveredJudgeRequired)
    );
}

#[test]
fn enforce_remains_locked_without_current_benchmark_decisions() {
    assert_eq!(
        validate_mode_activation(
            FinalizationMode::Enforce,
            &hash('1'),
            None,
            None,
            None,
            None
        ),
        Err(ActivationError::EnforceLocked)
    );
    assert!(
        validate_mode_activation(
            FinalizationMode::Shadow,
            "not-a-hash",
            Some("not-a-hash"),
            None,
            None,
            None
        )
        .is_ok()
    );
    let judge = BenchmarkDecisionV1 {
        decision_id: "judge".into(),
        profile_hash: hash('1'),
        passed: true,
    };
    assert!(
        validate_mode_activation(
            FinalizationMode::Enforce,
            &hash('1'),
            None,
            Some(&judge),
            None,
            None
        )
        .is_ok()
    );
    assert_eq!(
        validate_mode_activation(
            FinalizationMode::Enforce,
            &hash('1'),
            Some(&hash('2')),
            Some(&judge),
            None,
            None,
        ),
        Err(ActivationError::TranslationLocked)
    );
    let translation = BenchmarkDecisionV1 {
        decision_id: "translation".into(),
        profile_hash: hash('2'),
        passed: true,
    };
    let composite = CompositeActivationV1 {
        judge_decision_id: judge.decision_id.clone(),
        translation_decision_id: Some(translation.decision_id.clone()),
        plan_hash: composite_activation_plan_hash(
            &hash('1'),
            &hash('2'),
            &judge.decision_id,
            &translation.decision_id,
        ),
    };
    assert!(
        validate_mode_activation(
            FinalizationMode::Enforce,
            &hash('1'),
            Some(&hash('2')),
            Some(&judge),
            Some(&translation),
            Some(&composite),
        )
        .is_ok()
    );
    let mut stale = composite;
    stale.plan_hash = hash('9');
    assert_eq!(
        validate_mode_activation(
            FinalizationMode::Enforce,
            &hash('1'),
            Some(&hash('2')),
            Some(&judge),
            Some(&translation),
            Some(&stale),
        ),
        Err(ActivationError::CompositeMismatch)
    );
}

fn shadow_receipt(stage: ShadowReceiptStageV1) -> ShadowFinalizationReceiptV1 {
    ShadowFinalizationReceiptV1 {
        subject_id: hash('1'),
        judge_profile_hash: hash('2'),
        translation_profile_hash: None,
        source_message_id: "message".into(),
        stage,
        usage_missing: false,
    }
}

#[test]
fn restart_closes_inflight_shadow_once_and_never_retries() {
    for stage in [
        ShadowReceiptStageV1::Scheduled,
        ShadowReceiptStageV1::Running,
    ] {
        let mut receipt = shadow_receipt(stage);
        assert!(receipt.close_after_restart());
        assert_eq!(receipt.stage, ShadowReceiptStageV1::ShadowInterrupted);
        assert!(!receipt.close_after_restart());
        assert_eq!(
            receipt.transition(ShadowReceiptStageV1::Running),
            Err(FinalizationError::TerminalShadowReceipt)
        );
    }
    let mut completed = shadow_receipt(ShadowReceiptStageV1::Completed);
    assert!(!completed.close_after_restart());
}

#[test]
fn off_and_shadow_observation_cannot_change_delivery() {
    let published = ("message-id", "exact bytes\r\n");
    let observation = ShadowObservationV1 {
        subject_id: hash('1'),
        source_candidate_hash: hash('2'),
        source_verdict: Some(HostVerdictV1Serde::Fail),
        delivered_verdict: None,
        translation_attempted: true,
        fallback_used: true,
        integrity_passed: Some(false),
        usage_missing: true,
    };
    assert_eq!(
        preserve_delivery_for_non_enforcing_mode(FinalizationMode::Off, &published, None).unwrap(),
        published
    );
    assert_eq!(
        preserve_delivery_for_non_enforcing_mode(
            FinalizationMode::Shadow,
            &published,
            Some(&observation),
        )
        .unwrap(),
        published
    );
    assert_eq!(
        preserve_delivery_for_non_enforcing_mode(
            FinalizationMode::Enforce,
            &published,
            Some(&observation),
        ),
        Err(FinalizationError::EnforceCannotUseParityBypass)
    );
}

#[test]
fn protected_scanner_round_trips_commands_paths_ids_units_links_and_code() {
    let source = concat!(
        "Run `cargo test` against /tmp/project at v1.2.3 (25 ms).\r\n",
        "```sh\r\ngit status --short\r\n```\r\n",
        "See [docs](https://example.com/a) and [^proof]. UUID ",
        "123e4567-e89b-12d3-a456-426614174000.\r\n",
        "Evidence: exact tool log"
    );
    let document = protect_translation_input(source, &["exact tool log"]).unwrap();
    assert!(
        document
            .spans
            .iter()
            .any(|span| span.kind == ProtectedSpanKindV1::FencedCode)
    );
    assert!(
        document
            .spans
            .iter()
            .any(|span| span.kind == ProtectedSpanKindV1::InlineCode)
    );
    assert!(
        document
            .spans
            .iter()
            .any(|span| span.kind == ProtectedSpanKindV1::LinkDestination)
    );
    assert!(
        document
            .spans
            .iter()
            .any(|span| span.kind == ProtectedSpanKindV1::Citation)
    );
    assert!(
        document
            .spans
            .iter()
            .any(|span| span.kind == ProtectedSpanKindV1::ToolEvidence)
    );
    let translated = document
        .protected_text
        .replace("Run ", "実行 ")
        .replace("See ", "参照 ");
    let restored = restore_translation_output(&document, &translated).unwrap();
    for span in &document.spans {
        let exact = std::str::from_utf8(&span.exact_bytes).unwrap();
        assert!(restored.contains(exact));
    }
}

#[test]
fn restore_rejects_omission_duplication_reordering_and_mutation() {
    let document = protect_translation_input("Use `alpha` then `beta`.", &[]).unwrap();
    let first = &document.spans[0].sentinel;
    let second = &document.spans[1].sentinel;
    assert!(matches!(
        restore_translation_output(&document, &document.protected_text.replace(first, "")),
        Err(ProtectionError::SentinelCount)
    ));
    let duplicated = format!("{} {} {}", first, first, second);
    assert!(matches!(
        restore_translation_output(&document, &duplicated),
        Err(ProtectionError::SentinelCount | ProtectionError::DuplicateSentinel)
    ));
    let reordered = format!("{second} {first}");
    assert_eq!(
        restore_translation_output(&document, &reordered),
        Err(ProtectionError::SentinelOrderOrIdentity)
    );
    let mutated = document
        .protected_text
        .replacen("POLARIS", "ＰＯＬＡＲＩＳ", 1);
    assert_eq!(
        restore_translation_output(&document, &mutated),
        Err(ProtectionError::SentinelCount)
    );
}

#[test]
fn link_delimiter_damage_and_ambiguous_markdown_fail_closed() {
    let document =
        protect_translation_input("Read [the docs](https://example.com/a).", &[]).unwrap();
    let damaged = document.protected_text.replace("](", " - ");
    assert_eq!(
        restore_translation_output(&document, &damaged),
        Err(ProtectionError::StructuralIntegrity)
    );
    for unsupported in [
        "[label][reference]",
        "[^note]: definition",
        "| a | b |\n|---|---|",
        "$$x + y$$",
        "<div>raw</div>",
    ] {
        assert_eq!(
            protect_translation_input(unsupported, &[]),
            Err(ProtectionError::UnsupportedMarkdown),
            "{unsupported}"
        );
    }
}

#[test]
fn serde_rejects_extra_judge_fields() {
    let value = serde_json::json!({
        "stage": "delivered",
        "verdict": "pass",
        "criterion_results": [],
        "delivery_language": "ja",
        "format_match": true,
        "evidence_support": true,
        "failure_codes": [],
        "identity": {
            "request_hash": hash('1'),
            "profile_hash": hash('2'),
            "candidate_hash": hash('3'),
            "prompt_hash": hash('4'),
            "closed_evidence_hash": hash('5'),
            "provider": "fixture",
            "model": "model",
            "question_version": "v1"
        },
        "usage": { "input_tokens": 1, "output_tokens": 1, "latency_ms": 1 },
        "unexpected": true
    });
    assert!(serde_json::from_value::<FulfillmentJudgeResultV1>(value).is_err());
}
