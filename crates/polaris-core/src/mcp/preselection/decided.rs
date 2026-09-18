//! J-3: the same preselection, decided by typed questions instead of by
//! reading indices out of a model's prose.
//!
//! One question per candidate, all in a single request. Each asks whether
//! that one tool does what the request needs, and the answers are
//! independent: a tool clears the threshold on its own merits, so two
//! equally useful tools do not take the decision away from one another.
//!
//! **A `Choice` cannot be used for this.** Its probabilities are a
//! distribution over mutually exclusive options and sum to 1, so the more
//! tools genuinely fit, the smaller each one's share becomes — five good
//! candidates would leave every one of them below a threshold that a single
//! good candidate clears easily. Independent `Noul`s have no such coupling.
//!
//! The lexical narrowing above is therefore not a relevance filter any
//! more. It runs only when the whole catalogue will not fit in one request,
//! which is what lets a tool whose wording shares nothing with the request
//! still be considered.
//!
//! A pick is not permission. Everything the provider-driven path checks —
//! the candidate actually being in the list, no duplicates, no more than
//! `PICKS` — is checked here too.
use super::{Entry, Status};
use polaris_provider::decision::{Decider, Question};
use std::future::Future;

/// The band rule and the wording live in [`crate::decision_bands`], shared
/// with skill preselection so the two cannot drift from the measurement
/// that produced the edges.
pub use crate::decision_bands::{ADOPT, EXCLUDE};

/// What the request is about. The catalogue is **not** here: each question
/// carries the one tool it asks about, so a tool's description is sent once
/// rather than once in the state and again in its question. Measured on the
/// service on 2026-09-17 at roughly 40 input tokens per candidate this way.
pub(super) fn state(query: &str, phase: &str) -> serde_json::Value {
    serde_json::json!({ "task": query, "phase": phase })
}

/// What one candidate adds to a request: its own question and nothing else.
/// Used to decide how many candidates fit before any of them is sent.
pub(super) fn question_text(query: &str, entry: &Entry<'_>) -> String {
    match crate::decision_bands::question(query, "tool", &entry.tool.name, &entry.tool.description)
    {
        Question::Noul { instructions } => instructions,
        _ => unreachable!("a candidate is always asked about with a Noul"),
    }
}

fn failed(refused: crate::decision_bands::Refused) -> Status {
    match refused {
        crate::decision_bands::Refused::Cancelled => Status::Cancelled,
        // A malformed request is a defect in how the host built it, and a
        // reply that does not fit its question is the service's. Neither is
        // "no tool fits", and both surface as an unusable output.
        crate::decision_bands::Refused::InvalidOutput => Status::InvalidOutput,
        // Too large, refused, unreachable, unauthenticated: the host could
        // not get an answer. None of them is "no tool fits".
        crate::decision_bands::Refused::Unavailable => Status::ProviderFailed,
    }
}

pub(super) use crate::decision_bands::Bands;

/// Judges candidates the caller has already assembled. `candidates` is the
/// whole catalogue when it fits, and a lexically narrowed slice of it when
/// it does not.
pub(super) async fn judge<'a>(
    decider: &dyn Decider,
    candidates: &[&'a Entry<'a>],
    query: &str,
    phase: &str,
    cancelled: impl Future<Output = ()>,
) -> Result<Bands, Status> {
    let named: Vec<(&str, &str)> = candidates
        .iter()
        .map(|entry| (entry.tool.name.as_str(), entry.tool.description.as_str()))
        .collect();
    crate::decision_bands::judge(
        decider,
        &state(query, phase),
        query,
        "tool",
        &named,
        cancelled,
    )
    .await
    .map_err(failed)
}

#[cfg(test)]
mod tests;
