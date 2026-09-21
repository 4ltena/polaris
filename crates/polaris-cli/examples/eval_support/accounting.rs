//! Reconcile physical evidence and price known consumption without double counting.
use polaris_core::measurement::price;
use polaris_core::measurement::{Observed, Reasoning, Reported};
use polaris_provider::{
    Usage, UsageReport,
    attempts::{AttemptKind, AttemptRecord, AttemptStatus, UsageObservation},
};
use serde_json::{Value, json};
use std::{collections::BTreeSet, fs, path::Path};

pub(crate) fn read_metrics(path: &Path) -> Result<Vec<Value>, String> {
    let metadata = fs::symlink_metadata(path).map_err(|_| "physical metrics missing")?;
    if !metadata.is_file() || metadata.len() > 8 * 1024 * 1024 {
        return Err("physical metrics must be a bounded regular file".into());
    }
    let body = fs::read_to_string(path).map_err(|_| "physical metrics unavailable")?;
    if !body.is_empty() && !body.ends_with('\n') {
        return Err("incomplete physical metrics line".into());
    }
    body.lines()
        .map(|line| serde_json::from_str(line).map_err(|_| "invalid physical metrics JSON".into()))
        .collect()
}

pub(crate) struct RowCost {
    pub lower: u128,
    pub upper: u128,
    pub exact: Option<u128>,
}

// A failed response may still carry billable usage. Price it here; successful
// measurement eligibility is checked separately by reconcile.
pub(crate) fn assess_row(
    row: &Value,
    model: &str,
    effort: &str,
    usage: Usage,
) -> Result<RowCost, String> {
    if row["attempt_id"].as_str().is_none_or(str::is_empty)
        || row["model"] != model
        || row["effort"] != effort
        || row["transport"]
            .as_str()
            .is_some_and(|transport| transport != "http")
        || !matches!(
            row["outcome"].as_str(),
            Some(
                "completed"
                    | "auth_error"
                    | "http_error"
                    | "decode_error"
                    | "budget_error"
                    | "unsupported_error"
                    | "observation_error"
                    | "cancelled"
            )
        )
    {
        return Err("physical request identity or outcome mismatch".into());
    }
    let count = |key: &str| {
        row["usage"][key]
            .as_u64()
            .ok_or("required physical usage missing")
    };
    let (input, output, cached) = (
        count("input_tokens")?,
        count("output_tokens")?,
        count("cache_read_tokens")?,
    );
    if input != u64::from(usage.input_tokens)
        || output != u64::from(usage.output_tokens)
        || cached != u64::from(usage.cached_tokens)
        || cached > input
        || input.checked_add(output) != Some(u64::from(usage.total_tokens))
    {
        return Err("physical response usage mismatch".into());
    }
    for (key, ceiling) in [
        ("cache_write_tokens", input - cached),
        ("reasoning_tokens", output),
    ] {
        match row["usage"].get(key) {
            None | Some(Value::Null) => {}
            Some(value) if value.as_u64().is_some_and(|count| count <= ceiling) => {}
            _ => return Err("cache or reasoning usage contradiction".into()),
        }
    }
    let rates = price::Rates::embedded().map_err(str::to_owned)?;
    let mut observed = Observed::from_metrics_row(row);
    observed.reasoning = row["usage"]["reasoning_tokens"]
        .as_u64()
        .map_or(Reported::Unknown, |tokens| Reported::Known { tokens });
    // Codex metrics obtain this from output_tokens_details: it is a share, not an addition.
    observed.reasoning_relation = Reasoning::IncludedInOutput;
    let (lower, upper) =
        price::api_cost_interval(&rates, model, &observed).ok_or("fee interval unavailable")?;
    let exact = match price::api_cost_of(&rates, model, &observed) {
        price::Cost::Exact { micro_usd } => Some(micro_usd),
        price::Cost::Partial { .. } => None,
    };
    if upper < lower || exact.is_some_and(|cost| cost < lower || cost > upper) {
        return Err("price interval does not conservatively contain the exact fee".into());
    }
    Ok(RowCost {
        lower,
        upper,
        exact,
    })
}

pub(crate) fn reconcile(
    attempts: &[AttemptRecord],
    rows: &[Value],
    report: &UsageReport,
    effort: &str,
    allowed: &[(AttemptKind, &str)],
) -> Result<Value, String> {
    if attempts.is_empty()
        || attempts.len() != rows.len()
        || report.reported_responses != attempts.len() as u64
        || report.missing_responses != 0
        || report.failed_requests != 0
    {
        return Err("physical attempts, metrics and observed responses differ".into());
    }
    let mut ledger_ids = BTreeSet::new();
    let mut metric_ids = BTreeSet::new();
    let mut sums = [0u64; 4];
    let (mut lower, mut upper, mut exact) = (0u128, 0u128, Some(0u128));
    let mut by_model = serde_json::Map::new();
    for (attempt, row) in attempts.iter().zip(rows) {
        let id = row["attempt_id"]
            .as_str()
            .ok_or("metric identity missing")?;
        // The provider metrics and durable ledger mint separate ID namespaces.
        // These serial drivers reconcile by order, model and exact usage; IDs
        // must each be unique but are not expected to be equal across stores.
        if !ledger_ids.insert(&attempt.attempt_id)
            || !metric_ids.insert(id)
            || attempt.status != AttemptStatus::Succeeded
            || row["outcome"] != "completed"
            || attempt.retry_reason.is_some()
            || !allowed.contains(&(attempt.kind, attempt.model.as_str()))
        {
            return Err("duplicate, failed or unexpected physical attempt".into());
        }
        let UsageObservation::Known {
            input_tokens,
            output_tokens,
            total_tokens,
            cached_tokens,
        } = attempt.usage
        else {
            return Err("ledger usage missing".into());
        };
        let usage = Usage {
            input_tokens,
            output_tokens,
            total_tokens,
            cached_tokens,
        };
        let cost = assess_row(row, &attempt.model, effort, usage)?;
        lower += cost.lower;
        upper += cost.upper;
        exact = exact.zip(cost.exact).map(|(a, b)| a + b);
        for (sum, count) in
            sums.iter_mut()
                .zip([input_tokens, output_tokens, total_tokens, cached_tokens])
        {
            *sum += u64::from(count);
        }
        let entry = by_model.entry(attempt.model.clone()).or_insert_with(|| json!({"physical_sends":0,"input_tokens":0,"output_tokens":0,"cache_read_tokens":0,"lower_cost_microusd":0,"upper_cost_microusd":0}));
        for (key, count) in [
            ("physical_sends", 1),
            ("input_tokens", input_tokens.into()),
            ("output_tokens", output_tokens.into()),
            ("cache_read_tokens", cached_tokens.into()),
            ("lower_cost_microusd", cost.lower as u64),
            ("upper_cost_microusd", cost.upper as u64),
        ] {
            entry[key] = json!(entry[key].as_u64().expect("numeric aggregate") + count);
        }
    }
    let usage = report.usage;
    if sums
        != [
            usage.input_tokens,
            usage.output_tokens,
            usage.total_tokens,
            usage.cached_tokens,
        ]
        .map(u64::from)
    {
        return Err("parent-inclusive usage and physical metrics differ".into());
    }
    Ok(
        json!({"source":"physical_metrics_only","ledger_used_for":"ordered physical reconciliation, never added to costs",
        "lower_cost_microusd":lower,"upper_cost_microusd":upper,"exact_cost_microusd":exact,"exact_fee_complete":exact.is_some(),
        "reasoning_relation":"included_in_output","reasoning_complete":rows.iter().all(|row| row["usage"]["reasoning_tokens"].is_u64()),
        "cache_write_complete":rows.iter().all(|row| row["usage"]["cache_write_tokens"].is_u64()),"by_model":by_model}),
    )
}
