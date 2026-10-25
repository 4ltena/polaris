//! Pricing a wave of delegations with one typed decision.
//!
//! Every task in the wave becomes one `Choice` question, and the whole wave
//! goes in a single request. `Choice` is the right shape here and was the
//! wrong one for tool preselection: its options form a competitive
//! distribution summing to one, which prevents picking several tools by
//! threshold but is exactly what "the task is one of these weights"
//! means.
use std::collections::BTreeMap;

use polaris_provider::decision::{Decider, Question};
use polaris_skills::{AgentType, Tier};

use super::TierDecider;

/// What each weight is for, in the words the decision sees. These describe
/// the work, not the models: which model a weight buys is configuration,
/// and telling the decision about it would have it price the run rather
/// than judge the task.
///
/// Ordered, because the question is a `Score` rather than a `Choice`: the
/// four rungs are a scale, the answer is the probability-weighted position
/// on it, and a position is what a threshold can be biased against. A
/// `Choice` returns only the winning name, so a task the judge thought was
/// 51% heavy and one it thought was 99% heavy bought the same model.
fn levels() -> Vec<String> {
    let described = criteria();
    Tier::ALL
        .into_iter()
        .map(|tier| described[tier.name()].clone())
        .collect()
}

/// Where each rung starts on the answered scale.
///
/// The scale runs 0 to 3 over the four rungs in order, so plain rounding
/// would split at 0.5, 1.5 and 2.5. These sit a quarter of a rung higher,
/// so a task has to look clearly heavier before it buys a heavier model.
///
/// **A quarter, not a whole rung.** Biasing the first cut all the way to
/// 1.25 sent an entire feature's implementation to `dwarf` on 2026-09-20:
/// the run cost more, not less, because the parent then spent 27 shell
/// calls and 11 edits repairing what came back. Cheap routing that has to
/// be redone is not cheap, and the命題 is cost at held quality.
const RUNGS: [(f64, Tier); 3] = [
    (0.75, Tier::Subgiant),
    (1.75, Tier::Giant),
    (2.75, Tier::Supergiant),
];

/// The rung a score lands on. Below the first threshold is the lightest.
fn from_score(score: f64) -> Option<Tier> {
    if !score.is_finite() || score < 0.0 {
        return None;
    }
    Some(
        RUNGS
            .iter()
            .filter(|(at, _)| score >= *at)
            .map(|(_, tier)| *tier)
            .next_back()
            .unwrap_or(Tier::Dwarf),
    )
}

fn criteria() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            Tier::Dwarf.name().to_owned(),
            "One clear step. Reading, simple searches, fixed-format transformations, or applying an existing pattern, with almost no branching judgment.".to_owned(),
        ),
        (
            Tier::Subgiant.name().to_owned(),
            "Requires several steps, but the direction is evident from the request. Includes following existing structure to change multiple files.".to_owned(),
        ),
        (
            Tier::Giant.name().to_owned(),
            "A difficult instance of a known task type. An unexplained bug, a broad rework, or work requiring design decisions along the way. The steps are inferable but may not proceed as expected.".to_owned(),
        ),
        (
            Tier::Supergiant.name().to_owned(),
            "The direction itself must be chosen. Design choices or conflicting constraints have no single correct answer.".to_owned(),
        ),
    ])
}

pub struct TypedTierDecider<D> {
    decider: D,
}

impl<D: Decider> TypedTierDecider<D> {
    pub fn new(decider: D) -> Self {
        Self { decider }
    }
}

/// The key a task's question is asked under. Positions, not task text: the
/// reply is matched back by this key, so it has to be something the task
/// cannot change.
fn key(index: usize) -> String {
    format!("t{index}")
}

#[async_trait::async_trait]
impl<D: Decider> TierDecider for TypedTierDecider<D> {
    async fn decide(&self, tasks: &[(&AgentType, &str)]) -> Vec<Tier> {
        let unrouted = || {
            tasks
                .iter()
                .map(|(agent, _)| Tier::DEFAULT.min(agent.tier))
                .collect::<Vec<_>>()
        };
        if tasks.is_empty() {
            return Vec::new();
        }
        let questions: BTreeMap<String, Question> = tasks
            .iter()
            .enumerate()
            .map(|(index, (agent, task))| {
                (
                    key(index),
                    Question::Score {
                        instructions: format!(
                            "What is the weight of this request?\n\nAssigned type: {}\nType description: {}\nRequest: {task}",
                            agent.name, agent.description
                        ),
                        criteria: levels(),
                    },
                )
            })
            .collect();
        let state = serde_json::json!({
            "purpose": "Determine the weight of delegated work",
        });
        // Refused rather than truncated: dropping questions would leave
        // some tasks unanswered, and an answer list that does not line up
        // with the tasks is discarded whole by `decide_all` anyway.
        let counted = crate::budget::count_tokens(&state.to_string())
            + questions
                .values()
                .map(|question| {
                    crate::budget::count_tokens(
                        &serde_json::to_string(question).unwrap_or_default(),
                    )
                })
                .sum::<usize>();
        if polaris_provider::decision::within_limits(counted, &questions).is_err() {
            return unrouted();
        }
        let Ok(decision) = self.decider.decide(&state, &questions).await else {
            // Authentication, the service being unreachable, a malformed
            // reply, or cancellation: none of them is a reason to lose a
            // delegation. This is an unrouted task, so use the M16 default
            // held down by the declared ceiling.
            return unrouted();
        };
        (0..tasks.len())
            .map(|index| {
                decision
                    .answers
                    .get(&key(index))
                    .and_then(|answer| answer.score)
                    .and_then(from_score)
                    .unwrap_or_else(|| Tier::DEFAULT.min(tasks[index].0.tier))
            })
            .collect()
    }
}

#[cfg(test)]
#[path = "typed/tests.rs"]
mod tests;
