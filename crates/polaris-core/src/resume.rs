//! M6-2b: whether a finished run should be continued without being asked.
//! Pure: no clock, no store, no process inspection. The rule is conservative
//! on purpose — the first run is not counted, one automatic continuation is
//! allowed after it, and anything unresolved refuses instead of guessing.
use polaris_desktop_protocol::run_state::RunState;
use serde::Serialize;

/// One automatic continuation after the first run; a person asks for more.
pub const MAX_AUTOMATIC: u64 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum Decision {
    Continue,
    Refuse { reason: Refusal },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// Nothing says the work is unfinished.
    NoGoal,
    /// The person stopped it; only an explicit resume clears that.
    Stopped,
    /// The previous attempt has not finished, or its result is unknown.
    Unsettled,
    /// It finished the way it was asked to, and nothing says the goal is
    /// still outstanding.
    Completed,
    /// Someone must answer first.
    AwaitingPerson,
    /// The automatic budget is spent.
    LimitReached,
}

/// What the owner knows when the previous run reaches its end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Observed {
    pub goal_active: bool,
    pub goal_stopped: bool,
    pub previous: Option<RunState>,
    pub awaiting_approval: bool,
    /// Automatic continuations already spent for this goal revision.
    pub automatic_used: u64,
    /// M6-4: whether the goal itself is done, as judged after the run.
    ///
    /// `None` is "nobody judged", which is the ordinary state and the one
    /// every run had before the judge existed. It is deliberately not the
    /// same as `Some(false)`: a run that succeeded is left alone unless
    /// something actually says the goal is still outstanding. Guessing
    /// from an absent judgement in either direction would either loop on
    /// finished work or abandon unfinished work.
    pub goal_achieved: Option<bool>,
}

pub fn decide(observed: &Observed) -> Decision {
    let refuse = |reason| Decision::Refuse { reason };
    if !observed.goal_active {
        return refuse(Refusal::NoGoal);
    }
    if observed.goal_stopped {
        return refuse(Refusal::Stopped);
    }
    if observed.awaiting_approval {
        return refuse(Refusal::AwaitingPerson);
    }
    match observed.previous {
        // A run that is still going, or whose result is unknown, is never
        // replaced: that would risk doing the same work twice.
        None
        | Some(
            RunState::Queued
            | RunState::Loading
            | RunState::Running
            | RunState::AwaitingApproval
            | RunState::Cancelling,
        ) => {
            return refuse(Refusal::Unsettled);
        }
        // A run can finish exactly as asked and still leave the goal
        // undone — it was one step of it. That is the only thing the
        // judgement adds here, and it adds nothing when absent.
        Some(RunState::Succeeded) => {
            if observed.goal_achieved != Some(false) {
                return refuse(Refusal::Completed);
            }
        }
        Some(RunState::Cancelled) => return refuse(Refusal::Stopped),
        Some(RunState::OutcomeUnknown) => return refuse(Refusal::Unsettled),
        Some(RunState::Failed | RunState::Interrupted) => {}
    }
    if observed.automatic_used >= MAX_AUTOMATIC {
        return refuse(Refusal::LimitReached);
    }
    Decision::Continue
}

#[cfg(test)]
mod tests;

/// M6-4: asking whether the goal itself is done, after a run that finished
/// the way it was asked to.
///
/// This is one typed question, not a model turn. That is what makes it
/// possible at all: the judgement was deferred because sending to the main
/// model after a run needed a job path, cancellation, usage accounting
/// against the person's account and an account binding. A typed judge is a
/// separate service billed on its own, so none of that applies — what was
/// missing was the send path, and this does not use it.
pub mod achieved {
    use polaris_provider::decision::{Decider, Question};
    use std::collections::BTreeMap;

    /// The evidence is bounded here rather than at the call site, so a long
    /// run cannot turn one short question into a large request.
    pub const MAX_EVIDENCE_BYTES: usize = 4_096;

    fn bounded(text: &str) -> &str {
        let mut end = text.len().min(MAX_EVIDENCE_BYTES);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        &text[..end]
    }

    /// `None` whenever the answer is not usable, which the caller must not
    /// read as either verdict: an absent judgement leaves a finished run
    /// alone, and a wrong one either loops on finished work or abandons
    /// unfinished work.
    pub async fn judge(
        decider: &dyn Decider,
        goal: &str,
        evidence: &str,
        cancelled: impl std::future::Future<Output = ()>,
    ) -> Option<bool> {
        if goal.trim().is_empty() {
            return None;
        }
        if decider.capabilities().band_calibration
            != polaris_provider::decision::BandCalibration::LegacyJev
        {
            return None;
        }
        let mut questions = BTreeMap::new();
        questions.insert(
            "achieved".to_owned(),
            Question::Noul {
                instructions: format!(
                    "この目標は達成されたか。目標: {goal}\n直前の実行が返したもの: {}",
                    bounded(evidence)
                ),
            },
        );
        let state = serde_json::json!({ "purpose": "目標が達成されたかを判定する" });
        tokio::pin!(cancelled);
        let answered = tokio::select! {
            biased;
            _ = &mut cancelled => return None,
            held = decider.decide(&state, &questions) => held,
        };
        let answer = answered.ok()?;
        polaris_provider::decision::checked(&answer, &questions).ok()?;
        let question = questions.get("achieved")?;
        let held = answer.answers.get("achieved")?;
        let yes = held.answers(question).then_some(held.noul).flatten()?;
        // The two edges the rest of the host uses, for the same reason:
        // between them the answer is not a decision, and continuing or
        // stopping on a coin flip is worse than leaving the run alone.
        if yes >= crate::decision_bands::ADOPT {
            Some(true)
        } else if yes <= crate::decision_bands::EXCLUDE {
            Some(false)
        } else {
            None
        }
    }
}
