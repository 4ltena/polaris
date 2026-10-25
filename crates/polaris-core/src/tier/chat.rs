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
        "Determine the weight of delegated work. For each numbered request, return exactly one word from {names} on its own line.\nlow means a task completed by one clear step; medium requires several steps with a direction evident from the request; ultra requires choosing the direction itself.\nReturn exactly one line per request. Do not include explanations, numbers, symbols, or introductory text."
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
                format!("{}. Type: {} / request: {task}", index + 1, agent.name)
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
