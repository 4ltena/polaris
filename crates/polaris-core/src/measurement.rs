//! M8: what one comparison measurement leaves behind. Nothing here sends
//! anything or decides a winner: it fixes the conditions, adds up what the
//! providers reported, and holds the scoring. What a provider did not report
//! stays unknown, so a total can never be read as if it were complete.
use serde::Serialize;
use std::collections::BTreeSet;

pub mod output;
pub mod price;

pub const SCHEMA_VERSION: u32 = 1;

/// How far this measurement has actually gone. The machinery being finished
/// is not a measurement, and a synthetic pass is not a real one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    NotRun,
    Synthetic,
    Measured,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Arm {
    Polaris,
    /// Codex with an empty `AGENTS.md`; its own system prompt and tools stay.
    PlainCodex,
}

/// One part of what a request cost. Only `Input` and `Output` are counted
/// towards the token total: the cache figures are shares of `Input`, kept
/// because each share is billed at its own rate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    /// Everything the provider counted as input, cache reads included.
    Input,
    CachedInput,
    CacheWrite,
    Output,
    Reasoning,
}

/// A count the provider gave, or the fact that it did not. Never zero for a
/// missing value: zero is a measurement and this is the absence of one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum Reported {
    Known { tokens: u64 },
    Unknown,
}

impl Reported {
    pub fn known(&self) -> Option<u64> {
        match self {
            Self::Known { tokens } => Some(*tokens),
            Self::Unknown => None,
        }
    }
}

/// Whether the reasoning count is already part of the output count. Providers
/// differ, and adding a count that is already included doubles it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Reasoning {
    IncludedInOutput,
    Separate,
    Unknown,
}

/// What one physical attempt reported. The two cache figures are parts of
/// `input`, not separate amounts beside it: a provider reports them inside
/// its input details, and adding them would count the same tokens twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Observed {
    pub input: Reported,
    /// The share of `input` the provider read from its cache.
    pub cached_input: Reported,
    /// The share of `input` the provider wrote to its cache.
    pub cache_write: Reported,
    pub output: Reported,
    pub reasoning: Reported,
    pub reasoning_relation: Reasoning,
    /// The provider's own total, when it gave one. Used only to notice that
    /// the parts and the whole disagree.
    pub reported_total: Reported,
}

impl Observed {
    /// The share of the input billed at the full rate: what was neither read
    /// from the cache nor written to it. Unknown unless all three are known,
    /// because a rate cannot be applied to a guess.
    pub fn billed_input(&self) -> Reported {
        let (Some(input), Some(cached), Some(written)) = (
            self.input.known(),
            self.cached_input.known(),
            self.cache_write.known(),
        ) else {
            return Reported::Unknown;
        };
        // Shares adding up to more than the input contradict each other, and
        // saturating the difference to zero would hide that behind a figure
        // that looks measured.
        match input
            .checked_sub(cached)
            .and_then(|left| left.checked_sub(written))
        {
            Some(tokens) => Reported::Known { tokens },
            None => Reported::Unknown,
        }
    }

    /// Nothing reported at all.
    pub fn unknown() -> Self {
        Self {
            input: Reported::Unknown,
            cached_input: Reported::Unknown,
            cache_write: Reported::Unknown,
            output: Reported::Unknown,
            reasoning: Reported::Unknown,
            reasoning_relation: Reasoning::Unknown,
            reported_total: Reported::Unknown,
        }
    }

    /// What the durable attempt ledger holds. Its cached count is a plain
    /// number that the provider layer sets to zero both when the cache was
    /// not used and when the response carried no cache field at all, so zero
    /// is read here as "not reported" rather than as a measurement.
    pub fn from_ledger(usage: &polaris_provider::attempts::UsageObservation) -> Self {
        use polaris_provider::attempts::UsageObservation;
        match usage {
            UsageObservation::Missing => Self::unknown(),
            UsageObservation::Known {
                input_tokens,
                output_tokens,
                total_tokens,
                cached_tokens,
            } => Self {
                input: Reported::Known {
                    tokens: u64::from(*input_tokens),
                },
                cached_input: match cached_tokens {
                    0 => Reported::Unknown,
                    tokens => Reported::Known {
                        tokens: u64::from(*tokens),
                    },
                },
                // The ledger has no place for a cache-write count.
                cache_write: Reported::Unknown,
                output: Reported::Known {
                    tokens: u64::from(*output_tokens),
                },
                reasoning: Reported::Unknown,
                reasoning_relation: Reasoning::Unknown,
                reported_total: Reported::Known {
                    tokens: u64::from(*total_tokens),
                },
            },
        }
    }

    /// One row of the per-request diagnostic log, which is the only place a
    /// cache-write count appears. Each figure is absent when the provider did
    /// not send it, so nothing has to be inferred from a zero here.
    pub fn from_metrics_row(row: &serde_json::Value) -> Self {
        let read = |name: &str| {
            row.get("usage")
                .and_then(|usage| usage.get(name))
                .and_then(serde_json::Value::as_u64)
        };
        Self {
            input: reported(read("input_tokens")),
            cached_input: reported(read("cache_read_tokens")),
            cache_write: reported(read("cache_write_tokens")),
            output: reported(read("output_tokens")),
            reasoning: Reported::Unknown,
            reasoning_relation: Reasoning::Unknown,
            reported_total: Reported::Unknown,
        }
    }
}

fn reported(tokens: Option<u64>) -> Reported {
    tokens.map_or(Reported::Unknown, |tokens| Reported::Known { tokens })
}

/// Which observation layer an attempt was read from. The durable ledger and
/// the per-request log number their attempts differently, so the same call
/// appears under two unrelated identifiers and cannot be matched up. Adding
/// both to one total would count it twice.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Ledger,
    RequestLog,
}

/// One physical attempt, as the measurement counts it. A retry is its own
/// attempt; a child agent's attempt is its own too, and is never added to a
/// parent that already observed it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Attempt {
    pub attempt_id: String,
    pub source: Source,
    pub kind: Kind,
    pub retried: bool,
    pub observed: Observed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Completion,
    Summary,
    Embedding,
    WebSearch,
    Child,
    /// A model used to score the result, never to produce it.
    Grading,
}

/// Why a total is not the whole story.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(tag = "reason", rename_all = "snake_case")]
pub enum Unresolved {
    NotReported {
        category: Category,
    },
    /// A reasoning count exists but nobody knows whether it is already inside
    /// the output count, so it is left out rather than possibly doubled.
    ReasoningRelationUnknown,
    /// The provider's own total and the parts it gave do not agree.
    Contradictory,
    /// Attempts came from two observation layers whose identifiers cannot be
    /// matched, so the same call may be in the sum twice.
    MixedSources,
    /// The price list does not name this model. Another model's figures are
    /// never borrowed to fill the gap.
    RateUnknown,
}

/// The sum. A caller cannot read a number without also seeing what is
/// missing from it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "total", rename_all = "snake_case")]
pub enum Total {
    Exact {
        tokens: u64,
    },
    Partial {
        known: u64,
        unresolved: Vec<Unresolved>,
    },
}

/// Adds up what the attempts reported. Grading attempts are counted
/// separately, because scoring the work is not the cost of doing it.
pub fn total(attempts: &[Attempt]) -> Total {
    add_up(attempts.iter().filter(|held| held.kind != Kind::Grading))
}

/// What the scoring itself cost.
pub fn grading_total(attempts: &[Attempt]) -> Total {
    add_up(attempts.iter().filter(|held| held.kind == Kind::Grading))
}

fn add_up<'a>(attempts: impl Iterator<Item = &'a Attempt>) -> Total {
    let mut known = 0_u64;
    let mut unresolved = BTreeSet::new();
    let mut seen = BTreeSet::new();
    let mut sources = BTreeSet::new();
    for attempt in attempts {
        // One attempt read twice from the same layer is one attempt.
        if !seen.insert((attempt.source, attempt.attempt_id.clone())) {
            continue;
        }
        sources.insert(attempt.source);
        let observed = &attempt.observed;
        let mut parts = 0_u64;
        for (category, reported) in [
            (Category::Input, observed.input),
            (Category::Output, observed.output),
        ] {
            match reported.known() {
                Some(tokens) => parts = parts.saturating_add(tokens),
                None => {
                    unresolved.insert(Unresolved::NotReported { category });
                }
            }
        }
        // The cache figures are already inside the input count, so they are
        // never added. They are still needed to apply a rate, and a missing
        // one is recorded as missing.
        for (category, reported) in [
            (Category::CachedInput, observed.cached_input),
            (Category::CacheWrite, observed.cache_write),
        ] {
            if reported.known().is_none() {
                unresolved.insert(Unresolved::NotReported { category });
            }
        }
        match (observed.reasoning.known(), observed.reasoning_relation) {
            (Some(tokens), Reasoning::Separate) => parts = parts.saturating_add(tokens),
            // Already inside the output count; adding it would count it twice.
            (Some(_) | None, Reasoning::IncludedInOutput) => {}
            (Some(_), Reasoning::Unknown) => {
                unresolved.insert(Unresolved::ReasoningRelationUnknown);
            }
            // Billed apart from the output, but the amount is missing.
            (None, Reasoning::Separate) => {
                unresolved.insert(Unresolved::NotReported {
                    category: Category::Reasoning,
                });
            }
            // No reasoning figure at all. There is nothing to add and the
            // output count is still the whole of what was billed, so the sum
            // stands; only the breakdown is absent, which the record shows.
            (None, Reasoning::Unknown) => {}
        }
        // Cache shares that exceed the input are the provider's breakdown
        // disagreeing with itself.
        if let (Some(input), Some(cached), Some(written)) = (
            observed.input.known(),
            observed.cached_input.known(),
            observed.cache_write.known(),
        ) && cached.saturating_add(written) > input
        {
            unresolved.insert(Unresolved::Contradictory);
        }
        // The provider's own total against the parts it gave. Both come from
        // the same layer, so a difference is the provider counting something
        // it did not break down.
        if let Some(reported) = observed.reported_total.known()
            && observed.input.known().is_some()
            && observed.output.known().is_some()
            && reported != parts
        {
            unresolved.insert(Unresolved::Contradictory);
        }
        known = known.saturating_add(parts);
    }
    if sources.len() > 1 {
        unresolved.insert(Unresolved::MixedSources);
    }
    if unresolved.is_empty() {
        return Total::Exact { tokens: known };
    }
    Total::Partial {
        known,
        unresolved: unresolved.into_iter().collect(),
    }
}

/// What was fixed before the measurement started. Both arms carry the same
/// shape, so a difference between them is visible as a difference in values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Conditions {
    pub schema_version: u32,
    pub arm: Arm,
    pub model: String,
    pub effort: String,
    pub task_id: String,
    /// A digest of the instructions actually fed in, not of the file they
    /// were meant to come from.
    pub instructions_digest: String,
    pub initial_files_digest: String,
    pub allowed_operations: Vec<String>,
    pub max_seconds: u64,
    pub max_cost_microusd: u64,
    pub iteration: u32,
    pub toolchain: String,
    /// The scoring criteria and their version. Without it, a verdict read
    /// against one set of criteria cannot be told from a verdict read
    /// against a later set.
    pub grading_basis_digest: String,
    /// How the cache was prepared, including the version of the warm-up
    /// procedure. This cannot be recovered after the run.
    pub cache_setting: String,
    /// Where in the sequence this arm ran. A cache that one arm warmed is
    /// still warm for the next, so the order is part of the conditions.
    pub execution_order: u32,
}

impl Conditions {
    /// A stable digest of the whole condition set. Two arms that differ in
    /// anything but `arm` are not comparable, and this is how that is caught.
    pub fn digest(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut operations = self.allowed_operations.clone();
        operations.sort();
        operations.dedup();
        let mut body = String::new();
        // Each field carries its own length, so a separator appearing inside
        // a value cannot make two different condition sets look alike.
        for field in [
            self.schema_version.to_string(),
            format!("{:?}", self.arm),
            self.model.clone(),
            self.effort.clone(),
            self.task_id.clone(),
            self.instructions_digest.clone(),
            self.initial_files_digest.clone(),
            operations.len().to_string(),
            self.max_seconds.to_string(),
            self.max_cost_microusd.to_string(),
            self.iteration.to_string(),
            self.toolchain.clone(),
            self.grading_basis_digest.clone(),
            self.cache_setting.clone(),
            self.execution_order.to_string(),
        ]
        .into_iter()
        .chain(operations)
        {
            body.push_str(&format!("{}:{field}\n", field.len()));
        }
        format!("{:x}", Sha256::digest(body.as_bytes()))
    }

    /// Whether two arms were given the same measurement. Three fields are
    /// allowed to differ because they must: which arm it is, the
    /// instructions (that is the difference under test), and the position in
    /// the sequence, since one arm necessarily runs before the other. The
    /// order stays in the record, so the cache advantage the second arm may
    /// have had is visible rather than assumed away.
    pub fn comparable_with(&self, other: &Self) -> bool {
        Self {
            arm: other.arm,
            instructions_digest: other.instructions_digest.clone(),
            execution_order: other.execution_order,
            ..self.clone()
        }
        .digest()
            == other.digest()
    }
}

/// What actually happened, kept apart from what was asked for. Writing an
/// observation over its setting would hide the two disagreeing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Actual {
    /// The model the provider said answered, which is not always the one asked for.
    pub responded_model: Option<String>,
    /// Whether the cache was cold at the start, and what was observed later.
    pub cache_started_cold: Option<bool>,
    /// An account identifier that carries no credential, and every switch.
    pub account: Option<String>,
    pub account_switches: Vec<String>,
    pub started_unix_seconds: Option<u64>,
    pub finished_unix_seconds: Option<u64>,
    /// Measured against a clock that does not jump.
    pub elapsed_monotonic_ms: Option<u64>,
    pub stopped_because: Option<String>,
    pub human_intervention: Vec<String>,
    pub missing_because: Vec<String>,
    pub excluded_because: Option<String>,
    /// Which price list was applied, and under what conditions.
    pub price_basis: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    Met,
    NotImplemented,
    Stub,
    Defective,
    NotVerified,
}

/// Who judged. A model's judgement is only readable if its own version and
/// instructions are known.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "grader", rename_all = "snake_case")]
pub enum Grader {
    Person {
        id: String,
    },
    Model {
        version: String,
        instructions_digest: String,
    },
}

impl Grader {
    fn complete(&self) -> bool {
        match self {
            Self::Person { id } => !id.trim().is_empty(),
            Self::Model {
                version,
                instructions_digest,
            } => !version.trim().is_empty() && !instructions_digest.trim().is_empty(),
        }
    }
}

/// One requirement and how it was judged. A judgement with nothing to point
/// at is not a judgement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Criterion {
    pub id: String,
    pub criterion_version: u32,
    pub verdict: Verdict,
    /// The artefact the verdict was read from.
    pub artifact_hash: String,
    /// Where the evidence is: a command and its result, a saved file, a log.
    pub evidence: String,
    pub grader: Grader,
    pub scored_unix_seconds: u64,
}

impl Criterion {
    /// Scored only when all seven are there. A verdict that does not say
    /// which requirement it is about, or which version of that requirement it
    /// was read against, cannot be compared with anything later.
    pub fn scored(&self) -> bool {
        !self.id.trim().is_empty()
            && self.criterion_version > 0
            && !self.artifact_hash.trim().is_empty()
            && !self.evidence.trim().is_empty()
            && self.grader.complete()
            && self.scored_unix_seconds > 0
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Completeness {
    pub met: usize,
    pub criteria: usize,
    /// Criteria that were not scored at all. They are never met.
    pub unscored: Vec<String>,
}

pub fn completeness(criteria: &[Criterion]) -> Completeness {
    let unscored: Vec<String> = criteria
        .iter()
        .filter(|held| !held.scored())
        .map(|held| held.id.clone())
        .collect();
    Completeness {
        met: criteria
            .iter()
            .filter(|held| held.scored() && held.verdict == Verdict::Met)
            .count(),
        criteria: criteria.len(),
        unscored,
    }
}

/// One arm of one measurement, as it is written down.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Trial {
    pub stage: Stage,
    pub conditions: Conditions,
    pub actual: Actual,
    pub attempts: Vec<Attempt>,
    pub criteria: Vec<Criterion>,
}

impl Trial {
    pub fn total(&self) -> Total {
        total(&self.attempts)
    }

    pub fn grading_total(&self) -> Total {
        grading_total(&self.attempts)
    }

    pub fn completeness(&self) -> Completeness {
        completeness(&self.criteria)
    }
}

#[cfg(test)]
mod tests;
