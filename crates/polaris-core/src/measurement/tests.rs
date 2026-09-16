//! Pure records; nothing is sent and no file is read.
use super::*;

fn known(tokens: u64) -> Reported {
    Reported::Known { tokens }
}

fn attempt(id: &str, observed: Observed) -> Attempt {
    Attempt {
        attempt_id: id.into(),
        source: Source::Ledger,
        kind: Kind::Completion,
        retried: false,
        observed,
    }
}

/// The provider's own total is given by the caller, never derived from the
/// parts: deriving it here would only prove the module agrees with itself.
fn complete(input: u64, cached: u64, write: u64, output: u64, total: u64) -> Observed {
    Observed {
        input: known(input),
        cached_input: known(cached),
        cache_write: known(write),
        output: known(output),
        reasoning: known(0),
        reasoning_relation: Reasoning::IncludedInOutput,
        reported_total: known(total),
    }
}

#[test]
fn a_total_with_everything_reported_is_exact_and_counts_each_attempt_once() {
    let attempts = vec![
        // 1,000 input of which 800 was read and 50 written back, 200 output.
        attempt("a-1", complete(1_000, 800, 50, 200, 1_200)),
        attempt("a-2", complete(10, 0, 0, 5, 15)),
        // The same physical attempt read twice is one attempt.
        attempt("a-1", complete(1_000, 800, 50, 200, 1_200)),
    ];
    assert_eq!(
        total(&attempts),
        Total::Exact { tokens: 1_215 },
        "the cache figures are inside the input count and are not added again"
    );
    // The part billed at the full rate is what neither touched the cache.
    assert_eq!(
        complete(1_000, 800, 50, 200, 1_200).billed_input(),
        known(150)
    );
}

#[test]
fn an_unreported_part_is_never_read_as_zero() {
    let mut observed = complete(1_000, 800, 50, 200, 1_200);
    observed.cache_write = Reported::Unknown;
    let total = total(&[attempt("a-1", observed)]);
    assert_eq!(
        total,
        Total::Partial {
            known: 1_200,
            unresolved: vec![Unresolved::NotReported {
                category: Category::CacheWrite
            }],
        },
        "the token count is still right; the split it needs for a rate is not"
    );
    // There is no way to read a bare number out of an incomplete total.
    assert!(!matches!(total, Total::Exact { .. }));
    // A rate cannot be applied to a guess.
    assert_eq!(observed.billed_input(), Reported::Unknown);
}

#[test]
fn a_reasoning_count_is_added_only_when_it_is_known_to_be_outside_the_output() {
    let base = complete(100, 0, 0, 50, 150);
    let separate = Observed {
        reasoning: known(30),
        reasoning_relation: Reasoning::Separate,
        reported_total: known(180),
        ..base
    };
    assert_eq!(
        total(&[attempt("a-1", separate)]),
        Total::Exact { tokens: 180 }
    );

    // The same number, already inside the output count, is not added again.
    let included = Observed {
        reasoning: known(30),
        reasoning_relation: Reasoning::IncludedInOutput,
        ..base
    };
    assert_eq!(
        total(&[attempt("a-1", included)]),
        Total::Exact { tokens: 150 }
    );

    // Not knowing which it is leaves the count out and says so.
    let unclear = Observed {
        reasoning: known(30),
        reasoning_relation: Reasoning::Unknown,
        reported_total: Reported::Unknown,
        ..base
    };
    assert_eq!(
        total(&[attempt("a-1", unclear)]),
        Total::Partial {
            known: 150,
            unresolved: vec![Unresolved::ReasoningRelationUnknown],
        }
    );

    // No reasoning figure at all is different: there is nothing to add, and
    // the output count is still the whole of what was billed.
    let silent = Observed {
        reasoning: Reported::Unknown,
        reasoning_relation: Reasoning::Unknown,
        ..base
    };
    assert_eq!(
        total(&[attempt("a-1", silent)]),
        Total::Exact { tokens: 150 }
    );

    // Billed apart from the output but missing is a real gap.
    let missing = Observed {
        reasoning: Reported::Unknown,
        reasoning_relation: Reasoning::Separate,
        reported_total: Reported::Unknown,
        ..base
    };
    assert_eq!(
        total(&[attempt("a-1", missing)]),
        Total::Partial {
            known: 150,
            unresolved: vec![Unresolved::NotReported {
                category: Category::Reasoning
            }],
        }
    );
}

#[test]
fn parts_that_disagree_with_the_providers_own_total_are_reported() {
    // A ledger row whose declared total holds 2,500 tokens the parts do not
    // account for. The layers this module reads really produce this shape,
    // so the check must fire on it rather than on a hand-built record.
    let observed = Observed::from_ledger(&polaris_provider::attempts::UsageObservation::Known {
        input_tokens: 10_000,
        output_tokens: 500,
        total_tokens: 13_000,
        cached_tokens: 2_000,
    });
    let Total::Partial { known, unresolved } = total(&[attempt("a-1", observed)]) else {
        panic!("a contradiction is never an exact total");
    };
    assert_eq!(known, 10_500);
    assert!(
        unresolved.contains(&Unresolved::Contradictory),
        "{unresolved:?}"
    );
}

#[test]
fn the_cost_of_scoring_is_kept_apart_from_the_cost_of_the_work() {
    let attempts = vec![
        attempt("a-1", complete(100, 0, 0, 50, 150)),
        Attempt {
            kind: Kind::Grading,
            ..attempt("g-1", complete(10, 0, 0, 5, 15))
        },
    ];
    assert_eq!(total(&attempts), Total::Exact { tokens: 150 });
    assert_eq!(grading_total(&attempts), Total::Exact { tokens: 15 });
}

#[test]
fn the_ledgers_zero_cache_count_is_not_a_measurement() {
    let observed = Observed::from_ledger(&polaris_provider::attempts::UsageObservation::Known {
        input_tokens: 1_000,
        output_tokens: 200,
        total_tokens: 1_200,
        cached_tokens: 800,
    });
    assert_eq!(observed.input, known(1_000));
    assert_eq!(observed.cached_input, known(800));
    // The ledger holds neither of these, so neither is invented.
    assert_eq!(observed.cache_write, Reported::Unknown);
    assert_eq!(observed.reasoning, Reported::Unknown);
    assert_eq!(observed.reasoning_relation, Reasoning::Unknown);

    // The provider layer writes zero both when the cache was not used and
    // when the response carried no cache field, so zero proves nothing. Read
    // as a measurement it would bill the whole input at the full rate.
    let unreported = Observed::from_ledger(&polaris_provider::attempts::UsageObservation::Known {
        input_tokens: 9_708,
        output_tokens: 167,
        total_tokens: 9_875,
        cached_tokens: 0,
    });
    assert_eq!(unreported.cached_input, Reported::Unknown);
    assert_eq!(unreported.billed_input(), Reported::Unknown);
    let Total::Partial { known, unresolved } = total(&[attempt("a-1", unreported)]) else {
        panic!("an unreported cache split is never an exact total");
    };
    assert_eq!(known, 9_875, "the token count itself is still right");
    assert!(
        unresolved.contains(&Unresolved::NotReported {
            category: Category::CachedInput
        }),
        "{unresolved:?}"
    );

    assert_eq!(
        Observed::from_ledger(&polaris_provider::attempts::UsageObservation::Missing).output,
        Reported::Unknown
    );
}

#[test]
fn the_request_log_is_the_only_place_a_cache_write_is_reported() {
    let row = serde_json::json!({
        "attempt_id": "1234-5678-1",
        "usage": {
            "input_tokens": 1_000,
            "output_tokens": 200,
            "cache_read_tokens": 800,
            "cache_write_tokens": 64,
        },
    });
    let observed = Observed::from_metrics_row(&row);
    assert_eq!(observed.input, known(1_000));
    assert_eq!(observed.cached_input, known(800));
    assert_eq!(observed.cache_write, known(64));
    assert_eq!(observed.output, known(200));
    // Both cache figures are inside the input, so the full-rate part is what
    // is left after removing them.
    assert_eq!(observed.billed_input(), known(136));
    assert_eq!(
        total(&[attempt("a-1", observed)]),
        Total::Exact { tokens: 1_200 },
        "the cache write is not an amount beside the input"
    );

    // Without the cached part, how much was billed in full is not known.
    let partial = Observed::from_metrics_row(&serde_json::json!({
        "usage": { "input_tokens": 1_000, "output_tokens": 200 }
    }));
    assert_eq!(partial.cached_input, Reported::Unknown);
    assert_eq!(partial.billed_input(), Reported::Unknown);
    assert_eq!(partial.input, known(1_000));
    assert_eq!(partial.output, known(200));
    assert_eq!(
        Observed::from_metrics_row(&serde_json::json!({})),
        Observed::unknown()
    );
}

#[test]
fn two_observation_layers_are_never_added_to_one_total() {
    // The same call appears as `attempt-1` in the ledger and as
    // `1234-5678-1` in the request log; neither identifier matches the other.
    let attempts = vec![
        attempt("attempt-1", complete(100, 0, 0, 50, 150)),
        Attempt {
            source: Source::RequestLog,
            ..attempt("1234-5678-1", complete(100, 0, 0, 50, 150))
        },
    ];
    let Total::Partial { unresolved, .. } = total(&attempts) else {
        panic!("a sum over two layers is never exact");
    };
    assert!(unresolved.contains(&Unresolved::MixedSources));
}

fn conditions(arm: Arm) -> Conditions {
    Conditions {
        schema_version: SCHEMA_VERSION,
        arm,
        model: "gpt-6-astra".into(),
        effort: "high".into(),
        task_id: "mid-size-db".into(),
        instructions_digest: "polaris-instructions".into(),
        initial_files_digest: "files-0001".into(),
        allowed_operations: vec!["read".into(), "write".into()],
        max_seconds: 3_600,
        max_cost_microusd: 5_000_000,
        iteration: 1,
        toolchain: "rustc 1.90".into(),
        grading_basis_digest: "criteria-v1".into(),
        cache_setting: "cold:warmup-v1".into(),
        execution_order: 1,
    }
}

#[test]
fn the_same_conditions_give_the_same_digest_and_a_changed_instruction_gives_another() {
    let held = conditions(Arm::Polaris);
    let reordered = Conditions {
        allowed_operations: vec!["write".into(), "read".into(), "read".into()],
        ..held.clone()
    };
    assert_eq!(held.digest(), reordered.digest());
    assert_ne!(
        held.digest(),
        Conditions {
            instructions_digest: "polaris-instructions-v2".into(),
            ..held.clone()
        }
        .digest()
    );

    // Settings that cannot be recovered after the run change the digest.
    for changed in [
        Conditions {
            grading_basis_digest: "criteria-v2".into(),
            ..held.clone()
        },
        Conditions {
            cache_setting: "warmed:warmup-v1".into(),
            ..held.clone()
        },
    ] {
        assert_ne!(held.digest(), changed.digest(), "{changed:?}");
        assert!(!held.comparable_with(&changed));
    }

    // The two arms differ in their instructions and in nothing else. They
    // also necessarily run one after the other, which is recorded but does
    // not make them incomparable.
    let plain = Conditions {
        instructions_digest: "empty-agents-md".into(),
        execution_order: 2,
        ..conditions(Arm::PlainCodex)
    };
    assert!(held.comparable_with(&plain));
    assert!(
        !held.comparable_with(&Conditions {
            effort: "low".into(),
            ..plain
        }),
        "a different effort is a different measurement"
    );
}

fn criterion(id: &str, verdict: Verdict) -> Criterion {
    Criterion {
        id: id.into(),
        criterion_version: 1,
        verdict,
        artifact_hash: "sha256:abc".into(),
        evidence: "checks/db-01/result.json".into(),
        grader: Grader::Person {
            id: "reviewer-1".into(),
        },
        scored_unix_seconds: 1_789_000_000,
    }
}

#[test]
fn a_verdict_without_evidence_a_grader_or_a_time_is_not_a_verdict() {
    assert!(criterion("db-01", Verdict::Met).scored());
    for broken in [
        Criterion {
            evidence: "  ".into(),
            ..criterion("db-01", Verdict::Met)
        },
        // A verdict that does not say which requirement it is about.
        Criterion {
            id: String::new(),
            ..criterion("db-01", Verdict::Met)
        },
        // A verdict read against an unnamed version of the requirement.
        Criterion {
            criterion_version: 0,
            ..criterion("db-01", Verdict::Met)
        },
        Criterion {
            artifact_hash: String::new(),
            ..criterion("db-01", Verdict::Met)
        },
        Criterion {
            scored_unix_seconds: 0,
            ..criterion("db-01", Verdict::Met)
        },
        Criterion {
            grader: Grader::Model {
                version: "luna-1".into(),
                instructions_digest: String::new(),
            },
            ..criterion("db-01", Verdict::Met)
        },
    ] {
        assert!(!broken.scored(), "{broken:?}");
    }

    // An unscored criterion is never counted as met.
    let counted = completeness(&[
        criterion("db-01", Verdict::Met),
        criterion("db-02", Verdict::Stub),
        Criterion {
            evidence: String::new(),
            ..criterion("db-03", Verdict::Met)
        },
    ]);
    assert_eq!(counted.met, 1);
    assert_eq!(counted.criteria, 3);
    assert_eq!(counted.unscored, vec!["db-03".to_string()]);
}

#[test]
fn a_trial_says_how_far_it_actually_went() {
    let trial = Trial {
        stage: Stage::Synthetic,
        conditions: conditions(Arm::Polaris),
        actual: Actual {
            responded_model: Some("gpt-6-astra".into()),
            stopped_because: Some("課題完了".into()),
            ..Actual::default()
        },
        attempts: vec![attempt("a-1", complete(100, 0, 0, 50, 150))],
        criteria: vec![criterion("db-01", Verdict::Met)],
    };
    assert_eq!(trial.stage, Stage::Synthetic, "not a real measurement");
    assert_eq!(trial.total(), Total::Exact { tokens: 150 });
    assert_eq!(trial.completeness().met, 1);
    // The record is written down as it stands.
    let body = serde_json::to_string(&trial).unwrap();
    assert!(body.contains("\"stage\":\"synthetic\""), "{body}");
}
