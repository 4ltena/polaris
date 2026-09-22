//! A model that answers typed questions instead of writing text. It takes a
//! block of state and a set of questions, and returns one typed answer per
//! question with a calibrated probability. Nothing here generates prose, so
//! it is deliberately not the conversational `Provider`: putting a typed
//! answer into that type's `text` would claim the model wrote it, and into
//! its `tool_calls` would claim the model asked to run something.
use serde::{Deserialize, Serialize};

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

#[derive(Debug, Clone, PartialEq, Eq)]
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
    async fn decide(
        &self,
        state: &serde_json::Value,
        questions: &std::collections::BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError>;

    /// What answered, for the record.
    ///
    /// A stored decision that cannot name its judge cannot be checked
    /// later, and writing one judge's name into the format is how the
    /// format stops fitting the others. Implementations return their own
    /// name; nothing above this hard-codes one.
    fn name(&self) -> &str {
        "unknown"
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
    /// Requests that did not answer. Consumption unknown; the service may
    /// have done work before refusing.
    pub failed: u64,
}

/// Shared observation of a judge's consumption, the counterpart of
/// [`crate::UsageMeter`] for the model side.
#[derive(Clone, Default)]
pub struct DecisionMeter(std::sync::Arc<std::sync::Mutex<DecisionUsage>>);

impl DecisionMeter {
    pub fn snapshot(&self) -> DecisionUsage {
        *self.0.lock().expect("decision meter poisoned")
    }

    /// Wraps a judge so every question put to it is counted.
    pub fn wrap(&self, decider: std::sync::Arc<dyn Decider>) -> std::sync::Arc<dyn Decider> {
        std::sync::Arc::new(MeteredDecider {
            decider,
            meter: self.clone(),
        })
    }
}

struct MeteredDecider {
    decider: std::sync::Arc<dyn Decider>,
    meter: DecisionMeter,
}

#[async_trait::async_trait]
impl Decider for MeteredDecider {
    async fn decide(
        &self,
        state: &serde_json::Value,
        questions: &std::collections::BTreeMap<String, Question>,
    ) -> Result<Decision, DecisionError> {
        let result = self.decider.decide(state, questions).await;
        let mut held = self.meter.0.lock().expect("decision meter poisoned");
        match &result {
            Ok(decision) => match (decision.input_tokens, decision.output_tokens) {
                (Some(input), Some(output)) => {
                    held.reported = held.reported.saturating_add(1);
                    held.input_tokens = held.input_tokens.saturating_add(input);
                    held.output_tokens = held.output_tokens.saturating_add(output);
                }
                (input, output) => {
                    held.missing = held.missing.saturating_add(1);
                    if let Some(input) = input {
                        held.input_tokens = held.input_tokens.saturating_add(input);
                    }
                    if let Some(output) = output {
                        held.output_tokens = held.output_tokens.saturating_add(output);
                    }
                }
            },
            Err(_) => held.failed = held.failed.saturating_add(1),
        }
        result
    }
}

/// One judge is registered per run and consulted from several places, so it
/// is held behind an `Arc`. Forwarding here lets a shared handle be used
/// wherever the trait is expected, without each caller unwrapping it.
#[async_trait::async_trait]
impl<T: Decider + ?Sized> Decider for std::sync::Arc<T> {
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
                self.noul.is_some_and(ranged) && self.choice.is_none() && self.score.is_none()
            }
            Question::Choice { criteria, .. } => {
                // The chosen name has to be one that was offered, and every
                // offered name has to carry a probability: a reply that
                // answers about something else is not a narrower answer.
                self.choice
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
                self.probabilities.len() == criteria.len()
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

#[cfg(test)]
mod tests;
