//! M5-2c: tokens bound to one registration, a single refresh owner per slot
//! across processes, and the login exchange for new or re-authenticated
//! accounts. Each POST runs in an owned task, so a cancelled waiter never
//! drops a sent request or its lease, and an unknown result is never resent.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::accounts::directory::SlotLease;
use crate::accounts::observation::Observations;
use crate::accounts::store::{AccountState, AccountStore, Reservation, SlotView};
use crate::token::{IdentityRule, post_token_bound};
use crate::{AuthError, CLIENT_ID, Credentials, REDIRECT_URI};

const LEASE_WAIT: Duration = Duration::from_secs(35);
const SAVE_WAIT: Duration = Duration::from_secs(5);
const POLL: Duration = Duration::from_millis(100);

/// A token for one registration generation and token revision.
pub struct AccessToken {
    pub registration_id: String,
    pub generation: u64,
    pub token_revision: u64,
    pub account_id: String,
    access_token: String,
}

impl AccessToken {
    pub fn secret(&self) -> &str {
        &self.access_token
    }
}

impl std::fmt::Debug for AccessToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AccessToken")
            .field("registration_id", &self.registration_id)
            .field("generation", &self.generation)
            .field("token_revision", &self.token_revision)
            .finish_non_exhaustive()
    }
}

fn reauth_required() -> AuthError {
    AuthError::Denied("reauth_required".into())
}

fn cancelled() -> AuthError {
    AuthError::Denied("cancelled".into())
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// The saved token when it answers this request without a POST.
fn usable(
    view: &SlotView,
    registration_id: &str,
    generation: u64,
    used: Option<u64>,
) -> Result<Option<AccessToken>, AuthError> {
    if used.is_some_and(|used| used > view.token_revision) {
        return Err(AuthError::Denied("future_token_revision".into()));
    }
    let (AccountState::Ready, Some(credentials)) = (view.state, &view.credentials) else {
        return Ok(None);
    };
    let current = match used {
        Some(used) => used < view.token_revision,
        None => !crate::needs_refresh(credentials, now_secs()),
    };
    Ok(current.then(|| {
        token(
            registration_id,
            generation,
            view.token_revision,
            credentials,
        )
    }))
}

fn token(
    registration_id: &str,
    generation: u64,
    revision: u64,
    credentials: &Credentials,
) -> AccessToken {
    AccessToken {
        registration_id: registration_id.into(),
        generation,
        token_revision: revision,
        account_id: credentials.account_id.clone(),
        access_token: credentials.access_token.clone(),
    }
}

pub struct AccountOwner {
    store: AccountStore,
    issuer: String,
    usage_base: String,
    observations: Mutex<Observations>,
    flights: Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    /// Holds a saved login result inside its task so a test can fix the order
    /// in which a cancel and a completion land.
    #[cfg(test)]
    pub(crate) completion_gate: Mutex<Option<Arc<tokio::sync::Semaphore>>>,
}

impl AccountOwner {
    /// The mutable owner; settles registrations and refreshes left by an
    /// owner that is gone, without any HTTP.
    pub fn open(issuer: &str) -> Result<Arc<Self>, AuthError> {
        Self::with_store(AccountStore::open()?, issuer)
    }

    /// The owner only when a registration exists. An absent or empty registry
    /// keeps the caller on the single authentication; a registry that needs
    /// recovery is settled by opening the owner, never by falling back.
    pub fn open_if_registered(issuer: &str) -> Result<Option<Arc<Self>>, AuthError> {
        match AccountStore::read_only_snapshot() {
            Ok(snapshot) if snapshot.accounts.is_empty() => return Ok(None),
            Ok(_) | Err(AuthError::RecoveryRequired) => {}
            // accountsへ到達できない環境では登録も作れないため、単一認証のまま。
            Err(AuthError::ReadOnly | AuthError::Protection) => return Ok(None),
            Err(error) => return Err(error),
        }
        let owner = Self::open(issuer)?;
        let settled = owner.snapshot()?;
        Ok((!settled.accounts.is_empty()).then_some(owner))
    }

    fn with_store(store: AccountStore, issuer: &str) -> Result<Arc<Self>, AuthError> {
        Self::with_usage_base(store, issuer, crate::quota::usage::USAGE_BASE)
    }

    pub(crate) fn with_usage_base(
        store: AccountStore,
        issuer: &str,
        usage_base: &str,
    ) -> Result<Arc<Self>, AuthError> {
        let owner = Arc::new(Self {
            store,
            issuer: issuer.to_owned(),
            usage_base: usage_base.to_owned(),
            observations: Mutex::default(),
            flights: Mutex::default(),
            #[cfg(test)]
            completion_gate: Mutex::default(),
        });
        owner.settle_orphans()?;
        Ok(owner)
    }

    pub(crate) fn usage_base(&self) -> &str {
        &self.usage_base
    }

    pub(crate) fn observations(&self) -> std::sync::MutexGuard<'_, Observations> {
        self.observations.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Fails once the registration or its generation is gone.
    pub(crate) fn store_view(
        &self,
        registration_id: &str,
        generation: u64,
    ) -> Result<(), AuthError> {
        self.store.view(registration_id, generation).map(|_| ())
    }

    pub fn snapshot(&self) -> Result<crate::accounts::store::AccountSnapshot, AuthError> {
        self.store.snapshot()
    }

    /// 解除と表示名は所有者の保存を通す。実行中のrunは次の送信で止まる。
    pub fn logout(&self, registration_id: &str, generation: u64) -> Result<(), AuthError> {
        self.store.logout(registration_id, generation)
    }

    pub fn set_label(
        &self,
        registration_id: &str,
        generation: u64,
        label: Option<String>,
    ) -> Result<(), AuthError> {
        self.store.set_label(registration_id, generation, label)
    }

    fn settle_orphans(&self) -> Result<(), AuthError> {
        for crate::accounts::store::Unsettled {
            registration_id,
            generation,
            index,
            state,
            sent,
        } in self.store.unsettled()?
        {
            // A held lease means a live owner in some process; leave it.
            let Some(_lease) = self.store.try_slot_lease(index)? else {
                continue;
            };
            let attempt_id = match self.store.pending_attempt(&registration_id, generation) {
                Ok(attempt) => attempt,
                Err(AuthError::NotLoggedIn) => continue,
                Err(error) => return Err(error),
            };
            if state == AccountState::RefreshPending {
                self.store
                    .end_refresh(&registration_id, generation, &attempt_id, true)?;
            } else {
                let reservation = Reservation {
                    registration_id,
                    generation,
                    attempt_id,
                };
                self.store.cancel_registration(&reservation, sent)?;
            }
        }
        Ok(())
    }

    pub async fn token(
        self: &Arc<Self>,
        registration_id: &str,
        generation: u64,
    ) -> Result<AccessToken, AuthError> {
        self.owned(registration_id, generation, None).await
    }

    /// After a 401 for `used_revision`: a newer saved token is returned
    /// without a POST; a revision from the future is refused.
    pub async fn refresh_after_unauthorized(
        self: &Arc<Self>,
        registration_id: &str,
        generation: u64,
        used_revision: u64,
    ) -> Result<AccessToken, AuthError> {
        self.owned(registration_id, generation, Some(used_revision))
            .await
    }

    async fn owned(
        self: &Arc<Self>,
        registration_id: &str,
        generation: u64,
        used: Option<u64>,
    ) -> Result<AccessToken, AuthError> {
        let owner = Arc::clone(self);
        let registration_id = registration_id.to_owned();
        tokio::spawn(async move { owner.refresh(&registration_id, generation, used).await })
            .await
            .map_err(|_| AuthError::RecoveryRequired)?
    }

    fn flight(&self, registration_id: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut flights = self.flights.lock().unwrap_or_else(|e| e.into_inner());
        flights
            .entry(registration_id.to_owned())
            .or_default()
            .clone()
    }

    async fn lease(&self, index: usize) -> Result<SlotLease, AuthError> {
        let deadline = Instant::now() + LEASE_WAIT;
        loop {
            if let Some(lease) = self.store.try_slot_lease(index)? {
                return Ok(lease);
            }
            if Instant::now() >= deadline {
                return Err(AuthError::Busy);
            }
            tokio::time::sleep(POLL).await;
        }
    }

    /// Retries only a busy owner lock; the HTTP result is kept, never re-requested.
    async fn save<T>(
        &self,
        mut commit: impl FnMut() -> Result<T, AuthError>,
    ) -> Result<T, AuthError> {
        let deadline = Instant::now() + SAVE_WAIT;
        loop {
            match commit() {
                Err(AuthError::Busy) if Instant::now() < deadline => tokio::time::sleep(POLL).await,
                Err(AuthError::Busy) => return Err(AuthError::RecoveryRequired),
                other => return other,
            }
        }
    }

    async fn refresh(
        &self,
        registration_id: &str,
        generation: u64,
        used: Option<u64>,
    ) -> Result<AccessToken, AuthError> {
        let view = self.store.view(registration_id, generation)?;
        if let Some(token) = usable(&view, registration_id, generation, used)? {
            return Ok(token);
        }
        let flight = self.flight(registration_id);
        let _flight = flight.lock().await;
        let _lease = self.lease(view.index).await?;
        let view = self.store.view(registration_id, generation)?;
        if let Some(token) = usable(&view, registration_id, generation, used)? {
            return Ok(token);
        }
        match view.state {
            AccountState::Ready => {}
            // We hold the lease, so its owner is gone: the result is unknown.
            AccountState::RefreshPending => {
                let attempt = self.store.pending_attempt(registration_id, generation)?;
                self.store
                    .end_refresh(registration_id, generation, &attempt, true)?;
                return Err(reauth_required());
            }
            _ => return Err(reauth_required()),
        }
        let credentials = view.credentials.ok_or(AuthError::RecoveryRequired)?;
        let attempt = self
            .store
            .begin_refresh(registration_id, generation, view.token_revision)?;
        let result = post_token_bound(
            &self.issuer,
            &[
                ("grant_type", "refresh_token"),
                ("client_id", CLIENT_ID),
                ("refresh_token", &credentials.refresh_token),
            ],
            Some(&credentials.refresh_token),
            IdentityRule::SameAccount(&credentials.account_id),
        )
        .await;
        match result {
            Ok(fresh) => {
                self.save(|| {
                    self.store.complete_refresh(
                        registration_id,
                        generation,
                        &attempt,
                        view.token_revision,
                        fresh.clone(),
                    )
                })
                .await?;
                Ok(token(
                    registration_id,
                    generation,
                    view.token_revision + 1,
                    &fresh,
                ))
            }
            Err(error) => {
                // Only a client that was never built is known not to have sent.
                let sent =
                    !matches!(&error, AuthError::Http(kind) if kind == "client_configuration");
                match self
                    .save(|| {
                        self.store
                            .end_refresh(registration_id, generation, &attempt, sent)
                    })
                    .await
                {
                    // A logout already moved the generation; the result is discarded.
                    Ok(()) | Err(AuthError::NotLoggedIn) => Err(error),
                    Err(saving) => Err(saving),
                }
            }
        }
    }

    /// A new login. `callback` receives the authorize URL and state, opens it
    /// and returns the code; no POST starts before it returns.
    pub async fn register<C, F>(
        self: &Arc<Self>,
        label: Option<String>,
        callback: C,
        cancel: impl Future<Output = ()> + Send,
    ) -> Result<AccessToken, AuthError>
    where
        C: FnOnce(String, String) -> F,
        F: Future<Output = Result<String, AuthError>>,
    {
        let slot = self.store.candidate_slot()?;
        let lease = self.lease(slot).await?;
        let reservation = self.store.begin_registration_in(Some(slot), label)?;
        self.login(reservation, lease, false, callback, cancel)
            .await
    }

    /// Re-authenticates the same account; success moves the generation.
    pub async fn reauthenticate<C, F>(
        self: &Arc<Self>,
        registration_id: &str,
        generation: u64,
        callback: C,
        cancel: impl Future<Output = ()> + Send,
    ) -> Result<AccessToken, AuthError>
    where
        C: FnOnce(String, String) -> F,
        F: Future<Output = Result<String, AuthError>>,
    {
        let view = self.store.view(registration_id, generation)?;
        let lease = self.lease(view.index).await?;
        let reservation = self
            .store
            .begin_reauthentication(registration_id, generation)?;
        self.login(reservation, lease, true, callback, cancel).await
    }

    async fn login<C, F>(
        self: &Arc<Self>,
        reservation: Reservation,
        lease: SlotLease,
        reauthentication: bool,
        callback: C,
        cancel: impl Future<Output = ()> + Send,
    ) -> Result<AccessToken, AuthError>
    where
        C: FnOnce(String, String) -> F,
        F: Future<Output = Result<String, AuthError>>,
    {
        tokio::pin!(cancel);
        let pkce = crate::pkce::generate()?;
        let state = crate::pkce::random_urlsafe(16)?;
        let url = crate::login::authorize_url(&pkce.challenge, &state);
        let code = tokio::select! {
            code = callback(url, state) => code,
            () = &mut cancel => Err(cancelled()),
        };
        let code = match code {
            Ok(code) => code,
            Err(error) => {
                // Before the POST the listener is gone and nothing was sent.
                self.store.cancel_registration(&reservation, false)?;
                return Err(error);
            }
        };
        self.store.mark_exchange_pending(&reservation)?;
        let owner = Arc::clone(self);
        let saved = reservation.clone();
        let mut exchange = tokio::spawn(async move {
            let _lease = lease;
            let result = post_token_bound(
                &owner.issuer,
                &[
                    ("grant_type", "authorization_code"),
                    ("client_id", CLIENT_ID),
                    ("code", &code),
                    ("redirect_uri", REDIRECT_URI),
                    ("code_verifier", &pkce.verifier),
                ],
                None,
                IdentityRule::Required,
            )
            .await;
            match result {
                Ok(credentials) => {
                    let (generation, revision) = owner
                        .save(|| {
                            owner
                                .store
                                .complete_registration(&saved, credentials.clone())
                        })
                        .await?;
                    #[cfg(test)]
                    {
                        let gate = owner.completion_gate.lock().unwrap().clone();
                        if let Some(gate) = gate {
                            let _ = gate.acquire().await;
                        }
                    }
                    Ok(token(
                        &saved.registration_id,
                        generation,
                        revision,
                        &credentials,
                    ))
                }
                Err(error) => {
                    let _ = owner
                        .save(|| owner.store.cancel_registration(&saved, true))
                        .await;
                    Err(error)
                }
            }
        });
        tokio::select! {
            // A cancel ready together with the result still wins.
            biased;
            () = &mut cancel => {
                // Recording the cancel first makes a later completion stale.
                match self.save(|| self.store.cancel_registration(&reservation, true)).await {
                    Ok(()) => {}
                    // The exchange saved first: take back exactly that completion.
                    Err(AuthError::NotLoggedIn) => {
                        if let Ok(Ok(token)) = (&mut exchange).await {
                            match self
                                .save(|| {
                                    self.store.discard_completion(
                                        &token.registration_id,
                                        token.generation,
                                        token.token_revision,
                                        reauthentication,
                                    )
                                })
                                .await
                            {
                                Ok(()) | Err(AuthError::NotLoggedIn) => {}
                                Err(error) => return Err(error),
                            }
                        }
                    }
                    Err(error) => return Err(error),
                }
                Err(cancelled())
            }
            result = &mut exchange => result.map_err(|_| AuthError::RecoveryRequired)?,
        }
    }

    /// Read-only callers: no refresh, no recovery and nothing created.
    pub fn read_only_token(
        registration_id: &str,
        generation: u64,
    ) -> Result<AccessToken, AuthError> {
        read_only_token_at(
            &crate::protection::trusted_home()?,
            registration_id,
            generation,
        )
    }
}

fn read_only_token_at(
    home: &std::path::Path,
    registration_id: &str,
    generation: u64,
) -> Result<AccessToken, AuthError> {
    let view = AccountStore::read_only_view(home, registration_id, generation)?;
    match view.state {
        AccountState::Registering | AccountState::RefreshPending => {
            Err(AuthError::RecoveryRequired)
        }
        AccountState::Ready => {
            usable(&view, registration_id, generation, None)?.ok_or(AuthError::ReadOnly)
        }
        AccountState::ReauthRequired | AccountState::Disconnected => Err(AuthError::ReadOnly),
    }
}

#[cfg(test)]
mod tests;
