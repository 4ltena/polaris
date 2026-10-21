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

use polaris_provider::decision::{BandCalibration, Decider, DecisionError, Question, checked};
mod fallback;

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
    /// Historical policy or an explicitly unmeasured conservative result.
    pub calibration: BandCalibration,
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

impl Bands {
    /// Context-only callers have no arbiter stage. Keep unmeasured candidates
    /// eligible instead of treating an empty adopted band as an empty result.
    pub(crate) fn context_candidates(self) -> Vec<usize> {
        let mut held = self.adopted;
        if self.calibration == BandCalibration::Unmeasured {
            held.extend(self.middle);
        }
        held
    }
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
    judge_with(
        decider,
        state,
        candidates
            .iter()
            .map(|(name, description)| question(query, kind, name, description))
            .collect(),
        cancelled,
    )
    .await
}

/// The same banding, for a caller that writes its own question.
///
/// Split out because the wording above is fixed to 「役に立つか」 and its
/// two edges were calibrated on exactly that phrasing. A caller asking
/// something else needs the scoring, the completeness check and the
/// ordering — not the sentence. Keeping one copy of the scoring is what
/// stops a second set of edges appearing.
///
/// **A caller with its own wording does not inherit the calibration.** The
/// edges still divide the answers, but what they mean for that question is
/// unmeasured until it is measured. Read the adopted band as a suggestion
/// there, not as a verdict.
pub async fn judge_with(
    decider: &dyn Decider,
    state: &serde_json::Value,
    asked: Vec<Question>,
    cancelled: impl Future<Output = ()>,
) -> Result<Bands, Refused> {
    let questions: BTreeMap<String, Question> = asked
        .into_iter()
        .enumerate()
        .map(|(index, question)| (key(index), question))
        .collect();
    let count = questions.len();

    tokio::pin!(cancelled);
    let calibration = decider.capabilities().band_calibration;
    if !decider.may_return_calibrated_bands() {
        // These thresholds cannot turn an uncalibrated probability into a
        // decision. Leave every candidate for the existing arbiter/context
        // budget; no pointless model request is sent for a discarded score.
        tokio::select! {
            biased;
            _ = &mut cancelled => return Err(Refused::Cancelled),
            _ = std::future::ready(()) => {}
        }
        return Ok(Bands {
            middle: (0..count).collect(),
            calibration,
            ..Default::default()
        });
    }
    let answered = tokio::select! {
        biased;
        _ = &mut cancelled => return Err(Refused::Cancelled),
        held = decider.observe(state, &questions) => held,
    };
    let calibration = answered.capabilities.band_calibration;
    let answered = answered.result.map_err(Refused::from)?;
    checked(&answered, &questions).map_err(Refused::from)?;
    if calibration == BandCalibration::Unmeasured {
        return Ok(Bands {
            middle: (0..count).collect(),
            calibration,
            ..Default::default()
        });
    }

    // Every candidate must come back answered. A reply that skips one is
    // not a narrower answer, it is an answer to a different question set.
    let mut scored: Vec<(usize, f64)> = Vec::new();
    let mut excluded = 0;
    for index in 0..count {
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
        calibration,
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

/// Normal routing can use the backend's bounded retry policy. A finalizer
/// receipt covers one observation and must never become retry traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecisionUse {
    Routing,
    Finalization,
}

#[derive(Debug, thiserror::Error)]
pub enum DecisionBuildError {
    #[error("local-only: 判断器の外部通信禁止境界を確認できません")]
    LocalOnlyUnavailable,
    #[error("System Oneの認証参照を解決できません")]
    MissingSystemOneKey,
    #[error("POLARIS_JEV_API_KEY がありません")]
    MissingJevKey,
    #[error("helperが実在する絶対pathではありません")]
    MissingSemIfHelper,
    #[error("判断器を構築できません ({0:?})")]
    InvalidSettings(DecisionError),
    #[error("Clefでは固定model profile IDだけを指定できます")]
    InvalidClefSettings,
    #[error("{0}")]
    Clef(#[from] crate::managed_decision::ManagedDecisionError),
}

/// One factory for CLI and desktop. Looking up a remote credential is lazy:
/// disabled decisions and a selected local backend never read that key.
pub fn build_configured(
    config: &crate::config::DecisionConfig,
    purpose: DecisionUse,
) -> Result<Option<std::sync::Arc<dyn Decider>>, DecisionBuildError> {
    build_with_credentials(
        config,
        purpose,
        |name| std::env::var(name).ok(),
        crate::managed_decision::build,
    )
}

#[cfg(test)]
fn build_with_key(
    config: &crate::config::DecisionConfig,
    purpose: DecisionUse,
    key: impl FnMut() -> Option<String>,
) -> Result<Option<std::sync::Arc<dyn Decider>>, DecisionBuildError> {
    build_with_sources(config, purpose, key, crate::managed_decision::build)
}

#[cfg(test)]
fn build_with_sources(
    config: &crate::config::DecisionConfig,
    purpose: DecisionUse,
    mut key: impl FnMut() -> Option<String>,
    clef: impl FnMut(
        &str,
    ) -> Result<
        std::sync::Arc<dyn Decider>,
        crate::managed_decision::ManagedDecisionError,
    >,
) -> Result<Option<std::sync::Arc<dyn Decider>>, DecisionBuildError> {
    build_with_credentials(config, purpose, |_| key(), clef)
}

fn build_with_credentials(
    config: &crate::config::DecisionConfig,
    purpose: DecisionUse,
    mut key: impl FnMut(&str) -> Option<String>,
    mut clef: impl FnMut(
        &str,
    ) -> Result<
        std::sync::Arc<dyn Decider>,
        crate::managed_decision::ManagedDecisionError,
    >,
) -> Result<Option<std::sync::Arc<dyn Decider>>, DecisionBuildError> {
    if config.enabled && purpose == DecisionUse::Routing && config.fallback.is_some() {
        return fallback::build(config, &mut key, &mut clef).map(Some);
    }
    build_one_with_sources(config, purpose, key, clef, false)
}

fn build_one_with_sources(
    config: &crate::config::DecisionConfig,
    purpose: DecisionUse,
    mut key: impl FnMut(&str) -> Option<String>,
    clef: impl FnOnce(
        &str,
    ) -> Result<
        std::sync::Arc<dyn Decider>,
        crate::managed_decision::ManagedDecisionError,
    >,
    single_attempt: bool,
) -> Result<Option<std::sync::Arc<dyn Decider>>, DecisionBuildError> {
    use crate::config::DecisionBackend;
    use polaris_provider::decision::{jev::JevDecider, semif::SemIfDecider};

    if !config.enabled {
        return Ok(None);
    }
    let decider: std::sync::Arc<dyn Decider> = match config.backend {
        DecisionBackend::Clef => {
            // Finalizer activation needs its own accepted profile/receipt;
            // never substitute the remote judge or infer that acceptance.
            if purpose == DecisionUse::Finalization {
                return Ok(None);
            }
            let profile = config
                .model
                .as_deref()
                .ok_or(DecisionBuildError::InvalidClefSettings)?;
            if config.helper.is_some()
                || config.base_url.is_some()
                || !crate::managed_decision::valid_profile_id(profile)
            {
                return Err(DecisionBuildError::InvalidClefSettings);
            }
            clef(profile)?
        }
        DecisionBackend::SemIf => {
            // SemIf has no finalizer activation contract yet. Preserve its
            // routing support without silently substituting a remote judge.
            if purpose == DecisionUse::Finalization {
                return Ok(None);
            }
            std::sync::Arc::new(
                SemIfDecider::available(config.helper.as_deref())
                    .ok_or(DecisionBuildError::MissingSemIfHelper)?,
            )
        }
        DecisionBackend::SystemOne => {
            // No finalizer acceptance contract is inferred from HTTP compatibility.
            if purpose == DecisionUse::Finalization {
                return Ok(None);
            }
            if config.local_only {
                return Err(DecisionBuildError::LocalOnlyUnavailable);
            }
            let profile = config
                .system_one
                .as_ref()
                .ok_or(DecisionBuildError::InvalidSettings(
                    DecisionError::Malformed,
                ))?;
            if config.base_url.is_some() || config.model.is_some() || config.helper.is_some() {
                return Err(DecisionBuildError::InvalidSettings(
                    DecisionError::Malformed,
                ));
            }
            profile
                .validate()
                .map_err(|_| DecisionBuildError::InvalidSettings(DecisionError::Malformed))?;
            let target = profile
                .target()
                .map_err(DecisionBuildError::InvalidSettings)?;
            let binding = target.binding();
            // User-owned metadata is validated before the credential reference is read.
            let secret =
                key(&profile.credential_env).ok_or(DecisionBuildError::MissingSystemOneKey)?;
            let credential =
                polaris_provider::decision::system_one::BoundCredential::new(binding, secret)
                    .map_err(DecisionBuildError::InvalidSettings)?;
            std::sync::Arc::new(
                polaris_provider::decision::system_one::SystemOneDecider::new(
                    target,
                    credential,
                    profile.limits(),
                )
                .map_err(DecisionBuildError::InvalidSettings)?,
            )
        }
        DecisionBackend::Jev => {
            if config.local_only {
                return Err(DecisionBuildError::LocalOnlyUnavailable);
            }
            let key = key("POLARIS_JEV_API_KEY").ok_or(DecisionBuildError::MissingJevKey)?;
            let base = config
                .base_url
                .as_deref()
                .unwrap_or("https://api.typesafe.ai/v1");
            let model = config.model.clone().unwrap_or_else(|| "jev-latest".into());
            let client = match purpose {
                DecisionUse::Routing if single_attempt => {
                    JevDecider::new_single_attempt(base, key, model)
                }
                DecisionUse::Routing => JevDecider::new(base, key, model),
                DecisionUse::Finalization => JevDecider::new_finalizer(base, key, model),
            }
            .map_err(DecisionBuildError::InvalidSettings)?;
            std::sync::Arc::new(client)
        }
    };
    let decider = if config.backend == DecisionBackend::Clef {
        fallback::identify(config, decider)
    } else {
        decider
    };
    crate::decision_policy::restrict_decider(config.local_only, decider)
        .map(Some)
        .map_err(|_| DecisionBuildError::LocalOnlyUnavailable)
}

/// Optional routing for surfaces that already report unavailable capability
/// separately. Call `build_configured` when a diagnostic reason is needed.
pub fn from_configuration(
    config: &crate::config::DecisionConfig,
) -> Option<std::sync::Arc<dyn Decider>> {
    build_configured(config, DecisionUse::Routing)
        .ok()
        .flatten()
}

#[cfg(test)]
#[path = "decision_bands/tests.rs"]
mod tests;
