//! A model that answers typed questions instead of writing text. It takes a
//! block of state and a set of questions, and returns one typed answer per
//! question with probabilities whose calibration is recorded separately.
//! Nothing here generates prose, so
//! it is deliberately not the conversational `Provider`: putting a typed
//! answer into that type's `text` would claim the model wrote it, and into
//! its `tool_calls` would claim the model asked to run something.
use serde::{Deserialize, Serialize};

mod capabilities;
pub mod fallback;
mod observation;
mod telemetry;
pub use capabilities::{
    BandCalibration, DecisionCancellation, DecisionCapabilities, DecisionLocation, DecisionShapes,
    DecisionUsageReporting,
};
pub use observation::{
    DecisionAttempt, DecisionCapacity, DecisionContext, DecisionIdentity, DecisionObservation,
    DecisionTrace,
};
pub use telemetry::{DecisionMeter, DecisionObserver};

/// The most options one choice may hold, and the most state one request may
/// carry. Both are checked before sending rather than after being refused.
pub const MAX_OPTIONS: usize = 255;
pub const MAX_STATE_TOKENS: usize = 32_000;

/// The shapes are those the service actually accepts, confirmed against it
/// on 2026-09-17. An earlier version of this file was written from the
/// published description and used `ask`, a list of option strings and a
/// positional probability array; the service takes `instructions`, a named
/// map and returns probabilities keyed by those names.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Question {
    /// The probability that the answer to a yes/no question is yes.
    Noul { instructions: String },
    /// One option out of a set. The options are named, and the answer comes
    /// back as the name rather than a position, so adding an option cannot
    /// silently change what an earlier one meant.
    Choice {
        instructions: String,
        criteria: std::collections::BTreeMap<String, String>,
    },
    /// A rating against ordered, described levels. The answer is the
    /// probability-weighted level index, so a three-level scale answers
    /// between 0 and 2.
    Score {
        instructions: String,
        criteria: Vec<String>,
    },
}

impl Question {
    fn options(&self) -> usize {
        match self {
            Self::Noul { .. } => 0,
            Self::Choice { criteria, .. } => criteria.len(),
            Self::Score { criteria, .. } => criteria.len(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct Answer {
    /// Present for a `Noul`: the probability of yes.
    #[serde(default)]
    pub noul: Option<f64>,
    /// Present for a `Choice`: the name of the option chosen.
    #[serde(default)]
    pub choice: Option<String>,
    /// Present for a `Score`: the probability-weighted level index.
    #[serde(default)]
    pub score: Option<f64>,
    /// One probability per option, keyed by the option's name for a
    /// `Choice` and by the level's index for a `Score`. Empty for a `Noul`.
    #[serde(default)]
    pub probabilities: std::collections::BTreeMap<String, f64>,
    /// Present for a `Score`: the level index mapped back to its label.
    #[serde(default)]
    pub legend: std::collections::BTreeMap<String, String>,
    #[serde(default)]
    pub confidence: Option<f64>,
}

/// What the model was asked, kept beside the answers so a reply can be
/// checked against the question it belongs to.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    pub answers: std::collections::BTreeMap<String, Answer>,
    pub input_tokens: Option<u64>,
    pub output_tokens: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DecisionError {
    /// The state or a question is larger than one request may carry.
    TooLarge,
    /// The reply did not match the questions it answers.
    InvalidAnswer,
    /// The service rejected the request itself as malformed. The host built
    /// it, so this is a defect here rather than a condition to wait out, and
    /// asking again would send the same thing.
    Malformed,
    Auth,
    /// The service refused or was unavailable; the caller decides whether to
    /// wait and ask again.
    Unavailable,
    Cancelled,
}

#[async_trait::async_trait]
pub trait Decider: Send + Sync {
    /// Host-bound per-call context. Ordinary Jev/SemIf adapters preserve their path.
    async fn observe_with_context(
        &self,
        state: &serde_json::Value,
        questions: &std::collections::BTreeMap<String, Question>,
        _context: DecisionContext,
    ) -> DecisionObservation {
        self.observe(state, questions).await
    }

    async fn decide(
        &self,
        state: &serde_json::Value,
        questions: &std::collections::BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError>;

    /// Observe one request without changing the legacy typed-answer API.
    async fn observe(
        &self,
        state: &serde_json::Value,
        questions: &std::collections::BTreeMap<String, Question>,
    ) -> DecisionObservation {
        let identity = self.identity();
        let capabilities = self.capabilities();
        let started = std::time::Instant::now();
        let result = self.decide(state, questions).await;
        DecisionObservation::single(identity, capabilities, result, started.elapsed())
    }

    fn identity(&self) -> DecisionIdentity {
        DecisionIdentity {
            backend: self.name().into(),
            model: None,
            revision: None,
            quantization: None,
        }
    }

    /// Eligibility to ask for bands, not a guarantee about the eventual answer.
    fn may_return_calibrated_bands(&self) -> bool {
        self.capabilities().band_calibration == BandCalibration::LegacyJev
    }

    /// What answered, for the record.
    ///
    /// A stored decision that cannot name its judge cannot be checked
    /// later, and writing one judge's name into the format is how the
    /// format stops fitting the others. Implementations return their own
    /// name; nothing above this hard-codes one.
    /// Shared bounded telemetry when the host installed a meter.
    fn telemetry(&self) -> Option<DecisionMeter> {
        None
    }

    fn name(&self) -> &str {
        "unknown"
    }

    /// The adapter's own contract. Defaults never claim calibration, locality
    /// or a model limit merely because an implementation supports this trait.
    fn capabilities(&self) -> DecisionCapabilities {
        DecisionCapabilities::default()
    }
}

/// What a run's judge consumed, and how often it was asked.
///
/// The judge's own consumption is billed separately from the models', at a
/// different rate, and it was previously counted nowhere at all: the reply
/// carries a usage figure, [`Decision`] carries it forward, and no caller
/// read it. A run with the judge enabled therefore reported its cost with
/// the judge's part missing — which is the one number a person turning the
/// judge on wants to see.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct DecisionUsage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// Answered requests whose reply carried both usage figures.
    pub reported: u64,
    /// Answered requests whose reply omitted either usage figure. Known
    /// token subtotals remain recorded; the response's total is incomplete.
    pub missing: u64,
    /// Requests without an accepted answer. Totals retain any service-reported
    /// usage; absent figures stay unknown because work may precede refusal.
    pub failed: u64,
    /// Logical stages dropped before observation settled. Physical usage and
    /// which attempts reached a runtime remain unknown; never counted as zero.
    pub abandoned: u64,
    /// Observer writes that failed, separate from model consumption.
    pub observer_failures: u64,
}

/// One judge is registered per run and consulted from several places, so it
/// is held behind an `Arc`. Forwarding here lets a shared handle be used
/// wherever the trait is expected, without each caller unwrapping it.
#[async_trait::async_trait]
impl<T: Decider + ?Sized> Decider for std::sync::Arc<T> {
    async fn observe_with_context(
        &self,
        state: &serde_json::Value,
        questions: &std::collections::BTreeMap<String, Question>,
        context: DecisionContext,
    ) -> DecisionObservation {
        (**self)
            .observe_with_context(state, questions, context)
            .await
    }

    fn telemetry(&self) -> Option<DecisionMeter> {
        (**self).telemetry()
    }

    fn may_return_calibrated_bands(&self) -> bool {
        (**self).may_return_calibrated_bands()
    }
    fn identity(&self) -> DecisionIdentity {
        (**self).identity()
    }

    async fn observe(
        &self,
        state: &serde_json::Value,
        questions: &std::collections::BTreeMap<String, Question>,
    ) -> DecisionObservation {
        (**self).observe(state, questions).await
    }
    fn name(&self) -> &str {
        (**self).name()
    }

    fn capabilities(&self) -> DecisionCapabilities {
        (**self).capabilities()
    }

    async fn decide(
        &self,
        state: &serde_json::Value,
        questions: &std::collections::BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        (**self).decide(state, questions).await
    }
}

/// Refuses a request that cannot be sent as it stands. `state_tokens` is the
/// caller's own count, since only it knows which tokenizer applies.
pub fn within_limits(
    state_tokens: usize,
    questions: &std::collections::BTreeMap<String, Question>,
) -> Result<(), DecisionError> {
    if state_tokens > MAX_STATE_TOKENS
        || questions.values().any(|held| held.options() > MAX_OPTIONS)
    {
        return Err(DecisionError::TooLarge);
    }
    Ok(())
}

impl Answer {
    /// Whether this answer is a usable reply to that question. A probability
    /// outside 0–1, a count that does not match the options, or an index
    /// pointing outside them all mean the reply cannot be read.
    pub fn answers(&self, question: &Question) -> bool {
        let ranged = |value: f64| (0.0..=1.0).contains(&value);
        if self.probabilities.values().any(|held| !ranged(*held))
            || self.confidence.is_some_and(|held| !ranged(held))
        {
            return false;
        }
        match question {
            Question::Noul { .. } => {
                self.noul.is_some_and(ranged)
                    && self.choice.is_none()
                    && self.score.is_none()
                    && self.probabilities.is_empty()
            }
            Question::Choice { criteria, .. } => {
                // The chosen name has to be one that was offered, and every
                // offered name has to carry a probability: a reply that
                // answers about something else is not a narrower answer.
                self.noul.is_none()
                    && self.score.is_none()
                    && self
                        .choice
                        .as_ref()
                        .is_some_and(|name| criteria.contains_key(name))
                    && self.probabilities.len() == criteria.len()
                    && self
                        .probabilities
                        .keys()
                        .all(|name| criteria.contains_key(name))
                    && sums_to_one(&self.probabilities)
            }
            Question::Score { criteria, .. } => {
                // Levels come back keyed by their index, and the weighted
                // answer lies inside the scale it was asked on.
                self.noul.is_none()
                    && self.choice.is_none()
                    && self.probabilities.len() == criteria.len()
                    && (0..criteria.len())
                        .all(|index| self.probabilities.contains_key(&index.to_string()))
                    && self
                        .score
                        .is_some_and(|held| held >= 0.0 && held <= criteria.len() as f64 - 1.0)
                    && sums_to_one(&self.probabilities)
            }
        }
    }
}

/// Probabilities over a set of options describe one outcome, so they add up
/// to one. The tolerance allows for the rounding a wire format introduces.
fn sums_to_one(probabilities: &std::collections::BTreeMap<String, f64>) -> bool {
    let total: f64 = probabilities.values().sum();
    (total - 1.0).abs() < 0.01
}

/// Every answer read against the question it belongs to, with no question
/// left unanswered and no answer that was never asked for.
pub fn checked(
    decision: &Decision,
    questions: &std::collections::BTreeMap<String, Question>,
) -> Result<(), DecisionError> {
    if decision.answers.len() != questions.len() {
        return Err(DecisionError::InvalidAnswer);
    }
    for (id, question) in questions {
        let Some(answer) = decision.answers.get(id) else {
            return Err(DecisionError::InvalidAnswer);
        };
        if !answer.answers(question) {
            return Err(DecisionError::InvalidAnswer);
        }
    }
    Ok(())
}

pub mod jev;
pub mod semif;
pub mod system_one;
#[doc(hidden)]
pub mod wire;

#[cfg(test)]
mod tests;
