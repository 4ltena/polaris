//! The band rule, in one place.
//!
//! Tools and skills are preselected the same way and must stay that way: a
//! second copy of "0.8 adopts, 0.4 drops" would drift from the measurement
//! that produced those numbers, and the wording of the question is part of
//! that measurement too.
//!
//! One independent `Noul` per candidate, all in a single request. **A
//! `Choice` cannot be used for this.** Its probabilities are a distribution
//! over mutually exclusive options and sum to 1, so the more candidates
//! genuinely fit, the smaller each one's share becomes — five good ones
//! would leave every one below a threshold a single good one clears
//! easily. Independent answers have no such coupling. (Weighing a
//! delegated task is the opposite case, and uses a `Choice` for exactly
//! that reason.)
use std::collections::BTreeMap;
use std::future::Future;

use polaris_provider::decision::{Decider, DecisionError, Question};

/// The two edges of the middle band. Above `ADOPT` a candidate is taken on
/// the typed answer alone; at or below `EXCLUDE` it is dropped; between
/// them the answer is not a decision and a light model compares what is
/// left against each other.
///
/// Measured on the service on 2026-09-17 against a synthetic catalogue of
/// 24 tools and three tasks: nothing was adopted wrongly and nothing needed
/// was dropped. That is the only evidence for these two numbers; a real
/// catalogue is larger and its descriptions are not written to one
/// standard, so J-5 re-measures them on real work.
pub const ADOPT: f64 = 0.8;
pub const EXCLUDE: f64 = 0.4;

/// **"役に立つか", not "達成するか".** Asking whether a candidate
/// accomplishes the request makes a means to the end score like a failure:
/// `read_file` measured 0.45 against "does it accomplish this" and 0.83
/// against "is it useful for this", and 0.45 sits below the exclusion edge.
/// The wording is part of the calibration, not decoration — changing it
/// invalidates the two numbers above.
pub fn question(query: &str, kind: &str, name: &str, description: &str) -> Question {
    Question::Noul {
        instructions: format!(
            "この作業を進めるうえで、この{kind}は役に立つか。依頼: {query} / {kind}: {name} — {description}"
        ),
    }
}

/// Which band each candidate landed in, as indices into the list the caller
/// passed. Both are ordered most confident first; ties keep the order the
/// candidates arrived in, so the result never depends on sort details.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Bands {
    /// Taken on the typed answer alone.
    pub adopted: Vec<usize>,
    /// Neither taken nor dropped. Only these are worth a light model's
    /// attention, and only these are shown to it.
    pub middle: Vec<usize>,
    /// Dropped. Kept as a count because the share of the catalogue each
    /// band takes is what the calibration measures; the candidates
    /// themselves are not needed again.
    pub excluded: usize,
}

/// Why a judgement produced nothing. Callers map this onto whatever their
/// own surface reports; the distinction that matters to all of them is that
/// none of these means "nothing fits".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Refused {
    Cancelled,
    /// The reply did not answer the questions that were asked, or the host
    /// built a request the service could not read. Neither is the
    /// service being unable to answer right now.
    InvalidOutput,
    /// Too large, refused, unreachable, unauthenticated.
    Unavailable,
}

impl From<DecisionError> for Refused {
    fn from(error: DecisionError) -> Self {
        match error {
            DecisionError::Cancelled => Self::Cancelled,
            DecisionError::InvalidAnswer | DecisionError::Malformed => Self::InvalidOutput,
            _ => Self::Unavailable,
        }
    }
}

fn key(index: usize) -> String {
    format!("t-{index}")
}

/// Judges `candidates`, given as `(name, description)`, against `state`.
/// `kind` is the word the question uses for what is being judged.
pub async fn judge(
    decider: &dyn Decider,
    state: &serde_json::Value,
    query: &str,
    kind: &str,
    candidates: &[(&str, &str)],
    cancelled: impl Future<Output = ()>,
) -> Result<Bands, Refused> {
    let questions: BTreeMap<String, Question> = candidates
        .iter()
        .enumerate()
        .map(|(index, (name, description))| (key(index), question(query, kind, name, description)))
        .collect();

    tokio::pin!(cancelled);
    let answered = tokio::select! {
        biased;
        _ = &mut cancelled => return Err(Refused::Cancelled),
        held = decider.decide(state, &questions) => held,
    };
    let answered = answered.map_err(Refused::from)?;

    // Every candidate must come back answered. A reply that skips one is
    // not a narrower answer, it is an answer to a different question set.
    let mut scored: Vec<(usize, f64)> = Vec::new();
    let mut excluded = 0;
    for index in 0..candidates.len() {
        let key = key(index);
        let (Some(answer), Some(question)) = (answered.answers.get(&key), questions.get(&key))
        else {
            return Err(Refused::InvalidOutput);
        };
        let Some(yes) = answer.answers(question).then_some(answer.noul).flatten() else {
            return Err(Refused::InvalidOutput);
        };
        if yes <= EXCLUDE {
            excluded += 1;
        } else {
            scored.push((index, yes));
        }
    }
    scored.sort_by(|left, right| {
        right
            .1
            .partial_cmp(&left.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(left.0.cmp(&right.0))
    });
    let mut bands = Bands {
        excluded,
        ..Default::default()
    };
    for (index, yes) in scored {
        if yes >= ADOPT {
            bands.adopted.push(index);
        } else {
            bands.middle.push(index);
        }
    }
    Ok(bands)
}

/// Builds the judge every surface shares, from the configuration every
/// surface reads.
///
/// `None` both when it is switched off and when it is on but unusable:
/// every path that asks it something works without an answer, so neither
/// is a reason to refuse to start. The API key is read from the
/// environment and never from the configuration file, which is shared.
pub fn from_configuration(
    config: &crate::config::DecisionConfig,
) -> Option<std::sync::Arc<dyn Decider>> {
    if !config.enabled {
        return None;
    }
    let key = std::env::var("POLARIS_JEV_API_KEY").ok()?;
    let base = config
        .base_url
        .clone()
        .unwrap_or_else(|| "https://api.typesafe.ai/v1".to_owned());
    let model = config
        .model
        .clone()
        .unwrap_or_else(|| "jev-latest".to_owned());
    polaris_provider::decision::jev::JevDecider::new(&base, key, model)
        .ok()
        .map(|decider| std::sync::Arc::new(decider) as std::sync::Arc<dyn Decider>)
}

#[cfg(test)]
#[path = "decision_bands/tests.rs"]
mod tests;
