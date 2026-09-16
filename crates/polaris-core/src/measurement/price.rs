//! M8b-1/2: the one price list and the cost it gives. The table is a single
//! versioned file that this module embeds and the aggregation scripts read
//! from disk, so two arms of a comparison can never be priced differently.
//! Money is counted in whole micro-USD, never in floating point, and a cost
//! is never returned as a bare number when something needed for it is
//! missing.
use super::{Attempt, Category, Kind, Reasoning, Unresolved};
use serde::Deserialize;
use std::collections::BTreeMap;

/// The table both languages read. Embedded so a built binary cannot drift
/// from the file the scripts use.
pub const TABLE: &str = include_str!("../../../../scripts/rates.json");

/// One USD amount per million tokens, held as micro-USD so the arithmetic
/// stays exact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rate(u128);

const MICRO: u128 = 1_000_000;

/// Parses a decimal amount such as `12.5` into micro-USD. More than six
/// decimal places is refused rather than rounded away.
fn micro_usd(text: &str) -> Option<u128> {
    let (whole, fraction) = text.split_once('.').unwrap_or((text, ""));
    if fraction.len() > 6 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let whole: u128 = whole.parse().ok()?;
    let padded = format!("{fraction:0<6}");
    let fraction: u128 = if padded.is_empty() {
        0
    } else {
        padded.parse().ok()?
    };
    whole.checked_mul(MICRO)?.checked_add(fraction)
}

#[derive(Debug, Deserialize)]
struct RawPerMillion {
    billed_input: String,
    cache_read: String,
    cache_write: Option<String>,
    output: String,
}

#[derive(Debug, Deserialize)]
struct RawLongContext {
    input_tokens_above: u64,
    input_multiplier: String,
    output_multiplier: String,
}

#[derive(Debug, Deserialize)]
struct RawModel {
    rates_as_of: String,
    /// `not_applicable` for a model that has no cache at all. Absent means
    /// the ordinary case, where the tiers apply and a missing figure is a
    /// missing measurement.
    cache: Option<String>,
    api_per_million: RawPerMillion,
    long_context: RawLongContext,
}

#[derive(Debug, Deserialize)]
struct RawTable {
    schema_version: u32,
    models: BTreeMap<String, RawModel>,
}

/// What one model costs, with the conditions the figures were taken under.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelRates {
    pub rates_as_of: String,
    /// Whether this model has a cache at all. False makes the cache shares
    /// zero by definition when a cost is worked out, which is a rule from
    /// the table and not a reading of what the run reported.
    pub cache_applies: bool,
    pub billed_input: Rate,
    pub cache_read: Rate,
    pub cache_write: Rate,
    pub output: Rate,
    pub long_context_above: u64,
    /// Multipliers as micro-units, so `1.5` is 1_500_000.
    pub long_input_multiplier: u128,
    pub long_output_multiplier: u128,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rates {
    pub schema_version: u32,
    models: BTreeMap<String, ModelRates>,
}

impl Rates {
    /// The embedded table. A malformed table is a build-time mistake, so it
    /// is reported rather than silently replaced with defaults.
    pub fn embedded() -> Result<Self, &'static str> {
        Self::parse(TABLE)
    }

    pub fn parse(body: &str) -> Result<Self, &'static str> {
        let raw: RawTable = serde_json::from_str(body).map_err(|_| "単価表を読めません")?;
        let mut models = BTreeMap::new();
        for (name, model) in raw.models {
            let rate = |text: &str| micro_usd(text).map(Rate).ok_or("単価の桁が扱えません");
            models.insert(
                name,
                ModelRates {
                    rates_as_of: model.rates_as_of,
                    cache_applies: match model.cache.as_deref() {
                        None => true,
                        Some("not_applicable") => false,
                        // A word nobody defined must not be read as "it has
                        // one" or as "it has none".
                        Some(_) => return Err("cacheの扱いを読めません"),
                    },
                    billed_input: rate(&model.api_per_million.billed_input)?,
                    cache_read: rate(&model.api_per_million.cache_read)?,
                    cache_write: rate(model.api_per_million.cache_write.as_deref().unwrap_or("0"))?,
                    output: rate(&model.api_per_million.output)?,
                    long_context_above: model.long_context.input_tokens_above,
                    long_input_multiplier: micro_usd(&model.long_context.input_multiplier)
                        .ok_or("倍率の桁が扱えません")?,
                    long_output_multiplier: micro_usd(&model.long_context.output_multiplier)
                        .ok_or("倍率の桁が扱えません")?,
                },
            );
        }
        Ok(Self {
            schema_version: raw.schema_version,
            models,
        })
    }

    /// The rates for one model, or nothing. A model the table does not name
    /// is never priced with another model's figures.
    pub fn model(&self, name: &str) -> Option<&ModelRates> {
        self.models.get(name)
    }
}

/// What a measurement cost, in whole micro-USD. Like a token total, it
/// cannot be read as a plain number while something it needs is missing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cost {
    Exact {
        micro_usd: u128,
    },
    Partial {
        known_micro_usd: u128,
        unresolved: Vec<Unresolved>,
    },
}

/// One share's contribution, kept un-rounded. Rounding each share on its own
/// and then adding them would drift from an exact sum: `12.5` per million is
/// half a micro-USD on an odd token count, which a per-share rounding turns
/// into a whole one.
fn amount(tokens: u64, rate: Rate, multiplier: u128) -> u128 {
    u128::from(tokens) * rate.0 * multiplier
}

/// One attempt's shares as whole micro-USD. The shares are added first and
/// rounded once, and the rounding happens per attempt: the aggregation
/// script rounds each request before summing, and a total that rounded only
/// at the very end would not equal the sum of the records it is made of.
fn settle(accumulated: u128) -> u128 {
    let denominator = MICRO * MICRO;
    (accumulated + denominator / 2) / denominator
}

/// The API-equivalent cost of the work, with the scoring left out: judging
/// the result is not part of what producing it cost.
pub fn api_cost(rates: &Rates, model: &str, attempts: &[Attempt]) -> Cost {
    add_up(
        rates,
        model,
        attempts.iter().filter(|held| held.kind != Kind::Grading),
    )
}

/// What the scoring itself cost.
pub fn grading_api_cost(rates: &Rates, model: &str, attempts: &[Attempt]) -> Cost {
    add_up(
        rates,
        model,
        attempts.iter().filter(|held| held.kind == Kind::Grading),
    )
}

fn add_up<'a>(rates: &Rates, model: &str, attempts: impl Iterator<Item = &'a Attempt>) -> Cost {
    let Some(rates) = rates.model(model) else {
        // Another model's figures would produce a number that looks like a
        // measurement and is not one.
        return Cost::Partial {
            known_micro_usd: 0,
            unresolved: vec![Unresolved::RateUnknown],
        };
    };
    let mut known = 0_u128;
    let mut unresolved = std::collections::BTreeSet::new();
    let mut seen = std::collections::BTreeSet::new();
    let mut sources = std::collections::BTreeSet::new();
    for attempt in attempts {
        if !seen.insert((attempt.source, attempt.attempt_id.clone())) {
            continue;
        }
        sources.insert(attempt.source);
        let mut shares = 0_u128;
        price_attempt(&attempt.observed, rates, &mut shares, &mut unresolved);
        known += settle(shares);
    }
    if sources.len() > 1 {
        unresolved.insert(Unresolved::MixedSources);
    }
    finish(known, unresolved)
}

fn price_attempt(
    observed: &super::Observed,
    rates: &ModelRates,
    known: &mut u128,
    unresolved: &mut std::collections::BTreeSet<Unresolved>,
) {
    // A long request is charged at a higher rate, so without the input count
    // nothing on the input side can be priced.
    let Some(input) = observed.input.known() else {
        unresolved.insert(Unresolved::NotReported {
            category: Category::Input,
        });
        price_output(known, unresolved, observed, rates, MICRO);
        return;
    };
    let (input_multiplier, output_multiplier) = if input > rates.long_context_above {
        (rates.long_input_multiplier, rates.long_output_multiplier)
    } else {
        (MICRO, MICRO)
    };
    // A model the table says has no cache is priced with both cache shares
    // zero by definition, so a run that reports nothing for them is not
    // missing anything. The observation keeps saying nothing was reported;
    // only the cost applies the rule. A figure above zero is the table and
    // the provider disagreeing, which is left showing rather than absorbed.
    if !rates.cache_applies {
        let claimed = [observed.cached_input, observed.cache_write]
            .iter()
            .filter_map(|held| held.known())
            .any(|tokens| tokens > 0);
        if claimed {
            unresolved.insert(Unresolved::Contradictory);
        } else {
            *known += amount(input, rates.billed_input, input_multiplier);
        }
        price_output(known, unresolved, observed, rates, output_multiplier);
        return;
    }
    // Cache shares that add up to more than the input mean the provider's
    // own breakdown disagrees with itself. None of the input figures can be
    // trusted then, so none of them is priced: keeping the two cache shares
    // as a confirmed subtotal would put a number nobody can stand behind
    // into the known part. The output side is reported apart from all this
    // and is still priced.
    if let (Some(cached), Some(written)) =
        (observed.cached_input.known(), observed.cache_write.known())
        && cached.saturating_add(written) > input
    {
        unresolved.insert(Unresolved::Contradictory);
        price_output(known, unresolved, observed, rates, output_multiplier);
        return;
    }
    for (category, reported, rate) in [
        (
            Category::CachedInput,
            observed.cached_input,
            rates.cache_read,
        ),
        (
            Category::CacheWrite,
            observed.cache_write,
            rates.cache_write,
        ),
    ] {
        match reported.known() {
            Some(tokens) => *known += amount(tokens, rate, input_multiplier),
            None => {
                unresolved.insert(Unresolved::NotReported { category });
            }
        }
    }
    // The full-rate share needs all three; it is never guessed.
    match observed.billed_input().known() {
        Some(tokens) => *known += amount(tokens, rates.billed_input, input_multiplier),
        None => {
            unresolved.insert(Unresolved::NotReported {
                category: Category::Input,
            });
        }
    }
    price_output(known, unresolved, observed, rates, output_multiplier);
}

/// Reasoning is billed at the output rate, so it is priced here when it is
/// known to sit outside the output count and skipped when it is inside it.
fn price_output(
    known: &mut u128,
    unresolved: &mut std::collections::BTreeSet<Unresolved>,
    observed: &super::Observed,
    rates: &ModelRates,
    multiplier: u128,
) {
    match observed.output.known() {
        Some(tokens) => *known += amount(tokens, rates.output, multiplier),
        None => {
            unresolved.insert(Unresolved::NotReported {
                category: Category::Output,
            });
        }
    }
    match (observed.reasoning.known(), observed.reasoning_relation) {
        (Some(tokens), Reasoning::Separate) => *known += amount(tokens, rates.output, multiplier),
        (Some(_) | None, Reasoning::IncludedInOutput) => {}
        (Some(_), Reasoning::Unknown) => {
            unresolved.insert(Unresolved::ReasoningRelationUnknown);
        }
        (None, Reasoning::Separate) => {
            unresolved.insert(Unresolved::NotReported {
                category: Category::Reasoning,
            });
        }
        // No reasoning figure at all: the output count is the whole of what
        // was billed, so there is nothing missing from the price.
        (None, Reasoning::Unknown) => {}
    }
}

/// What one attempt cost, for a caller holding a single observation.
pub fn api_cost_of(rates: &Rates, model: &str, observed: &super::Observed) -> Cost {
    let Some(rates) = rates.model(model) else {
        return Cost::Partial {
            known_micro_usd: 0,
            unresolved: vec![Unresolved::RateUnknown],
        };
    };
    let mut shares = 0_u128;
    let mut unresolved = std::collections::BTreeSet::new();
    price_attempt(observed, rates, &mut shares, &mut unresolved);
    finish(settle(shares), unresolved)
}

fn finish(known: u128, unresolved: std::collections::BTreeSet<Unresolved>) -> Cost {
    if unresolved.is_empty() {
        return Cost::Exact { micro_usd: known };
    }
    Cost::Partial {
        known_micro_usd: known,
        unresolved: unresolved.into_iter().collect(),
    }
}

/// The least and the most one attempt can have cost while the cache-write
/// share is not known. The input that was not read from the cache is priced
/// at the full rate for the floor and at the cache-write rate for the
/// ceiling. These are bounds, not a cost, and are kept out of `Cost` so
/// neither can be read as one.
pub fn api_cost_interval(
    rates: &Rates,
    model: &str,
    observed: &super::Observed,
) -> Option<(u128, u128)> {
    let rates = rates.model(model)?;
    let input = observed.input.known()?;
    let cached = observed.cached_input.known()?;
    if cached > input {
        return None;
    }
    let (input_multiplier, output_multiplier) = if input > rates.long_context_above {
        (rates.long_input_multiplier, rates.long_output_multiplier)
    } else {
        (MICRO, MICRO)
    };
    let fixed = amount(cached, rates.cache_read, input_multiplier)
        + amount(observed.output.known()?, rates.output, output_multiplier);
    Some((
        settle(fixed + amount(input - cached, rates.billed_input, input_multiplier)),
        settle(fixed + amount(input - cached, rates.cache_write, input_multiplier)),
    ))
}

/// The ceiling alone, for a caller that only guards against spending too much.
pub fn api_cost_upper(rates: &Rates, model: &str, observed: &super::Observed) -> Option<u128> {
    api_cost_interval(rates, model, observed).map(|(_, upper)| upper)
}

/// The figure as the recorded JSON fields carry it, which is a number and
/// not a string: the launchers that read those records add them up and
/// compare them against a spending limit. Arithmetic stays in micro-USD;
/// this is the last step before writing the record.
pub fn usd(micro_usd: u128) -> f64 {
    micro_usd as f64 / MICRO as f64
}

/// Only for showing a figure to a person; never for further arithmetic.
pub fn usd_text(micro_usd: u128) -> String {
    format!("{}.{:06}", micro_usd / MICRO, micro_usd % MICRO)
}

#[cfg(test)]
mod tests;
