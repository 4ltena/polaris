//! Pricing a wave with an ordinary model, for runs with no typed decider.
//!
//! The answer is constrained to one weight name per task and nothing else.
//! A model that writes prose here has not answered, and its reply is
//! discarded rather than parsed out of — an explanation that happens to
//! contain the word "ultra" is not a decision.
use polaris_provider::{CompletionRequest, Message, Provider};
use polaris_skills::{AgentType, Tier};
use std::sync::Arc;

use super::TierDecider;

pub struct ChatTierDecider {
    provider: Arc<dyn Provider>,
}

impl ChatTierDecider {
    pub fn new(provider: Arc<dyn Provider>) -> Self {
        Self { provider }
    }
}

fn system() -> String {
    let names = Tier::ALL
        .into_iter()
        .map(Tier::name)
        .collect::<Vec<_>>()
        .join(" / ");
    format!(
        "委譲する作業の重さを決める。番号の付いた依頼ごとに、{names} のいずれか1語だけを1行で返す。\n\
         low は一つの明確な手順で終わる作業、medium は複数の手順を組み立てるが方針は依頼から読み取れる作業、\n\
         ultra は方針そのものを決める必要がある作業を指す。\n\
         行数は依頼の数と一致させる。説明、番号、記号、前置きは書かない。"
    )
}

#[async_trait::async_trait]
impl TierDecider for ChatTierDecider {
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
        let body = tasks
            .iter()
            .enumerate()
            .map(|(index, (agent, task))| {
                format!("{}. 型: {} / 依頼: {task}", index + 1, agent.name)
            })
            .collect::<Vec<_>>()
            .join("\n");
        let request = CompletionRequest {
            system: system(),
            messages: vec![Message::user(body)],
            tools: vec![],
        };
        let Ok(response) = self.provider.complete(request).await else {
            return declared();
        };
        let Some(answered) = read(&response.text, tasks.len()) else {
            return declared();
        };
        answered
            .into_iter()
            .zip(tasks)
            .map(|(chosen, (agent, _))| chosen.unwrap_or(agent.tier))
            .collect()
    }
}

/// One weight per line, nothing else on the line. Returns `None` when the
/// reply is not that shape at all, so the caller can keep every
/// declaration rather than accept a partial reading of prose.
fn read(text: &str, expected: usize) -> Option<Vec<Option<Tier>>> {
    let lines: Vec<&str> = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    if lines.len() != expected {
        return None;
    }
    Some(
        lines
            .into_iter()
            .map(|line| Tier::ALL.into_iter().find(|tier| tier.name() == line))
            .collect(),
    )
}

#[cfg(test)]
#[path = "chat/tests.rs"]
mod tests;
