//! M5-3b: the usage fetch job, its memory-only cache and the selection input
//! built from it. An observation never reaches a file and never crosses a
//! process; a read-only caller has no owner, so it never fetches.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use crate::AuthError;
use crate::accounts::owner::{AccessToken, AccountOwner};
use crate::accounts::store::AccountState;
use crate::quota::usage::{self, UsageProfile};
use crate::quota::{
    EvaluationTime, ObservationState, QuotaObservation, RegisteredAccount, RegistrySnapshot,
    SelectionOutcome, select_account,
};

/// Per process, per registration. Two processes fetch independently.
const MIN_INTERVAL: Duration = Duration::from_secs(60);

/// A process-lifetime identity and monotonic origin, so an observation made
/// by an earlier process can never be treated as fresh.
fn epoch() -> (u64, Instant) {
    static EPOCH: OnceLock<(u64, Instant)> = OnceLock::new();
    *EPOCH.get_or_init(|| {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(1, |since| since.as_nanos() as u64);
        (
            nanos ^ (u64::from(std::process::id()) << 48),
            Instant::now(),
        )
    })
}

pub fn evaluation_time() -> EvaluationTime {
    let (process_epoch, origin) = epoch();
    EvaluationTime {
        process_epoch,
        monotonic_millis: origin.elapsed().as_millis() as u64,
        unix_seconds: crate::accounts::owner::now_secs(),
    }
}

#[derive(Default)]
pub(crate) struct Observations {
    states: HashMap<(String, String), QuotaObservation>,
    attempts: HashMap<String, Instant>,
}

impl AccountOwner {
    /// One usage request for a registration and model. Every failure becomes
    /// `fetch_failed`, so a stale reading is never kept, and no credential is
    /// discarded. Returns false when the interval has not elapsed.
    pub async fn observe(
        self: &Arc<Self>,
        registration_id: &str,
        generation: u64,
        model_id: &str,
    ) -> bool {
        self.observe_with(
            usage::profile_for(model_id),
            registration_id,
            generation,
            model_id,
        )
        .await
    }

    pub(crate) async fn observe_with(
        self: &Arc<Self>,
        profile: Option<&UsageProfile>,
        registration_id: &str,
        generation: u64,
        model_id: &str,
    ) -> bool {
        let state = match profile {
            None => Some(ObservationState::Unsupported),
            // An unverified contract is not worth a request to a real host.
            Some(profile) if !profile.verified => Some(ObservationState::MappingUnknown),
            Some(_) => None,
        };
        if let Some(state) = state {
            self.record(registration_id, generation, model_id, state);
            return true;
        }
        let profile = profile.expect("a verified profile");
        {
            let mut cache = self.observations();
            let now = Instant::now();
            if let Some(last) = cache.attempts.get(registration_id)
                && now.duration_since(*last) < MIN_INTERVAL
            {
                return false;
            }
            cache.attempts.insert(registration_id.to_owned(), now);
        }
        let state = self
            .fetch_observation(profile, registration_id, generation)
            .await
            .unwrap_or(ObservationState::FetchFailed);
        // A logout or re-authentication during the request discards the result.
        if self.store_view(registration_id, generation).is_err() {
            return true;
        }
        self.record(registration_id, generation, model_id, state);
        true
    }

    /// Holds neither the owner lock nor a slot lease; a refresh is delegated
    /// to the existing owner path, and only one 401 retry is allowed.
    async fn fetch_observation(
        self: &Arc<Self>,
        profile: &UsageProfile,
        registration_id: &str,
        generation: u64,
    ) -> Result<ObservationState, AuthError> {
        let token = self.token(registration_id, generation).await?;
        let response = match self.request(profile, &token).await {
            Err(AuthError::Http(kind)) if kind == "status 401" => {
                let token = self
                    .refresh_after_unauthorized(registration_id, generation, token.token_revision)
                    .await?;
                self.request(profile, &token).await?
            }
            other => other?,
        };
        Ok(
            usage::observe(profile, &response, crate::accounts::owner::now_secs())
                .unwrap_or(ObservationState::FetchFailed),
        )
    }

    async fn request(
        &self,
        profile: &UsageProfile,
        token: &AccessToken,
    ) -> Result<usage::UsageResponse, AuthError> {
        usage::fetch(
            self.usage_base(),
            profile.endpoint,
            token.secret(),
            &token.account_id,
        )
        .await
    }

    fn record(
        &self,
        registration_id: &str,
        generation: u64,
        model_id: &str,
        state: ObservationState,
    ) {
        let now = evaluation_time();
        self.observations().states.insert(
            (registration_id.to_owned(), model_id.to_owned()),
            QuotaObservation {
                registration_id: registration_id.to_owned(),
                generation,
                model_id: model_id.to_owned(),
                process_epoch: now.process_epoch,
                captured_monotonic_millis: now.monotonic_millis,
                captured_unix_seconds: now.unix_seconds,
                state,
            },
        );
    }

    /// What the last attempt for this registration and model produced.
    pub fn observation(&self, registration_id: &str, model_id: &str) -> Option<QuotaObservation> {
        self.observations()
            .states
            .get(&(registration_id.to_owned(), model_id.to_owned()))
            .cloned()
    }

    /// The registrations as selection input; an account never observed in
    /// this process is `missing`, never an old value. Every model the run will
    /// use must be known, so a run cannot send its summary or its light model
    /// to an account whose coverage for them is unknown.
    pub fn selection_snapshot(&self, models: &[&str]) -> Result<RegistrySnapshot, AuthError> {
        let model_id = *models.first().ok_or(AuthError::NotLoggedIn)?;
        let snapshot = self.snapshot()?;
        let now = evaluation_time();
        let cache = self.observations();
        Ok(RegistrySnapshot {
            revision: snapshot.registry_revision,
            accounts: snapshot
                .accounts
                .into_iter()
                .map(|account| {
                    let mut observation = cache
                        .states
                        .get(&(account.registration_id.clone(), model_id.to_owned()))
                        .cloned()
                        .unwrap_or_else(|| QuotaObservation {
                            registration_id: account.registration_id.clone(),
                            generation: account.generation,
                            model_id: model_id.to_owned(),
                            process_epoch: now.process_epoch,
                            captured_monotonic_millis: now.monotonic_millis,
                            captured_unix_seconds: now.unix_seconds,
                            state: ObservationState::Missing,
                        });
                    // The first model without a known observation decides the
                    // reason; otherwise the binding model's reading is used.
                    if let Some(unknown) = models
                        .iter()
                        .map(|model| {
                            cache
                                .states
                                .get(&(account.registration_id.clone(), (*model).to_owned()))
                                .map_or(ObservationState::Missing, |observed| {
                                    observed.state.clone()
                                })
                        })
                        .find(|state| !matches!(state, ObservationState::Known(_)))
                    {
                        observation.state = unknown;
                    }
                    RegisteredAccount {
                        registration_id: account.registration_id,
                        generation: account.generation,
                        registration_order: account.registration_order,
                        principal_key: account.principal_key,
                        automatic_use_allowed: true,
                        authentication_usable: account.state == AccountState::Ready,
                        observation,
                    }
                })
                .collect(),
        })
    }

    /// The registration a new run must use. One usable registration is taken
    /// as it is; only a choice between several consults the quota, which
    /// refuses rather than guessing. An explicit id skips the selection and
    /// is accepted even with no observation.
    pub fn bind(
        &self,
        models: &[&str],
        explicit: Option<&str>,
    ) -> Result<(String, u64), AuthError> {
        let snapshot = self.snapshot()?;
        if let Some(id) = explicit {
            let account = snapshot
                .accounts
                .iter()
                .find(|account| account.registration_id == id)
                .ok_or(AuthError::NotLoggedIn)?;
            if account.state != AccountState::Ready {
                return Err(AuthError::Denied("account_not_ready".into()));
            }
            return Ok((account.registration_id.clone(), account.generation));
        }
        let mut usable = snapshot
            .accounts
            .iter()
            .filter(|account| account.state == AccountState::Ready);
        match (usable.next(), usable.next()) {
            (None, _) => Err(AuthError::NotLoggedIn),
            (Some(only), None) => Ok((only.registration_id.clone(), only.generation)),
            _ => match self.select(models)? {
                SelectionOutcome::Selected { selection, .. } => {
                    Ok((selection.registration_id, selection.generation))
                }
                _ => Err(AuthError::Denied("no_selectable_account".into())),
            },
        }
    }

    /// `models` is every model the run will use; the first is the one the
    /// quota is judged on.
    pub fn select(&self, models: &[&str]) -> Result<SelectionOutcome, AuthError> {
        let model_id = *models.first().ok_or(AuthError::NotLoggedIn)?;
        Ok(select_account(
            &self.selection_snapshot(models)?,
            model_id,
            evaluation_time(),
        ))
    }
}

/// The only way a run reaches a token: one registration and generation, fixed
/// when the run was accepted.
#[derive(Clone)]
pub struct BoundTokenSource {
    owner: Arc<AccountOwner>,
    registration_id: String,
    generation: u64,
}

impl BoundTokenSource {
    pub fn new(owner: Arc<AccountOwner>, registration_id: String, generation: u64) -> Self {
        Self {
            owner,
            registration_id,
            generation,
        }
    }

    pub fn registration_id(&self) -> &str {
        &self.registration_id
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    pub async fn token(&self) -> Result<AccessToken, AuthError> {
        self.owner
            .token(&self.registration_id, self.generation)
            .await
    }

    pub async fn refresh_after_unauthorized(
        &self,
        used_revision: u64,
    ) -> Result<AccessToken, AuthError> {
        self.owner
            .refresh_after_unauthorized(&self.registration_id, self.generation, used_revision)
            .await
    }
}

impl std::fmt::Debug for BoundTokenSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BoundTokenSource")
            .field("registration_id", &self.registration_id)
            .field("generation", &self.generation)
            .finish()
    }
}

#[cfg(test)]
mod tests;
