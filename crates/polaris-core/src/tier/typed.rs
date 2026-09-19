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
fn criteria() -> BTreeMap<String, String> {
    BTreeMap::from([
        (
            Tier::Dwarf.name().to_owned(),
            "一つの明確な手順で終わる。読み取り、単純な検索、決まった形への変換、既にある書き方の横展開など、判断の分岐がほとんど無い。".to_owned(),
        ),
        (
            Tier::Subgiant.name().to_owned(),
            "複数の手順を組み立てる必要があるが、方針は依頼から読み取れる。既存の構造を追って複数のファイルを変更する作業がこれにあたる。".to_owned(),
        ),
        (
            Tier::Giant.name().to_owned(),
            "同じ種類の作業のうち難しい個体。原因の分からない不具合、広い範囲にまたがる作り直し、途中で設計の判断が要るもの。手順は読み取れるが、そのとおりに進むとは限らない。".to_owned(),
        ),
        (
            Tier::Supergiant.name().to_owned(),
            "方針そのものを決める必要がある。設計の選択、互いに衝突する条件の調停など、正解が一つに定まらない。".to_owned(),
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
        let declared = || {
            tasks
                .iter()
                .map(|(agent, _)| agent.tier)
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
                    Question::Choice {
                        instructions: format!(
                            "次の依頼は、どの重さの作業か。\n\n担当する型: {}\n型の説明: {}\n依頼: {task}",
                            agent.name, agent.description
                        ),
                        criteria: criteria(),
                    },
                )
            })
            .collect();
        let state = serde_json::json!({
            "purpose": "委譲する作業の重さを決める",
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
            return declared();
        }
        let Ok(decision) = self.decider.decide(&state, &questions).await else {
            // Authentication, the service being unreachable, a malformed
            // reply: none of them is a reason to lose a delegation. The
            // declared weight is a complete answer on its own.
            return declared();
        };
        (0..tasks.len())
            .map(|index| {
                decision
                    .answers
                    .get(&key(index))
                    .and_then(|answer| answer.choice.as_deref())
                    .and_then(from_name)
                    .unwrap_or(tasks[index].0.tier)
            })
            .collect()
    }
}

/// The reply names an option rather than pointing at a position, so a name
/// nobody offered is the one thing that cannot be interpreted. It falls
/// back to the declaration rather than to a guess.
fn from_name(name: &str) -> Option<Tier> {
    Tier::ALL.into_iter().find(|tier| tier.name() == name)
}

#[cfg(test)]
#[path = "typed/tests.rs"]
mod tests;
