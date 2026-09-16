//! Durable account registry: schema, deterministic transitions and exact
//! crash recovery. Network exchange and refresh ownership are M5-2c.
//!
//! Every saved envelope names its operation. A replacement and a recovered
//! tmp are both checked by recomputing that operation from the canonical
//! registry, so a stale or foreign tmp is never published.

use std::collections::BTreeSet;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{
    AuthError, Credentials,
    accounts::directory::{AccountDirectory, RegistryWriter, read_registry_read_only},
};

pub const MAX_SLOTS: usize = 16;
const VERSION: u32 = 1;
const MAX_LABEL_BYTES: usize = 256;

/// Non-secret projection for selection and UI.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountSnapshot {
    pub registry_revision: u64,
    pub accounts: Vec<AccountSummary>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AccountSummary {
    pub registration_id: String,
    pub generation: u64,
    pub state: AccountState,
    pub label: Option<String>,
    /// Stable per registration and never reused, so selection can tell two
    /// registrations apart without a credential.
    pub principal_key: String,
    pub registration_order: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountState {
    Registering,
    Ready,
    RefreshPending,
    ReauthRequired,
    Disconnected,
}

/// A reserved registration or re-authentication, bound to its generation and attempt.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Reservation {
    pub registration_id: String,
    pub generation: u64,
    pub attempt_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AccountRegistry {
    version: u32,
    previous_revision: u64,
    revision: u64,
    protection_revision: u64,
    next_registration_order: u64,
    slots: Vec<Slot>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Slot {
    registration_id: String,
    generation: u64,
    principal_key: String,
    registration_order: u64,
    token_revision: u64,
    label: Option<String>,
    /// The account this registration is bound to, kept when credentials are
    /// removed so re-authentication and duplicate checks still see it.
    upstream_account_id: Option<String>,
    state: SlotState,
}

/// Credential values never appear in Debug output.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct StoredCredentials {
    access_token: String,
    refresh_token: String,
    upstream_account_id: String,
    expires_at: Option<u64>,
}

impl std::fmt::Debug for StoredCredentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StoredCredentials(<redacted>)")
    }
}

impl StoredCredentials {
    pub(crate) fn to_credentials(&self) -> Credentials {
        Credentials {
            access_token: self.access_token.clone(),
            refresh_token: self.refresh_token.clone(),
            account_id: self.upstream_account_id.clone(),
            expires_at: self.expires_at,
        }
    }
}

impl From<Credentials> for StoredCredentials {
    fn from(value: Credentials) -> Self {
        Self {
            access_token: value.access_token,
            refresh_token: value.refresh_token,
            upstream_account_id: value.account_id,
            expires_at: value.expires_at,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum SlotState {
    Registering {
        attempt_id: String,
        phase: RegistrationPhase,
        reauthentication: bool,
        /// Restored when a re-authentication is cancelled before its POST.
        previous: Option<Box<SlotState>>,
    },
    Ready {
        credentials: StoredCredentials,
    },
    RefreshPending {
        attempt_id: String,
        token_revision: u64,
        credentials: StoredCredentials,
    },
    ReauthRequired,
    Disconnected,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum RegistrationPhase {
    AwaitingCallback,
    ExchangePending,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum Operation {
    Initialize,
    Register {
        slot: usize,
        registration_id: String,
        principal_key: String,
        attempt_id: String,
        label: Option<String>,
    },
    ExchangePending {
        registration_id: String,
        generation: u64,
        attempt_id: String,
    },
    /// The new credentials are read from the target slot of the saved registry.
    CompleteRegistration {
        registration_id: String,
        generation: u64,
        attempt_id: String,
    },
    CancelRegistration {
        registration_id: String,
        generation: u64,
        attempt_id: String,
        after_post: bool,
    },
    BeginReauthentication {
        registration_id: String,
        generation: u64,
        attempt_id: String,
    },
    SetLabel {
        registration_id: String,
        generation: u64,
        label: Option<String>,
    },
    Logout {
        registration_id: String,
        generation: u64,
    },
    /// Durable before the refresh POST; the old credentials stay for recovery only.
    BeginRefresh {
        registration_id: String,
        generation: u64,
        attempt_id: String,
        token_revision: u64,
    },
    /// The new credentials are read from the target slot of the saved registry.
    CompleteRefresh {
        registration_id: String,
        generation: u64,
        attempt_id: String,
        token_revision: u64,
    },
    /// Only a live owner that knows the POST never started may release.
    ReleaseRefresh {
        registration_id: String,
        generation: u64,
        attempt_id: String,
    },
    /// A login whose caller cancelled after its exchange had already saved:
    /// a new registration disconnects, a re-authentication needs another login.
    DiscardCompletion {
        registration_id: String,
        generation: u64,
        token_revision: u64,
        reauthentication: bool,
    },
    /// A sent, failed or unknown refresh: the account needs a new login.
    FailRefresh {
        registration_id: String,
        generation: u64,
        attempt_id: String,
    },
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Envelope {
    operation: Operation,
    registry: AccountRegistry,
}

impl SlotState {
    fn public(&self) -> AccountState {
        match self {
            Self::Registering { .. } => AccountState::Registering,
            Self::Ready { .. } => AccountState::Ready,
            Self::RefreshPending { .. } => AccountState::RefreshPending,
            Self::ReauthRequired => AccountState::ReauthRequired,
            Self::Disconnected => AccountState::Disconnected,
        }
    }

    fn credentials(&self) -> Option<&StoredCredentials> {
        match self {
            Self::Ready { credentials } | Self::RefreshPending { credentials, .. } => {
                Some(credentials)
            }
            _ => None,
        }
    }
}

fn valid_id(value: &str) -> bool {
    (22..=128).contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn valid_label(value: Option<&str>) -> bool {
    value.is_none_or(|v| v.len() <= MAX_LABEL_BYTES && !v.chars().any(char::is_control))
}

fn next_u64(value: u64) -> Result<u64, AuthError> {
    value.checked_add(1).ok_or(AuthError::AccountLimit)
}

impl AccountRegistry {
    fn initial(protection_revision: u64) -> Self {
        Self {
            version: VERSION,
            previous_revision: 0,
            revision: 1,
            protection_revision,
            next_registration_order: 0,
            slots: Vec::new(),
        }
    }

    fn snapshot(&self) -> AccountSnapshot {
        AccountSnapshot {
            registry_revision: self.revision,
            accounts: self
                .slots
                .iter()
                .map(|slot| AccountSummary {
                    registration_id: slot.registration_id.clone(),
                    generation: slot.generation,
                    state: slot.state.public(),
                    label: slot.label.clone(),
                    principal_key: slot.principal_key.clone(),
                    registration_order: slot.registration_order,
                })
                .collect(),
        }
    }

    fn validate(&self) -> Result<(), AuthError> {
        let bad = || AuthError::RecoveryRequired;
        if self.version != VERSION
            || self.revision == 0
            || self.protection_revision == 0
            || self.slots.len() > MAX_SLOTS
            || (self.previous_revision == 0
                && (self.revision != 1
                    || !self.slots.is_empty()
                    || self.next_registration_order != 0))
            || (self.previous_revision != 0
                && self.previous_revision.checked_add(1) != Some(self.revision))
        {
            return Err(bad());
        }
        let (mut ids, mut principals, mut orders, mut upstream) = (
            BTreeSet::new(),
            BTreeSet::new(),
            BTreeSet::new(),
            BTreeSet::new(),
        );
        for slot in &self.slots {
            if !valid_id(&slot.registration_id)
                || !valid_id(&slot.principal_key)
                || slot.generation == 0
                || slot.token_revision == 0
                || slot.registration_order >= self.next_registration_order
                || !ids.insert(&slot.registration_id)
                || !principals.insert(&slot.principal_key)
                || !orders.insert(slot.registration_order)
                || !valid_label(slot.label.as_deref())
                || slot
                    .upstream_account_id
                    .as_deref()
                    .is_some_and(str::is_empty)
                || (slot.state != SlotState::Disconnected
                    && slot
                        .upstream_account_id
                        .as_ref()
                        .is_some_and(|id| !upstream.insert(id)))
            {
                return Err(bad());
            }
            match &slot.state {
                SlotState::Registering {
                    attempt_id,
                    previous,
                    reauthentication,
                    ..
                } => {
                    if !valid_id(attempt_id)
                        || previous.is_some() != *reauthentication
                        || previous.as_deref().is_some_and(|p| {
                            matches!(
                                p,
                                SlotState::Registering { .. } | SlotState::RefreshPending { .. }
                            )
                        })
                    {
                        return Err(bad());
                    }
                }
                SlotState::RefreshPending {
                    attempt_id,
                    token_revision,
                    ..
                } if !valid_id(attempt_id) || *token_revision != slot.token_revision => {
                    return Err(bad());
                }
                _ => {}
            }
            if let Some(credentials) = slot.state.credentials()
                && (credentials.access_token.is_empty()
                    || credentials.refresh_token.is_empty()
                    || slot.upstream_account_id.as_ref() != Some(&credentials.upstream_account_id))
            {
                return Err(bad());
            }
        }
        Ok(())
    }

    fn slot(&self, registration_id: &str, generation: u64) -> Result<usize, AuthError> {
        self.slots
            .iter()
            .position(|s| s.registration_id == registration_id && s.generation == generation)
            .ok_or(AuthError::NotLoggedIn)
    }

    /// The next registry for `operation`; `credentials` is only for completions.
    fn apply(
        &self,
        operation: &Operation,
        credentials: Option<StoredCredentials>,
    ) -> Result<Self, AuthError> {
        let stale = || AuthError::NotLoggedIn;
        let mut next = self.clone();
        next.previous_revision = self.revision;
        next.revision = next_u64(self.revision)?;
        match operation {
            Operation::Initialize => return Err(AuthError::RecoveryRequired),
            Operation::Register {
                slot,
                registration_id,
                principal_key,
                attempt_id,
                label,
            } => {
                if ![registration_id, principal_key, attempt_id]
                    .into_iter()
                    .all(|v| valid_id(v))
                    || !valid_label(label.as_deref())
                    || self.slots.iter().any(|s| {
                        &s.registration_id == registration_id || &s.principal_key == principal_key
                    })
                {
                    return Err(AuthError::RecoveryRequired);
                }
                let order = self.next_registration_order;
                next.next_registration_order = next_u64(order)?;
                let state = SlotState::Registering {
                    attempt_id: attempt_id.clone(),
                    phase: RegistrationPhase::AwaitingCallback,
                    reauthentication: false,
                    previous: None,
                };
                if *slot == self.slots.len() {
                    if self.slots.len() >= MAX_SLOTS {
                        return Err(AuthError::AccountLimit);
                    }
                    next.slots.push(Slot {
                        registration_id: registration_id.clone(),
                        generation: 1,
                        principal_key: principal_key.clone(),
                        registration_order: order,
                        token_revision: 1,
                        label: label.clone(),
                        upstream_account_id: None,
                        state,
                    });
                } else {
                    // Only a disconnected slot is reused, with fresh identities.
                    let old = self.slots.get(*slot).ok_or(AuthError::AccountLimit)?;
                    if old.state != SlotState::Disconnected {
                        return Err(AuthError::AccountLimit);
                    }
                    next.slots[*slot] = Slot {
                        registration_id: registration_id.clone(),
                        generation: next_u64(old.generation)?,
                        principal_key: principal_key.clone(),
                        registration_order: order,
                        token_revision: next_u64(old.token_revision)?,
                        label: label.clone(),
                        upstream_account_id: None,
                        state,
                    };
                }
            }
            Operation::ExchangePending {
                registration_id,
                generation,
                attempt_id,
            } => {
                let index = self.slot(registration_id, *generation)?;
                match &mut next.slots[index].state {
                    SlotState::Registering {
                        attempt_id: current,
                        phase,
                        ..
                    } if current == attempt_id && *phase == RegistrationPhase::AwaitingCallback => {
                        *phase = RegistrationPhase::ExchangePending;
                    }
                    _ => return Err(stale()),
                }
            }
            Operation::CompleteRegistration {
                registration_id,
                generation,
                attempt_id,
            } => {
                let index = self.slot(registration_id, *generation)?;
                let credentials = credentials.ok_or(AuthError::RecoveryRequired)?;
                let SlotState::Registering {
                    attempt_id: current,
                    phase: RegistrationPhase::ExchangePending,
                    reauthentication,
                    ..
                } = &self.slots[index].state
                else {
                    return Err(stale());
                };
                if current != attempt_id {
                    return Err(stale());
                }
                let account = &credentials.upstream_account_id;
                if self.slots.iter().enumerate().any(|(i, s)| {
                    i != index
                        && s.state != SlotState::Disconnected
                        && s.upstream_account_id.as_ref() == Some(account)
                }) {
                    return Err(AuthError::AlreadyRegistered);
                }
                // Re-authentication keeps the bound account whatever the prior
                // state; success invalidates old bindings.
                if *reauthentication {
                    if self.slots[index].upstream_account_id.as_ref() != Some(account) {
                        return Err(AuthError::Denied("different_account".into()));
                    }
                    next.slots[index].generation = next_u64(*generation)?;
                }
                next.slots[index].upstream_account_id = Some(account.clone());
                next.slots[index].token_revision = next_u64(self.slots[index].token_revision)?;
                next.slots[index].state = SlotState::Ready { credentials };
            }
            Operation::CancelRegistration {
                registration_id,
                generation,
                attempt_id,
                after_post,
            } => {
                let index = self.slot(registration_id, *generation)?;
                let SlotState::Registering {
                    attempt_id: current,
                    phase,
                    reauthentication,
                    previous,
                } = &self.slots[index].state
                else {
                    return Err(stale());
                };
                if current != attempt_id
                    || (*phase == RegistrationPhase::ExchangePending) != *after_post
                {
                    return Err(stale());
                }
                let slot = &mut next.slots[index];
                slot.state = match (reauthentication, after_post) {
                    (false, _) => SlotState::Disconnected,
                    (true, false) => *previous.clone().ok_or(AuthError::RecoveryRequired)?,
                    (true, true) => SlotState::ReauthRequired,
                };
                if !reauthentication || *after_post {
                    slot.generation = next_u64(*generation)?;
                }
            }
            Operation::BeginReauthentication {
                registration_id,
                generation,
                attempt_id,
            } => {
                let index = self.slot(registration_id, *generation)?;
                let previous = &self.slots[index].state;
                if !valid_id(attempt_id)
                    || !matches!(
                        previous,
                        SlotState::Ready { .. } | SlotState::ReauthRequired
                    )
                {
                    return Err(stale());
                }
                next.slots[index].state = SlotState::Registering {
                    attempt_id: attempt_id.clone(),
                    phase: RegistrationPhase::AwaitingCallback,
                    reauthentication: true,
                    previous: Some(Box::new(previous.clone())),
                };
            }
            Operation::SetLabel {
                registration_id,
                generation,
                label,
            } => {
                let index = self.slot(registration_id, *generation)?;
                if !valid_label(label.as_deref()) {
                    return Err(AuthError::Denied("invalid_label".into()));
                }
                next.slots[index].label = label.clone();
            }
            Operation::Logout {
                registration_id,
                generation,
            } => {
                let index = self.slot(registration_id, *generation)?;
                let slot = &mut next.slots[index];
                slot.generation = next_u64(*generation)?;
                slot.state = SlotState::Disconnected;
            }
            Operation::BeginRefresh {
                registration_id,
                generation,
                attempt_id,
                token_revision,
            } => {
                let index = self.slot(registration_id, *generation)?;
                let slot = &self.slots[index];
                let SlotState::Ready { credentials } = &slot.state else {
                    return Err(stale());
                };
                if !valid_id(attempt_id) || slot.token_revision != *token_revision {
                    return Err(stale());
                }
                next.slots[index].state = SlotState::RefreshPending {
                    attempt_id: attempt_id.clone(),
                    token_revision: *token_revision,
                    credentials: credentials.clone(),
                };
            }
            Operation::CompleteRefresh {
                registration_id,
                generation,
                attempt_id,
                token_revision,
            } => {
                let index = self.slot(registration_id, *generation)?;
                let credentials = credentials.ok_or(AuthError::RecoveryRequired)?;
                let slot = &self.slots[index];
                match &slot.state {
                    SlotState::RefreshPending {
                        attempt_id: current,
                        token_revision: pending,
                        ..
                    } if current == attempt_id && pending == token_revision => {}
                    _ => return Err(stale()),
                }
                if slot.upstream_account_id.as_ref() != Some(&credentials.upstream_account_id) {
                    return Err(AuthError::Denied("different_account".into()));
                }
                next.slots[index].token_revision = next_u64(slot.token_revision)?;
                next.slots[index].state = SlotState::Ready { credentials };
            }
            Operation::DiscardCompletion {
                registration_id,
                generation,
                token_revision,
                reauthentication,
            } => {
                let index = self.slot(registration_id, *generation)?;
                let slot = &mut next.slots[index];
                if !matches!(slot.state, SlotState::Ready { .. })
                    || slot.token_revision != *token_revision
                {
                    return Err(stale());
                }
                slot.generation = next_u64(*generation)?;
                slot.state = if *reauthentication {
                    SlotState::ReauthRequired
                } else {
                    SlotState::Disconnected
                };
            }
            Operation::ReleaseRefresh {
                registration_id,
                generation,
                attempt_id,
            }
            | Operation::FailRefresh {
                registration_id,
                generation,
                attempt_id,
            } => {
                let index = self.slot(registration_id, *generation)?;
                let SlotState::RefreshPending {
                    attempt_id: current,
                    credentials,
                    ..
                } = &self.slots[index].state
                else {
                    return Err(stale());
                };
                if current != attempt_id {
                    return Err(stale());
                }
                next.slots[index].state = if matches!(operation, Operation::ReleaseRefresh { .. }) {
                    SlotState::Ready {
                        credentials: credentials.clone(),
                    }
                } else {
                    SlotState::ReauthRequired
                };
            }
        }
        next.validate()?;
        Ok(next)
    }

    /// The credentials a completion saved, read back only to recompute it.
    fn completed_credentials(&self, operation: &Operation) -> Option<StoredCredentials> {
        match operation {
            Operation::CompleteRegistration {
                registration_id, ..
            }
            | Operation::CompleteRefresh {
                registration_id, ..
            } => self
                .slots
                .iter()
                .find(|s| &s.registration_id == registration_id)
                .and_then(|s| s.state.credentials().cloned()),
            _ => None,
        }
    }
}

fn encode(envelope: &Envelope) -> Result<Vec<u8>, AuthError> {
    envelope.registry.validate()?;
    serde_json::to_vec(envelope).map_err(|_| AuthError::RecoveryRequired)
}

fn decode(body: &[u8]) -> Result<Envelope, AuthError> {
    let envelope: Envelope =
        serde_json::from_slice(body).map_err(|_| AuthError::RecoveryRequired)?;
    envelope.registry.validate()?;
    Ok(envelope)
}

/// Accepts the tmp only as the exact next step of canonical, journaled at or
/// below the current journal revision. Nothing is deleted on refusal.
fn recover(writer: &mut RegistryWriter<'_>) -> Result<(), AuthError> {
    let Some(temporary) = writer.temporary() else {
        return Ok(());
    };
    let temporary = decode(temporary)?;
    let journal = writer.protection_revision();
    let expected = match writer.canonical() {
        None => {
            if temporary.operation != Operation::Initialize {
                return Err(AuthError::RecoveryRequired);
            }
            AccountRegistry::initial(temporary.registry.protection_revision)
        }
        Some(canonical) => {
            let canonical = decode(canonical)?.registry;
            if temporary.registry.protection_revision < canonical.protection_revision {
                return Err(AuthError::RecoveryRequired);
            }
            let credentials = temporary
                .registry
                .completed_credentials(&temporary.operation);
            let mut expected = canonical
                .apply(&temporary.operation, credentials)
                .map_err(|_| AuthError::RecoveryRequired)?;
            expected.protection_revision = temporary.registry.protection_revision;
            expected
        }
    };
    if expected != temporary.registry || temporary.registry.protection_revision > journal {
        return Err(AuthError::RecoveryRequired);
    }
    writer.publish_temporary()
}

/// A registration or refresh an owner left in progress; `sent` is true when
/// its POST may have started.
pub(crate) struct Unsettled {
    pub(crate) registration_id: String,
    pub(crate) generation: u64,
    pub(crate) index: usize,
    pub(crate) state: AccountState,
    pub(crate) sent: bool,
}

/// One registration as the refresh owner sees it under the owner lock.
pub(crate) struct SlotView {
    pub(crate) index: usize,
    pub(crate) token_revision: u64,
    pub(crate) state: AccountState,
    pub(crate) credentials: Option<Credentials>,
}

fn slot_view(
    registry: &AccountRegistry,
    registration_id: &str,
    generation: u64,
) -> Result<SlotView, AuthError> {
    let index = registry.slot(registration_id, generation)?;
    let slot = &registry.slots[index];
    Ok(SlotView {
        index,
        token_revision: slot.token_revision,
        state: slot.state.public(),
        credentials: slot
            .state
            .credentials()
            .map(StoredCredentials::to_credentials),
    })
}

/// The mutable owner of the fixed `~/.polaris/accounts` registry.
pub struct AccountStore {
    directory: AccountDirectory,
}

impl AccountStore {
    /// Refused under `POLARIS_AUTH_READ_ONLY`; read-only callers use
    /// [`AccountStore::read_only_snapshot`], which creates nothing.
    pub fn open() -> Result<Self, AuthError> {
        if crate::current_refresh_policy()? == crate::RefreshPolicy::ReadOnly {
            return Err(AuthError::ReadOnly);
        }
        Ok(Self {
            directory: AccountDirectory::open()?,
        })
    }

    #[cfg(test)]
    pub(crate) fn open_at(home: &Path) -> Result<Self, AuthError> {
        Ok(Self {
            directory: AccountDirectory::open_for_test(home)?,
        })
    }

    /// No directory, lock or journal is created and nothing is recovered.
    pub fn read_only_snapshot() -> Result<AccountSnapshot, AuthError> {
        Self::read_only_at(&crate::protection::trusted_home()?)
    }

    fn read_only_at(home: &Path) -> Result<AccountSnapshot, AuthError> {
        let body = read_registry_read_only(home)?.ok_or(AuthError::ReadOnly)?;
        Ok(decode(&body)?.registry.snapshot())
    }

    pub fn snapshot(&self) -> Result<AccountSnapshot, AuthError> {
        self.transact(|_| Ok(None))
            .map(|registry| registry.snapshot())
    }

    /// Reserves `slot` (append, or reuse a disconnected slot) for a new login.
    pub fn begin_registration(&self, label: Option<String>) -> Result<Reservation, AuthError> {
        self.begin_registration_in(None, label)
    }

    /// The slot a new registration would take now; its lease is taken first
    /// and the reservation rechecks the same slot under the owner lock.
    pub(crate) fn candidate_slot(&self) -> Result<usize, AuthError> {
        let registry = self.transact(|_| Ok(None))?;
        Ok(registry
            .slots
            .iter()
            .position(|s| s.state == SlotState::Disconnected)
            .unwrap_or(registry.slots.len()))
    }

    pub(crate) fn try_slot_lease(
        &self,
        index: usize,
    ) -> Result<Option<crate::accounts::directory::SlotLease>, AuthError> {
        self.directory.try_slot_lease(index)
    }

    pub(crate) fn begin_registration_in(
        &self,
        slot: Option<usize>,
        label: Option<String>,
    ) -> Result<Reservation, AuthError> {
        if !valid_label(label.as_deref()) {
            return Err(AuthError::Denied("invalid_label".into()));
        }
        let registration_id = crate::pkce::random_urlsafe(18)?;
        let principal_key = crate::pkce::random_urlsafe(18)?;
        let attempt_id = crate::pkce::random_urlsafe(18)?;
        let registry = self.transact(|current| {
            let slot = slot.unwrap_or_else(|| {
                current
                    .slots
                    .iter()
                    .position(|s| s.state == SlotState::Disconnected)
                    .unwrap_or(current.slots.len())
            });
            Ok(Some((
                Operation::Register {
                    slot,
                    registration_id: registration_id.clone(),
                    principal_key: principal_key.clone(),
                    attempt_id: attempt_id.clone(),
                    label: label.clone(),
                },
                None,
            )))
        })?;
        let generation = registry
            .slots
            .iter()
            .find(|s| s.registration_id == registration_id)
            .map(|s| s.generation);
        Ok(Reservation {
            registration_id,
            generation: generation.ok_or(AuthError::RecoveryRequired)?,
            attempt_id,
        })
    }

    pub fn mark_exchange_pending(&self, reservation: &Reservation) -> Result<(), AuthError> {
        self.commit(
            Operation::ExchangePending {
                registration_id: reservation.registration_id.clone(),
                generation: reservation.generation,
                attempt_id: reservation.attempt_id.clone(),
            },
            None,
        )
    }

    /// Returns the saved generation and token revision; a re-authentication
    /// moves the generation by one.
    pub fn complete_registration(
        &self,
        reservation: &Reservation,
        credentials: Credentials,
    ) -> Result<(u64, u64), AuthError> {
        if credentials.account_id.is_empty() {
            return Err(AuthError::Denied("missing_account".into()));
        }
        let operation = Operation::CompleteRegistration {
            registration_id: reservation.registration_id.clone(),
            generation: reservation.generation,
            attempt_id: reservation.attempt_id.clone(),
        };
        let registry = self.transact(|_| Ok(Some((operation, Some(credentials.into())))))?;
        registry
            .slots
            .iter()
            .find(|s| s.registration_id == reservation.registration_id)
            .map(|s| (s.generation, s.token_revision))
            .ok_or(AuthError::RecoveryRequired)
    }

    pub fn cancel_registration(
        &self,
        reservation: &Reservation,
        after_post: bool,
    ) -> Result<(), AuthError> {
        self.commit(
            Operation::CancelRegistration {
                registration_id: reservation.registration_id.clone(),
                generation: reservation.generation,
                attempt_id: reservation.attempt_id.clone(),
                after_post,
            },
            None,
        )
    }

    pub fn begin_reauthentication(
        &self,
        registration_id: &str,
        generation: u64,
    ) -> Result<Reservation, AuthError> {
        let attempt_id = crate::pkce::random_urlsafe(18)?;
        self.commit(
            Operation::BeginReauthentication {
                registration_id: registration_id.into(),
                generation,
                attempt_id: attempt_id.clone(),
            },
            None,
        )?;
        Ok(Reservation {
            registration_id: registration_id.into(),
            generation,
            attempt_id,
        })
    }

    pub fn set_label(
        &self,
        registration_id: &str,
        generation: u64,
        label: Option<String>,
    ) -> Result<(), AuthError> {
        self.commit(
            Operation::SetLabel {
                registration_id: registration_id.into(),
                generation,
                label,
            },
            None,
        )
    }

    /// Never waits for a refresh; later results for the old generation are refused.
    pub fn logout(&self, registration_id: &str, generation: u64) -> Result<(), AuthError> {
        self.commit(
            Operation::Logout {
                registration_id: registration_id.into(),
                generation,
            },
            None,
        )
    }

    pub(crate) fn view(
        &self,
        registration_id: &str,
        generation: u64,
    ) -> Result<SlotView, AuthError> {
        slot_view(&self.transact(|_| Ok(None))?, registration_id, generation)
    }

    /// Read-only callers: nothing is created or recovered.
    pub(crate) fn read_only_view(
        home: &Path,
        registration_id: &str,
        generation: u64,
    ) -> Result<SlotView, AuthError> {
        let body = read_registry_read_only(home)?.ok_or(AuthError::ReadOnly)?;
        slot_view(&decode(&body)?.registry, registration_id, generation)
    }

    /// Every registration or refresh that some owner left in progress.
    pub(crate) fn unsettled(&self) -> Result<Vec<Unsettled>, AuthError> {
        let registry = self.transact(|_| Ok(None))?;
        Ok(registry
            .slots
            .iter()
            .enumerate()
            .filter_map(|(index, slot)| {
                let sent = match &slot.state {
                    SlotState::Registering { phase, .. } => {
                        *phase == RegistrationPhase::ExchangePending
                    }
                    SlotState::RefreshPending { .. } => true,
                    _ => return None,
                };
                Some(Unsettled {
                    registration_id: slot.registration_id.clone(),
                    generation: slot.generation,
                    index,
                    state: slot.state.public(),
                    sent,
                })
            })
            .collect())
    }

    /// The attempt IDs of a pending registration or refresh, for recovery only.
    pub(crate) fn pending_attempt(
        &self,
        registration_id: &str,
        generation: u64,
    ) -> Result<String, AuthError> {
        let registry = self.transact(|_| Ok(None))?;
        let index = registry.slot(registration_id, generation)?;
        match &registry.slots[index].state {
            SlotState::Registering { attempt_id, .. }
            | SlotState::RefreshPending { attempt_id, .. } => Ok(attempt_id.clone()),
            _ => Err(AuthError::NotLoggedIn),
        }
    }

    pub(crate) fn begin_refresh(
        &self,
        registration_id: &str,
        generation: u64,
        token_revision: u64,
    ) -> Result<String, AuthError> {
        let attempt_id = crate::pkce::random_urlsafe(18)?;
        self.commit(
            Operation::BeginRefresh {
                registration_id: registration_id.into(),
                generation,
                attempt_id: attempt_id.clone(),
                token_revision,
            },
            None,
        )?;
        Ok(attempt_id)
    }

    pub(crate) fn complete_refresh(
        &self,
        registration_id: &str,
        generation: u64,
        attempt_id: &str,
        token_revision: u64,
        credentials: Credentials,
    ) -> Result<(), AuthError> {
        self.commit(
            Operation::CompleteRefresh {
                registration_id: registration_id.into(),
                generation,
                attempt_id: attempt_id.into(),
                token_revision,
            },
            Some(credentials.into()),
        )
    }

    /// Takes back one saved login result, bound to its generation and token revision.
    pub(crate) fn discard_completion(
        &self,
        registration_id: &str,
        generation: u64,
        token_revision: u64,
        reauthentication: bool,
    ) -> Result<(), AuthError> {
        self.commit(
            Operation::DiscardCompletion {
                registration_id: registration_id.into(),
                generation,
                token_revision,
                reauthentication,
            },
            None,
        )
    }

    pub(crate) fn end_refresh(
        &self,
        registration_id: &str,
        generation: u64,
        attempt_id: &str,
        sent: bool,
    ) -> Result<(), AuthError> {
        let (registration_id, attempt_id) = (registration_id.into(), attempt_id.into());
        self.commit(
            if sent {
                Operation::FailRefresh {
                    registration_id,
                    generation,
                    attempt_id,
                }
            } else {
                Operation::ReleaseRefresh {
                    registration_id,
                    generation,
                    attempt_id,
                }
            },
            None,
        )
    }

    fn commit(
        &self,
        operation: Operation,
        credentials: Option<StoredCredentials>,
    ) -> Result<(), AuthError> {
        self.transact(|_| Ok(Some((operation, credentials))))
            .map(|_| ())
    }

    /// One owner-lock section: genesis or exact recovery, then the operation.
    fn transact(
        &self,
        choose: impl FnOnce(
            &AccountRegistry,
        )
            -> Result<Option<(Operation, Option<StoredCredentials>)>, AuthError>,
    ) -> Result<AccountRegistry, AuthError> {
        let mut writer = self.directory.registry_writer()?;
        recover(&mut writer)?;
        if writer.canonical().is_none() {
            writer.replace(|protection| {
                encode(&Envelope {
                    operation: Operation::Initialize,
                    registry: AccountRegistry::initial(protection),
                })
            })?;
        }
        let current = decode(writer.canonical().ok_or(AuthError::RecoveryRequired)?)?.registry;
        let Some((operation, credentials)) = choose(&current)? else {
            return Ok(current);
        };
        let mut next = current.apply(&operation, credentials)?;
        let mut saved = None;
        writer.replace(|protection| {
            next.protection_revision = protection;
            saved = Some(next.clone());
            encode(&Envelope {
                operation,
                registry: next,
            })
        })?;
        saved.ok_or(AuthError::RecoveryRequired)
    }
}

#[cfg(test)]
mod tests;
