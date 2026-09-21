//! The embedded table and exact money arithmetic; nothing is sent.
use super::super::{Attempt, Kind, Observed, Reasoning, Reported, Source, Unresolved};
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

fn observed(input: u64, cached: u64, write: u64, output: u64) -> Observed {
    Observed {
        input: known(input),
        cached_input: known(cached),
        cache_write: known(write),
        output: known(output),
        reasoning: known(0),
        reasoning_relation: Reasoning::IncludedInOutput,
        reported_total: Reported::Unknown,
    }
}

fn rates() -> Rates {
    Rates::embedded().expect("the embedded table must parse")
}

#[test]
fn the_embedded_table_holds_the_figures_the_scripts_use() {
    let rates = rates();
    assert_eq!(rates.schema_version, 1);
    let model = rates.model("gpt-6-astra").expect("the measured model");
    assert_eq!(model.rates_as_of, "2026-09-07");
    // 10 / 1 / 12.5 / 50 USD per million, held as whole micro-USD.
    assert_eq!(model.billed_input, Rate(10_000_000));
    assert_eq!(model.cache_read, Rate(1_000_000));
    assert_eq!(model.cache_write, Rate(12_500_000));
    assert_eq!(model.output, Rate(50_000_000));
    assert_eq!(model.long_context_above, 272_000);
    assert_eq!(model.long_input_multiplier, 2_000_000);
    assert_eq!(model.long_output_multiplier, 1_500_000);
}

#[test]
fn luna_children_use_their_own_rates_including_cache_and_long_context() {
    let rates = rates();
    let model = rates.model("gpt-5.6-luna").expect("the M23 child model");
    assert_eq!(model.rates_as_of, "2026-09-19");
    // 150 uncached, 800 cached, 50 cache writes, 200 output tokens.
    // $0.20 / $0.02 / $0.25 / $1.20 per million => 298.5 micro-USD,
    // rounded once to 299. Astra's rates would instead produce 12,925.
    assert_eq!(
        api_cost_of(&rates, "gpt-5.6-luna", &observed(1_000, 800, 50, 200)),
        Cost::Exact { micro_usd: 299 }
    );
    for (input, expected) in [(272_000, 55_600), (272_001, 110_600)] {
        assert_eq!(
            api_cost_of(&rates, "gpt-5.6-luna", &observed(input, 0, 0, 1_000)),
            Cost::Exact {
                micro_usd: expected
            }
        );
    }
    let mut missing_write = observed(1_000, 800, 0, 200);
    missing_write.cache_write = Reported::Unknown;
    assert!(matches!(
        api_cost_of(&rates, "gpt-5.6-luna", &missing_write),
        Cost::Partial { .. }
    ));
    assert_eq!(
        api_cost_interval(&rates, "gpt-5.6-luna", &missing_write),
        Some((296, 306))
    );
}

#[test]
fn a_model_the_table_does_not_name_is_never_priced_with_another_models_figures() {
    let attempts = [attempt("a-1", observed(1_000, 0, 0, 100))];
    assert_eq!(
        api_cost(&rates(), "some-unlisted-model", &attempts),
        Cost::Partial {
            known_micro_usd: 0,
            unresolved: vec![Unresolved::RateUnknown],
        }
    );
}

#[test]
fn each_share_of_the_input_is_charged_at_its_own_rate() {
    // 1,000 input: 800 read from the cache, 50 written to it, 150 at the
    // full rate. 200 output.
    // 150*10 + 800*1 + 50*12.5 + 200*50 = 1500 + 800 + 625 + 10000 per
    // million, that is 12,925 micro-USD / 1e6 → 12,925 pico... in whole
    // micro-USD: 0.012925 USD.
    let attempts = [attempt("a-1", observed(1_000, 800, 50, 200))];
    assert_eq!(
        api_cost(&rates(), "gpt-6-astra", &attempts),
        Cost::Exact { micro_usd: 12_925 }
    );
    assert_eq!(usd_text(12_925), "0.012925");
}

#[test]
fn a_long_request_is_charged_at_the_higher_rate_on_both_sides() {
    // Just over the limit, so input is doubled and output is one and a half.
    let long = observed(272_001, 0, 0, 1_000);
    let short = observed(272_000, 0, 0, 1_000);
    let Cost::Exact { micro_usd: raised } =
        api_cost(&rates(), "gpt-6-astra", &[attempt("a-1", long)])
    else {
        panic!("everything needed is known");
    };
    let Cost::Exact {
        micro_usd: ordinary,
    } = api_cost(&rates(), "gpt-6-astra", &[attempt("a-1", short)])
    else {
        panic!("everything needed is known");
    };
    // 272,001*10*2 + 1,000*50*1.5 against 272,000*10 + 1,000*50.
    assert_eq!(ordinary, 2_770_000);
    assert_eq!(raised, 5_515_020);
}

#[test]
fn a_missing_cache_figure_leaves_the_cost_incomplete_rather_than_cheap() {
    let mut held = observed(1_000, 800, 50, 200);
    held.cache_write = Reported::Unknown;
    let cost = api_cost(&rates(), "gpt-6-astra", &[attempt("a-1", held)]);
    let Cost::Partial {
        known_micro_usd,
        unresolved,
    } = cost
    else {
        panic!("a missing share is never an exact cost");
    };
    // The cache read and the output can still be priced; the full-rate share
    // cannot, because it is the input minus two figures and one is missing.
    assert_eq!(known_micro_usd, 10_800);
    assert!(unresolved.contains(&Unresolved::NotReported {
        category: Category::CacheWrite
    }));
    assert!(unresolved.contains(&Unresolved::NotReported {
        category: Category::Input
    }));
}

#[test]
fn the_shares_are_added_before_the_one_rounding_and_not_after() {
    // Two rates that each land on half a micro-USD for this token count.
    // Rounding them apart gives 1 + 1 = 2; the exact sum is 1.
    let table = r#"{"schema_version":1,"models":{"halves":{"rates_as_of":"x",
        "api_per_million":{"billed_input":"0","cache_read":"0.5","cache_write":"0.5","output":"0"},
        "long_context":{"input_tokens_above":1000000,"input_multiplier":"1","output_multiplier":"1"}}}}"#;
    let rates = Rates::parse(table).expect("the table parses");
    let held = observed(2, 1, 1, 0);
    assert_eq!(
        api_cost(&rates, "halves", &[attempt("a-1", held)]),
        Cost::Exact { micro_usd: 1 },
        "each share is half a micro-USD; together they are one, not two"
    );
}

#[test]
fn a_cache_split_larger_than_the_input_is_never_a_confirmed_cost() {
    // The provider's own breakdown disagrees with itself: 900 read plus 200
    // written out of 1,000 input. Saturating the remainder to zero would
    // produce a figure that reads as measured.
    let held = observed(1_000, 900, 200, 10);
    assert_eq!(held.billed_input(), Reported::Unknown);
    let Cost::Partial {
        known_micro_usd,
        unresolved,
    } = api_cost(&rates(), "gpt-6-astra", &[attempt("a-1", held)])
    else {
        panic!("a contradictory split is never an exact cost");
    };
    assert!(
        unresolved.contains(&Unresolved::Contradictory),
        "{unresolved:?}"
    );
    // None of the input figures is trusted, so none of them is in the known
    // part; the output is reported apart from the split and is still priced.
    assert_eq!(known_micro_usd, 500, "10 output tokens at 50 per million");
}

/// A model that has no cache at all. The rule is declared in the table, not
/// inferred from a run that happened to report nothing.
fn cacheless() -> Rates {
    let table = r#"{"schema_version":1,"models":{"no-cache":{"rates_as_of":"2026-09-16",
        "cache":"not_applicable",
        "api_per_million":{"billed_input":"10","cache_read":"0","output":"50"},
        "long_context":{"input_tokens_above":1000000,"input_multiplier":"1","output_multiplier":"1"}}}}"#;
    Rates::parse(table).expect("the table parses")
}

#[test]
fn a_model_without_a_cache_is_priced_from_the_table_rule_not_left_unknown() {
    let cacheless = cacheless();
    // Nothing is reported for either cache share, because there is no cache
    // to report. The whole input is billed at the full rate.
    let held = Observed {
        input: known(1_000),
        cached_input: Reported::Unknown,
        cache_write: Reported::Unknown,
        output: known(200),
        reasoning: known(0),
        reasoning_relation: Reasoning::IncludedInOutput,
        reported_total: Reported::Unknown,
    };
    assert_eq!(
        api_cost(&cacheless, "no-cache", &[attempt("a-1", held)]),
        Cost::Exact { micro_usd: 20_000 },
        "1,000 at 10 plus 200 at 50, per million"
    );
    // The observation itself is untouched: it still says nothing was reported.
    assert_eq!(held.cached_input, Reported::Unknown);
    assert_eq!(held.billed_input(), Reported::Unknown);
    // A model that does have a cache is left unknown by the same input.
    assert!(matches!(
        api_cost(&rates(), "gpt-6-astra", &[attempt("a-1", held)]),
        Cost::Partial { .. }
    ));
}

#[test]
fn a_cache_figure_from_a_model_that_has_no_cache_is_a_contradiction() {
    let rates = cacheless();
    for (cached, write) in [(known(1), Reported::Unknown), (Reported::Unknown, known(1))] {
        let held = Observed {
            input: known(1_000),
            cached_input: cached,
            cache_write: write,
            output: known(200),
            reasoning: known(0),
            reasoning_relation: Reasoning::IncludedInOutput,
            reported_total: Reported::Unknown,
        };
        let Cost::Partial {
            known_micro_usd,
            unresolved,
        } = api_cost(&rates, "no-cache", &[attempt("a-1", held)])
        else {
            panic!("a report contradicting the table is never an exact cost");
        };
        assert!(
            unresolved.contains(&Unresolved::Contradictory),
            "{unresolved:?}"
        );
        // The input side is not trusted at all; the output still is.
        assert_eq!(known_micro_usd, 10_000);
    }
    // Zero is what a cacheless model consistently reports, not a contradiction.
    let held = Observed {
        input: known(1_000),
        cached_input: known(0),
        cache_write: known(0),
        output: known(200),
        reasoning: known(0),
        reasoning_relation: Reasoning::IncludedInOutput,
        reported_total: Reported::Unknown,
    };
    assert_eq!(
        api_cost(&rates, "no-cache", &[attempt("a-1", held)]),
        Cost::Exact { micro_usd: 20_000 }
    );
}

#[test]
fn an_unreadable_cache_declaration_is_refused_rather_than_read_as_having_one() {
    let table = r#"{"schema_version":1,"models":{"m":{"rates_as_of":"x","cache":"maybe",
        "api_per_million":{"billed_input":"1","cache_read":"1","output":"1"},
        "long_context":{"input_tokens_above":1,"input_multiplier":"1","output_multiplier":"1"}}}}"#;
    assert!(Rates::parse(table).is_err());
}

#[test]
fn every_entry_point_returns_whole_micro_usd() {
    // 1,000 input of which 800 was read and 50 written, 200 output:
    // 150*10 + 800*1 + 50*12.5 + 200*50 = 12,925 micro-USD.
    let rates = rates();
    let held = observed(1_000, 800, 50, 200);
    assert_eq!(
        api_cost_of(&rates, "gpt-6-astra", &held),
        Cost::Exact { micro_usd: 12_925 }
    );
    assert_eq!(
        api_cost(&rates, "gpt-6-astra", &[attempt("a-1", held)]),
        Cost::Exact { micro_usd: 12_925 },
        "one attempt costs the same either way"
    );
    // The bounds price the 200 tokens that touched no cache at the full rate
    // and at the cache-write rate: 800*1 + 200*10 + 200*50 = 12,800 and
    // 800*1 + 200*12.5 + 200*50 = 13,300.
    assert_eq!(
        api_cost_interval(&rates, "gpt-6-astra", &held),
        Some((12_800, 13_300))
    );
    assert_eq!(api_cost_upper(&rates, "gpt-6-astra", &held), Some(13_300));
    assert_eq!(usd(12_925), 0.012_925);
}

#[test]
fn a_total_equals_the_sum_of_the_records_it_is_made_of() {
    // Each attempt is rounded before it is added, because the aggregation
    // script rounds each request and the records hold those per-request
    // figures. Rounding only at the very end would not match them.
    let rates = rates();
    let odd = observed(100, 0, 1, 0);
    let Cost::Exact { micro_usd: one } = api_cost_of(&rates, "gpt-6-astra", &odd) else {
        panic!("everything needed is known");
    };
    assert_eq!(one, 1_003, "1,002.5 rounds to 1,003");
    assert_eq!(
        api_cost(
            &rates,
            "gpt-6-astra",
            &[attempt("a-1", odd), attempt("a-2", odd)]
        ),
        Cost::Exact { micro_usd: 2_006 }
    );
}

#[test]
fn reasoning_outside_the_output_is_charged_and_reasoning_inside_it_is_not() {
    let base = observed(100, 0, 0, 1_000);
    let separate = Observed {
        reasoning: known(1_000),
        reasoning_relation: Reasoning::Separate,
        ..base
    };
    let Cost::Exact { micro_usd: with } =
        api_cost(&rates(), "gpt-6-astra", &[attempt("a-1", separate)])
    else {
        panic!("everything needed is known");
    };
    let Cost::Exact {
        micro_usd: without, ..
    } = api_cost(&rates(), "gpt-6-astra", &[attempt("a-1", base)])
    else {
        panic!("everything needed is known");
    };
    // The separate 1,000 reasoning tokens are billed at the output rate.
    assert_eq!(with - without, 50_000);
}

#[test]
fn scoring_the_work_is_not_part_of_what_the_work_cost() {
    let attempts = [
        attempt("a-1", observed(1_000, 0, 0, 100)),
        Attempt {
            kind: Kind::Grading,
            ..attempt("g-1", observed(500, 0, 0, 50))
        },
    ];
    let rates = rates();
    assert_eq!(
        api_cost(&rates, "gpt-6-astra", &attempts),
        Cost::Exact { micro_usd: 15_000 }
    );
    assert_eq!(
        grading_api_cost(&rates, "gpt-6-astra", &attempts),
        Cost::Exact { micro_usd: 7_500 }
    );
}

#[test]
fn the_aggregation_script_and_this_module_price_the_same_usage_alike() {
    // The cases `scripts/test_benchmark_costs.py` pins, with the USD figures
    // that test asserts. Both sides read `scripts/rates.json`, so a change to
    // one language's arithmetic shows up here.
    for (input, cached, write, output, expected) in [
        (100_u64, 20_u64, 10_u64, 5_u64, "0.001095"),
        (100, 0, 100, 10, "0.001750"),
        (272_000, 0, 0, 1, "2.720050"),
        (272_001, 0, 0, 1, "5.440095"),
        // An odd cache-write count: 12.5 per million is half a micro-USD, so
        // rounding each share on its own would drift from the exact sum.
        (100, 0, 1, 0, "0.001003"),
        (100, 0, 3, 0, "0.001008"),
    ] {
        let held = observed(input, cached, write, output);
        let Cost::Exact { micro_usd } = api_cost(&rates(), "gpt-6-astra", &[attempt("a-1", held)])
        else {
            panic!("everything needed is known");
        };
        assert_eq!(usd_text(micro_usd), expected, "{input}/{cached}/{write}");
    }
}

#[test]
fn a_rate_with_more_precision_than_the_arithmetic_holds_is_refused() {
    assert_eq!(micro_usd("12.5"), Some(12_500_000));
    assert_eq!(micro_usd("10"), Some(10_000_000));
    assert_eq!(micro_usd("0.000001"), Some(1));
    // Seven decimal places would be rounded away without anyone noticing.
    assert_eq!(micro_usd("0.0000001"), None);
    assert_eq!(micro_usd("1.2.3"), None);
    assert_eq!(micro_usd("-1"), None);
    assert!(Rates::parse(r#"{"schema_version":1,"models":{"m":{"rates_as_of":"x","api_per_million":{"billed_input":"0.0000001","cache_read":"1","output":"2"},"long_context":{"input_tokens_above":1,"input_multiplier":"1","output_multiplier":"1"}}}}"#).is_err());
}
