//! Pure quota validation and account selection.
//!
//! Callers provide a normalized snapshot and fixed clocks.  This module does
//! not obtain credentials, read a clock, or perform any I/O.

pub mod usage;

use std::collections::BTreeSet;

/// A clock value supplied by the caller for one process epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EvaluationTime {
    pub process_epoch: u64,
    pub monotonic_millis: u64,
    pub unix_seconds: u64,
}

/// The registered accounts available to a selection scope.
#[derive(Debug, Clone, PartialEq)]
pub struct RegistrySnapshot {
    pub revision: u64,
    pub accounts: Vec<RegisteredAccount>,
}

/// Identity and permission data for one host-side registration.
#[derive(Debug, Clone, PartialEq)]
pub struct RegisteredAccount {
    pub registration_id: String,
    pub generation: u64,
    pub registration_order: u64,
    pub principal_key: String,
    pub automatic_use_allowed: bool,
    pub authentication_usable: bool,
    pub observation: QuotaObservation,
}

/// A normalized observation, bound to a registration, model, and clocks.
#[derive(Debug, Clone, PartialEq)]
pub struct QuotaObservation {
    pub registration_id: String,
    pub generation: u64,
    pub model_id: String,
    pub process_epoch: u64,
    pub captured_monotonic_millis: u64,
    pub captured_unix_seconds: u64,
    pub state: ObservationState,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ObservationState {
    Known(KnownQuota),
    Missing,
    FetchFailed,
    MappingUnknown,
    Unsupported,
}

/// The complete host-side profile coverage for a known observation.
#[derive(Debug, Clone, PartialEq)]
pub struct KnownQuota {
    pub profile_id: String,
    pub expected_windows: Vec<WindowKey>,
    pub windows: Vec<QuotaWindow>,
    pub bucket_states: Vec<BucketState>,
}

/// Untrusted adapter input for a quota key.  `None` represents a required
/// upstream field that was absent; adapters must not replace it with a
/// convenient default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawWindowKey {
    pub bucket_key: Option<String>,
    pub window_slot_key: Option<String>,
}

/// Untrusted adapter input for one window. Signed values deliberately keep
/// conversion failures representable until this pure normalization boundary.
#[derive(Debug, Clone, PartialEq)]
pub struct RawQuotaWindow {
    pub key: RawWindowKey,
    pub usage_percent: Option<f64>,
    pub window_millis: Option<i128>,
    pub reset_unix_seconds: Option<i128>,
}

/// A stable host key for one bucket/window pair.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct WindowKey {
    pub bucket_key: String,
    pub window_slot_key: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QuotaWindow {
    pub key: WindowKey,
    pub usage_percent: f64,
    pub window_millis: u64,
    pub reset_unix_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BucketState {
    pub bucket_key: String,
    pub availability: ServerAvailability,
    pub reached: ReachedState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerAvailability {
    Allowed,
    Denied,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReachedState {
    NotReached,
    Reached,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExclusionReason {
    ScopeNotAllowed,
    AuthenticationUnavailable,
    Missing,
    FetchFailed,
    MappingUnknown,
    Unsupported,
    BindingMismatch,
    Stale,
    ClockInvalid,
    InvalidObservation,
    ResetDue,
    ServerDenied,
    UnknownLimit,
    Exhausted,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exclusion {
    pub registration_id: String,
    pub reason: ExclusionReason,
}

/// The deterministic proposal returned for an eligible account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub registration_id: String,
    pub generation: u64,
    pub registry_revision: u64,
    pub model_id: String,
    pub profile_id: String,
    pub observation_process_epoch: u64,
    pub observation_monotonic_millis: u64,
    pub evaluated_at: EvaluationTime,
    pub earliest_reset_unix_seconds: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionOutcome {
    Selected {
        selection: Selection,
        exclusions: Vec<Exclusion>,
    },
    Unavailable {
        reason: UnavailableReason,
        exclusions: Vec<Exclusion>,
    },
    InvalidInventory,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnavailableReason {
    NoAccounts,
    NoEligibleAccounts,
}

const MAX_OBSERVATION_AGE_MILLIS: u64 = 300_000;

/// Converts adapter-shaped input without reading external state.
///
/// Missing fields, negative values, and values outside `u64` are rejected
/// instead of being saturated or rounded into an apparently usable limit.
pub fn normalize_known_quota(
    profile_id: Option<String>,
    expected_windows: Vec<RawWindowKey>,
    windows: Vec<RawQuotaWindow>,
    bucket_states: Vec<BucketState>,
) -> Result<KnownQuota, ExclusionReason> {
    let profile_id = required(profile_id)?;
    let expected_windows = expected_windows
        .into_iter()
        .map(normalize_key)
        .collect::<Result<Vec<_>, _>>()?;
    let windows = windows
        .into_iter()
        .map(|window| {
            Ok(QuotaWindow {
                key: normalize_key(window.key)?,
                usage_percent: window
                    .usage_percent
                    .ok_or(ExclusionReason::InvalidObservation)?,
                window_millis: checked_u64(window.window_millis)?,
                reset_unix_seconds: checked_u64(window.reset_unix_seconds)?,
            })
        })
        .collect::<Result<Vec<_>, ExclusionReason>>()?;
    let known = KnownQuota {
        profile_id,
        expected_windows,
        windows,
        bucket_states,
    };
    validate_known(&known)?;
    Ok(known)
}

fn required(value: Option<String>) -> Result<String, ExclusionReason> {
    match value {
        Some(value) if !value.is_empty() => Ok(value),
        _ => Err(ExclusionReason::InvalidObservation),
    }
}

fn normalize_key(key: RawWindowKey) -> Result<WindowKey, ExclusionReason> {
    Ok(WindowKey {
        bucket_key: required(key.bucket_key)?,
        window_slot_key: required(key.window_slot_key)?,
    })
}

fn checked_u64(value: Option<i128>) -> Result<u64, ExclusionReason> {
    value
        .ok_or(ExclusionReason::InvalidObservation)
        .and_then(|value| u64::try_from(value).map_err(|_| ExclusionReason::InvalidObservation))
}

/// Validates all applicable quota buckets and proposes the best registration.
///
/// `model_id` is an exact host-side model identifier; no partial matching is
/// performed.  The supplied input is only borrowed and is never normalized in
/// place.
pub fn select_account(
    snapshot: &RegistrySnapshot,
    model_id: &str,
    now: EvaluationTime,
) -> SelectionOutcome {
    if !valid_inventory(snapshot) || model_id.is_empty() || now.unix_seconds == 0 {
        return SelectionOutcome::InvalidInventory;
    }

    let mut exclusions = Vec::new();
    let mut eligible = Vec::new();
    for account in &snapshot.accounts {
        match assess(account, model_id, now) {
            Ok((profile_id, earliest_reset)) => {
                eligible.push((account, profile_id, earliest_reset))
            }
            Err(reason) => exclusions.push(Exclusion {
                registration_id: account.registration_id.clone(),
                reason,
            }),
        }
    }
    exclusions.sort_by(|left, right| {
        left.registration_id
            .as_bytes()
            .cmp(right.registration_id.as_bytes())
    });
    eligible.sort_by(|left, right| {
        (
            left.2,
            left.0.registration_order,
            left.0.registration_id.as_bytes(),
        )
            .cmp(&(
                right.2,
                right.0.registration_order,
                right.0.registration_id.as_bytes(),
            ))
    });

    let Some((account, profile_id, earliest_reset)) = eligible.into_iter().next() else {
        return SelectionOutcome::Unavailable {
            reason: if snapshot.accounts.is_empty() {
                UnavailableReason::NoAccounts
            } else {
                UnavailableReason::NoEligibleAccounts
            },
            exclusions,
        };
    };
    let observation = &account.observation;
    SelectionOutcome::Selected {
        selection: Selection {
            registration_id: account.registration_id.clone(),
            generation: account.generation,
            registry_revision: snapshot.revision,
            model_id: model_id.to_owned(),
            profile_id,
            observation_process_epoch: observation.process_epoch,
            observation_monotonic_millis: observation.captured_monotonic_millis,
            evaluated_at: now,
            earliest_reset_unix_seconds: earliest_reset,
        },
        exclusions,
    }
}

fn valid_inventory(snapshot: &RegistrySnapshot) -> bool {
    if snapshot.revision == 0 {
        return false;
    }
    let mut ids = BTreeSet::new();
    let mut principals = BTreeSet::new();
    snapshot.accounts.iter().all(|account| {
        account.generation != 0
            && !account.registration_id.is_empty()
            && !account.principal_key.is_empty()
            && ids.insert(account.registration_id.as_bytes())
            && principals.insert(account.principal_key.as_bytes())
    })
}

fn assess(
    account: &RegisteredAccount,
    model_id: &str,
    now: EvaluationTime,
) -> Result<(String, u64), ExclusionReason> {
    if !account.automatic_use_allowed {
        return Err(ExclusionReason::ScopeNotAllowed);
    }
    if !account.authentication_usable {
        return Err(ExclusionReason::AuthenticationUnavailable);
    }
    let known = match &account.observation.state {
        ObservationState::Known(known) => known,
        ObservationState::Missing => return Err(ExclusionReason::Missing),
        ObservationState::FetchFailed => return Err(ExclusionReason::FetchFailed),
        ObservationState::MappingUnknown => return Err(ExclusionReason::MappingUnknown),
        ObservationState::Unsupported => return Err(ExclusionReason::Unsupported),
    };
    let observation = &account.observation;
    if observation.registration_id != account.registration_id
        || observation.generation != account.generation
        || observation.model_id != model_id
    {
        return Err(ExclusionReason::BindingMismatch);
    }
    if observation.process_epoch != now.process_epoch {
        return Err(ExclusionReason::Stale);
    }
    let Some(age) = now
        .monotonic_millis
        .checked_sub(observation.captured_monotonic_millis)
    else {
        return Err(ExclusionReason::ClockInvalid);
    };
    if observation.captured_unix_seconds == 0
        || observation.captured_unix_seconds > now.unix_seconds
    {
        return Err(ExclusionReason::ClockInvalid);
    }
    if age > MAX_OBSERVATION_AGE_MILLIS {
        return Err(ExclusionReason::Stale);
    }
    let earliest_reset = validate_known(known)?;
    if known
        .windows
        .iter()
        .any(|window| window.reset_unix_seconds <= now.unix_seconds)
    {
        return Err(ExclusionReason::ResetDue);
    }
    if known
        .bucket_states
        .iter()
        .any(|state| state.availability == ServerAvailability::Denied)
    {
        return Err(ExclusionReason::ServerDenied);
    }
    if known.bucket_states.iter().any(|state| {
        state.availability == ServerAvailability::Unknown || state.reached == ReachedState::Unknown
    }) {
        return Err(ExclusionReason::UnknownLimit);
    }
    if known
        .bucket_states
        .iter()
        .any(|state| state.reached == ReachedState::Reached)
        || known
            .windows
            .iter()
            .any(|window| window.usage_percent == 100.0)
    {
        return Err(ExclusionReason::Exhausted);
    }
    Ok((known.profile_id.clone(), earliest_reset))
}

fn validate_known(known: &KnownQuota) -> Result<u64, ExclusionReason> {
    if known.profile_id.is_empty() || known.expected_windows.is_empty() {
        return Err(ExclusionReason::InvalidObservation);
    }
    let expected: BTreeSet<_> = known.expected_windows.iter().collect();
    if expected.len() != known.expected_windows.len()
        || expected
            .iter()
            .any(|key| key.bucket_key.is_empty() || key.window_slot_key.is_empty())
    {
        return Err(ExclusionReason::InvalidObservation);
    }
    let windows: BTreeSet<_> = known.windows.iter().map(|window| &window.key).collect();
    if windows.len() != known.windows.len()
        || windows.len() != expected.len()
        || !windows.iter().all(|key| expected.contains(key))
        || known.windows.iter().any(|window| {
            !window.usage_percent.is_finite()
                || !(0.0..=100.0).contains(&window.usage_percent)
                || window.window_millis == 0
                || window.reset_unix_seconds == 0
        })
    {
        return Err(ExclusionReason::InvalidObservation);
    }
    let expected_buckets: BTreeSet<_> =
        expected.iter().map(|key| key.bucket_key.as_str()).collect();
    let states: BTreeSet<_> = known
        .bucket_states
        .iter()
        .map(|state| state.bucket_key.as_str())
        .collect();
    if states.len() != known.bucket_states.len()
        || states.len() != expected_buckets.len()
        || states != expected_buckets
        || known
            .bucket_states
            .iter()
            .any(|state| state.bucket_key.is_empty())
    {
        return Err(ExclusionReason::InvalidObservation);
    }
    known
        .windows
        .iter()
        .map(|window| window.reset_unix_seconds)
        .min()
        .ok_or(ExclusionReason::InvalidObservation)
}

#[cfg(test)]
mod tests {
    use super::*;

    const MODEL: &str = "model.test";
    const NOW: EvaluationTime = EvaluationTime {
        process_epoch: 7,
        monotonic_millis: 1_000_000,
        unix_seconds: 2_000_000,
    };

    fn key(bucket: &str, slot: &str) -> WindowKey {
        WindowKey {
            bucket_key: bucket.into(),
            window_slot_key: slot.into(),
        }
    }

    fn known(short_reset: u64, long_usage: f64) -> KnownQuota {
        KnownQuota {
            profile_id: "profile.test".into(),
            expected_windows: vec![key("short", "hour"), key("long", "week")],
            windows: vec![
                QuotaWindow {
                    key: key("short", "hour"),
                    usage_percent: 0.0,
                    window_millis: 1,
                    reset_unix_seconds: short_reset,
                },
                QuotaWindow {
                    key: key("long", "week"),
                    usage_percent: long_usage,
                    window_millis: 1,
                    reset_unix_seconds: short_reset + 10,
                },
            ],
            bucket_states: vec![
                BucketState {
                    bucket_key: "short".into(),
                    availability: ServerAvailability::Allowed,
                    reached: ReachedState::NotReached,
                },
                BucketState {
                    bucket_key: "long".into(),
                    availability: ServerAvailability::Allowed,
                    reached: ReachedState::NotReached,
                },
            ],
        }
    }

    fn account(id: &str, principal: &str, order: u64, quota: KnownQuota) -> RegisteredAccount {
        RegisteredAccount {
            registration_id: id.into(),
            generation: 1,
            registration_order: order,
            principal_key: principal.into(),
            automatic_use_allowed: true,
            authentication_usable: true,
            observation: QuotaObservation {
                registration_id: id.into(),
                generation: 1,
                model_id: MODEL.into(),
                process_epoch: NOW.process_epoch,
                captured_monotonic_millis: NOW.monotonic_millis - 1,
                captured_unix_seconds: NOW.unix_seconds - 1,
                state: ObservationState::Known(quota),
            },
        }
    }

    fn snapshot(accounts: Vec<RegisteredAccount>) -> RegistrySnapshot {
        RegistrySnapshot {
            revision: 1,
            accounts,
        }
    }

    fn selected_id(outcome: SelectionOutcome) -> String {
        match outcome {
            SelectionOutcome::Selected { selection, .. } => selection.registration_id,
            other => panic!("expected selected, got {other:?}"),
        }
    }

    fn unavailable_reason(outcome: SelectionOutcome) -> ExclusionReason {
        match outcome {
            SelectionOutcome::Unavailable { exclusions, .. } => exclusions[0].reason.clone(),
            other => panic!("expected unavailable, got {other:?}"),
        }
    }

    #[test]
    fn selects_earliest_reset_independent_of_input_order() {
        let a = account("a", "principal-a", 9, known(NOW.unix_seconds + 20, 0.0));
        let b = account("b", "principal-b", 1, known(NOW.unix_seconds + 30, 0.0));
        assert_eq!(
            selected_id(select_account(
                &snapshot(vec![a.clone(), b.clone()]),
                MODEL,
                NOW
            )),
            "a"
        );
        assert_eq!(
            selected_id(select_account(&snapshot(vec![b, a]), MODEL, NOW)),
            "a"
        );
    }

    #[test]
    fn considers_every_bucket_and_server_state() {
        let long_exhausted = account("a", "pa", 0, known(NOW.unix_seconds + 20, 100.0));
        assert_eq!(
            unavailable_reason(select_account(&snapshot(vec![long_exhausted]), MODEL, NOW)),
            ExclusionReason::Exhausted
        );
        let mut exhausted = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        if let ObservationState::Known(known) = &mut exhausted.observation.state {
            known.windows[0].usage_percent = 100.0;
        }
        let available = account("b", "pb", 0, known(NOW.unix_seconds + 30, 0.0));
        assert_eq!(
            selected_id(select_account(
                &snapshot(vec![exhausted, available]),
                MODEL,
                NOW
            )),
            "b"
        );

        let mut denied = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        if let ObservationState::Known(known) = &mut denied.observation.state {
            known.bucket_states[0].availability = ServerAvailability::Denied;
        }
        assert_eq!(
            unavailable_reason(select_account(&snapshot(vec![denied]), MODEL, NOW)),
            ExclusionReason::ServerDenied
        );
    }

    #[test]
    fn boundary_freshness_and_checked_clocks_are_enforced() {
        let mut at_limit = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        at_limit.observation.captured_monotonic_millis -= MAX_OBSERVATION_AGE_MILLIS - 1;
        assert_eq!(
            selected_id(select_account(&snapshot(vec![at_limit]), MODEL, NOW)),
            "a"
        );

        let mut stale = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        stale.observation.captured_monotonic_millis =
            NOW.monotonic_millis - MAX_OBSERVATION_AGE_MILLIS - 1;
        assert_eq!(
            unavailable_reason(select_account(&snapshot(vec![stale]), MODEL, NOW)),
            ExclusionReason::Stale
        );

        let mut future = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        future.observation.captured_monotonic_millis = NOW.monotonic_millis + 1;
        assert_eq!(
            unavailable_reason(select_account(&snapshot(vec![future]), MODEL, NOW)),
            ExclusionReason::ClockInvalid
        );
    }

    #[test]
    fn rejects_invalid_coverage_and_inventory_without_mutating_input() {
        let mut malformed = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        if let ObservationState::Known(known) = &mut malformed.observation.state {
            known.windows.pop();
        }
        assert_eq!(
            unavailable_reason(select_account(&snapshot(vec![malformed]), MODEL, NOW)),
            ExclusionReason::InvalidObservation
        );

        let a = account("a", "same", 0, known(NOW.unix_seconds + 20, 0.0));
        let b = account("b", "same", 1, known(NOW.unix_seconds + 30, 0.0));
        assert_eq!(
            select_account(&snapshot(vec![a, b]), MODEL, NOW),
            SelectionOutcome::InvalidInventory
        );

        let input = snapshot(vec![account(
            "a",
            "pa",
            0,
            known(NOW.unix_seconds + 20, 0.0),
        )]);
        let before = input.clone();
        assert_eq!(
            select_account(&input, MODEL, NOW),
            select_account(&input, MODEL, NOW)
        );
        assert_eq!(input, before);
    }

    #[test]
    fn binding_state_ordering_and_tie_breakers_are_stable() {
        let mut bad = account("b", "pb", 0, known(NOW.unix_seconds + 20, 0.0));
        bad.observation.model_id = "other".into();
        assert_eq!(
            unavailable_reason(select_account(&snapshot(vec![bad]), MODEL, NOW)),
            ExclusionReason::BindingMismatch
        );

        let mut missing = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        missing.observation.state = ObservationState::Missing;
        missing.observation.model_id = "other".into();
        assert_eq!(
            unavailable_reason(select_account(&snapshot(vec![missing]), MODEL, NOW)),
            ExclusionReason::Missing
        );

        let a = account("z", "pa", 1, known(NOW.unix_seconds + 20, 0.0));
        let b = account("a", "pb", 1, known(NOW.unix_seconds + 20, 0.0));
        assert_eq!(
            selected_id(select_account(&snapshot(vec![a, b]), MODEL, NOW)),
            "a"
        );
        let a = account("z", "pa", 1, known(NOW.unix_seconds + 20, 0.0));
        let b = account("a", "pb", 1, known(NOW.unix_seconds + 20, 0.0));
        assert_eq!(
            selected_id(select_account(&snapshot(vec![b, a]), MODEL, NOW)),
            "a"
        );
    }

    #[test]
    fn state_reset_unknown_and_reached_reasons_do_not_become_available() {
        for (state, expected) in [
            (ObservationState::Missing, ExclusionReason::Missing),
            (ObservationState::FetchFailed, ExclusionReason::FetchFailed),
            (
                ObservationState::MappingUnknown,
                ExclusionReason::MappingUnknown,
            ),
            (ObservationState::Unsupported, ExclusionReason::Unsupported),
        ] {
            let mut account = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
            account.observation.state = state;
            assert_eq!(
                unavailable_reason(select_account(&snapshot(vec![account]), MODEL, NOW)),
                expected
            );
        }

        let due = account("a", "pa", 0, known(NOW.unix_seconds, 0.0));
        assert_eq!(
            unavailable_reason(select_account(&snapshot(vec![due]), MODEL, NOW)),
            ExclusionReason::ResetDue
        );
        let overdue = account("a", "pa", 0, known(NOW.unix_seconds - 1, 0.0));
        assert_eq!(
            unavailable_reason(select_account(&snapshot(vec![overdue]), MODEL, NOW)),
            ExclusionReason::ResetDue
        );

        let mut unknown = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        if let ObservationState::Known(known) = &mut unknown.observation.state {
            known.bucket_states[0].reached = ReachedState::Unknown;
        }
        assert_eq!(
            unavailable_reason(select_account(&snapshot(vec![unknown]), MODEL, NOW)),
            ExclusionReason::UnknownLimit
        );
        let mut availability_unknown = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        if let ObservationState::Known(known) = &mut availability_unknown.observation.state {
            known.bucket_states[0].availability = ServerAvailability::Unknown;
        }
        assert_eq!(
            unavailable_reason(select_account(
                &snapshot(vec![availability_unknown]),
                MODEL,
                NOW
            )),
            ExclusionReason::UnknownLimit
        );

        let mut reached = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        if let ObservationState::Known(known) = &mut reached.observation.state {
            known.bucket_states[0].reached = ReachedState::Reached;
        }
        assert_eq!(
            unavailable_reason(select_account(&snapshot(vec![reached]), MODEL, NOW)),
            ExclusionReason::Exhausted
        );
    }

    #[test]
    fn rejects_invalid_numeric_and_coverage_values() {
        for usage in [-1.0, 100.1, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let mut account = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
            if let ObservationState::Known(known) = &mut account.observation.state {
                known.windows[0].usage_percent = usage;
            }
            assert_eq!(
                unavailable_reason(select_account(&snapshot(vec![account]), MODEL, NOW)),
                ExclusionReason::InvalidObservation
            );
        }

        let mut duplicate_state = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        if let ObservationState::Known(known) = &mut duplicate_state.observation.state {
            known.bucket_states.push(known.bucket_states[0].clone());
        }
        assert_eq!(
            unavailable_reason(select_account(&snapshot(vec![duplicate_state]), MODEL, NOW)),
            ExclusionReason::InvalidObservation
        );

        let mut zero_window = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        if let ObservationState::Known(known) = &mut zero_window.observation.state {
            known.windows[0].window_millis = 0;
        }
        assert_eq!(
            unavailable_reason(select_account(&snapshot(vec![zero_window]), MODEL, NOW)),
            ExclusionReason::InvalidObservation
        );
    }

    #[test]
    fn validates_permissions_revisions_and_empty_inventory() {
        let mut out_of_scope = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        out_of_scope.automatic_use_allowed = false;
        assert_eq!(
            unavailable_reason(select_account(&snapshot(vec![out_of_scope]), MODEL, NOW)),
            ExclusionReason::ScopeNotAllowed
        );
        let mut unauthenticated = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        unauthenticated.authentication_usable = false;
        assert_eq!(
            unavailable_reason(select_account(&snapshot(vec![unauthenticated]), MODEL, NOW)),
            ExclusionReason::AuthenticationUnavailable
        );
        assert_eq!(
            select_account(&snapshot(Vec::new()), MODEL, NOW),
            SelectionOutcome::Unavailable {
                reason: UnavailableReason::NoAccounts,
                exclusions: Vec::new()
            }
        );
        let mut zero_revision = snapshot(vec![account(
            "a",
            "pa",
            0,
            known(NOW.unix_seconds + 20, 0.0),
        )]);
        zero_revision.revision = 0;
        assert_eq!(
            select_account(&zero_revision, MODEL, NOW),
            SelectionOutcome::InvalidInventory
        );
        let max = account("a", "pa", u64::MAX, known(NOW.unix_seconds + 20, 0.0));
        assert_eq!(
            selected_id(select_account(
                &RegistrySnapshot {
                    revision: u64::MAX,
                    accounts: vec![max]
                },
                MODEL,
                NOW
            )),
            "a"
        );
    }

    #[test]
    fn q09_q10_q11_q12_epoch_clock_and_all_binding_fields() {
        let mut epoch = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        epoch.observation.process_epoch += 1;
        assert_eq!(
            unavailable_reason(select_account(&snapshot(vec![epoch]), MODEL, NOW)),
            ExclusionReason::Stale
        );
        let mut rollback = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        rollback.observation.captured_unix_seconds = NOW.unix_seconds + 1;
        assert_eq!(
            unavailable_reason(select_account(&snapshot(vec![rollback]), MODEL, NOW)),
            ExclusionReason::ClockInvalid
        );
        for change in [0, 1] {
            let mut mismatch = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
            if change == 0 {
                mismatch.observation.registration_id = "other".into();
            } else {
                mismatch.observation.generation = 2;
            }
            assert_eq!(
                unavailable_reason(select_account(&snapshot(vec![mismatch]), MODEL, NOW)),
                ExclusionReason::BindingMismatch
            );
        }
        // Token revision is deliberately absent: a normal token refresh does not change generation.
        assert_eq!(
            selected_id(select_account(
                &snapshot(vec![account(
                    "a",
                    "pa",
                    0,
                    known(NOW.unix_seconds + 20, 0.0)
                )]),
                MODEL,
                NOW
            )),
            "a"
        );
    }

    #[test]
    fn q14_exact_coverage_accepts_one_bucket_with_two_windows_and_rejects_each_set_error() {
        let two_windows = KnownQuota {
            profile_id: "p".into(),
            expected_windows: vec![key("same", "hour"), key("same", "week")],
            windows: vec![
                QuotaWindow {
                    key: key("same", "hour"),
                    usage_percent: 0.0,
                    window_millis: 1,
                    reset_unix_seconds: NOW.unix_seconds + 20,
                },
                QuotaWindow {
                    key: key("same", "week"),
                    usage_percent: 99.999,
                    window_millis: 1,
                    reset_unix_seconds: NOW.unix_seconds + 30,
                },
            ],
            bucket_states: vec![BucketState {
                bucket_key: "same".into(),
                availability: ServerAvailability::Allowed,
                reached: ReachedState::NotReached,
            }],
        };
        assert_eq!(
            selected_id(select_account(
                &snapshot(vec![account("a", "pa", 0, two_windows.clone())]),
                MODEL,
                NOW
            )),
            "a"
        );
        for bad in [
            {
                let mut value = two_windows.clone();
                value.expected_windows.clear();
                value
            },
            {
                let mut value = two_windows.clone();
                value.expected_windows.push(key("same", "hour"));
                value
            },
            {
                let mut value = two_windows.clone();
                value.windows.pop();
                value
            },
            {
                let mut value = two_windows.clone();
                value.windows[0].key = key("extra", "hour");
                value
            },
            {
                let mut value = two_windows.clone();
                value.windows.push(value.windows[0].clone());
                value
            },
            {
                let mut value = two_windows.clone();
                value.bucket_states.clear();
                value
            },
            {
                let mut value = two_windows.clone();
                value.bucket_states[0].bucket_key = "extra".into();
                value
            },
        ] {
            assert_eq!(
                unavailable_reason(select_account(
                    &snapshot(vec![account("a", "pa", 0, bad)]),
                    MODEL,
                    NOW
                )),
                ExclusionReason::InvalidObservation
            );
        }
    }

    #[test]
    fn q15_q16_raw_normalization_rejects_missing_signed_and_overflow_values() {
        let raw_key = || RawWindowKey {
            bucket_key: Some("b".into()),
            window_slot_key: Some("w".into()),
        };
        let raw = |length, reset| RawQuotaWindow {
            key: raw_key(),
            usage_percent: Some(0.0),
            window_millis: length,
            reset_unix_seconds: reset,
        };
        let states = vec![BucketState {
            bucket_key: "b".into(),
            availability: ServerAvailability::Allowed,
            reached: ReachedState::NotReached,
        }];
        assert!(
            normalize_known_quota(
                Some("p".into()),
                vec![raw_key()],
                vec![raw(Some(1), Some((NOW.unix_seconds + 1) as i128))],
                states.clone()
            )
            .is_ok()
        );
        for invalid in [
            normalize_known_quota(
                None,
                vec![raw_key()],
                vec![raw(Some(1), Some(1))],
                states.clone(),
            ),
            normalize_known_quota(
                Some("p".into()),
                vec![raw_key()],
                vec![RawQuotaWindow {
                    usage_percent: None,
                    ..raw(Some(1), Some(1))
                }],
                states.clone(),
            ),
            normalize_known_quota(
                Some("p".into()),
                vec![raw_key()],
                vec![raw(Some(-1), Some(1))],
                states.clone(),
            ),
            normalize_known_quota(
                Some("p".into()),
                vec![raw_key()],
                vec![raw(Some(1), Some(0))],
                states.clone(),
            ),
            normalize_known_quota(
                Some("p".into()),
                vec![raw_key()],
                vec![raw(Some(1), Some(-1))],
                states.clone(),
            ),
            normalize_known_quota(
                Some("p".into()),
                vec![raw_key()],
                vec![raw(Some(i128::MAX), Some(1))],
                states.clone(),
            ),
            normalize_known_quota(
                Some("p".into()),
                vec![raw_key()],
                vec![raw(Some(1), Some(i128::MAX))],
                states,
            ),
        ] {
            assert_eq!(invalid, Err(ExclusionReason::InvalidObservation));
        }
    }

    #[test]
    fn q17_to_q21_ordering_duplicate_ids_and_available_fallback() {
        let late = account("late", "pl", 2, known(NOW.unix_seconds + 20, 0.0));
        let early = account("early", "pe", 1, known(NOW.unix_seconds + 20, 0.0));
        assert_eq!(
            selected_id(select_account(&snapshot(vec![late, early]), MODEL, NOW)),
            "early"
        );
        let late = account("late", "pl", 2, known(NOW.unix_seconds + 20, 0.0));
        let early = account("early", "pe", 1, known(NOW.unix_seconds + 20, 0.0));
        assert_eq!(
            selected_id(select_account(&snapshot(vec![early, late]), MODEL, NOW)),
            "early"
        );
        let mut duplicate = account("a", "pb", 1, known(NOW.unix_seconds + 20, 0.0));
        duplicate.registration_id = "a".into();
        assert_eq!(
            select_account(
                &snapshot(vec![
                    account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0)),
                    duplicate
                ]),
                MODEL,
                NOW
            ),
            SelectionOutcome::InvalidInventory
        );
        let mut excluded = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        excluded.automatic_use_allowed = false;
        assert_eq!(
            selected_id(select_account(
                &snapshot(vec![
                    excluded,
                    account("b", "pb", 1, known(NOW.unix_seconds + 20, 0.0))
                ]),
                MODEL,
                NOW
            )),
            "b"
        );
        let mut unauthenticated = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        unauthenticated.authentication_usable = false;
        assert_eq!(
            selected_id(select_account(
                &snapshot(vec![
                    unauthenticated,
                    account("b", "pb", 1, known(NOW.unix_seconds + 20, 0.0))
                ]),
                MODEL,
                NOW
            )),
            "b"
        );
        let all_due = account("a", "pa", 0, known(NOW.unix_seconds, 0.0));
        assert_eq!(
            select_account(&snapshot(vec![all_due]), MODEL, NOW),
            SelectionOutcome::Unavailable {
                reason: UnavailableReason::NoEligibleAccounts,
                exclusions: vec![Exclusion {
                    registration_id: "a".into(),
                    reason: ExclusionReason::ResetDue
                }]
            }
        );
    }

    #[test]
    fn q22_to_q27_precedence_stable_exclusions_and_generation_range() {
        let unknown_mapping = RegisteredAccount {
            observation: QuotaObservation {
                state: ObservationState::MappingUnknown,
                ..account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0)).observation
            },
            ..account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0))
        };
        assert_eq!(
            unavailable_reason(select_account(&snapshot(vec![unknown_mapping]), MODEL, NOW)),
            ExclusionReason::MappingUnknown
        );
        let mut multi = account("z", "pz", 0, known(NOW.unix_seconds, 100.0));
        multi.observation.model_id = "wrong".into();
        let mut denied = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        if let ObservationState::Known(known) = &mut denied.observation.state {
            known.bucket_states[0].availability = ServerAvailability::Denied;
        }
        let outcome = select_account(&snapshot(vec![multi, denied]), MODEL, NOW);
        assert_eq!(
            outcome,
            SelectionOutcome::Unavailable {
                reason: UnavailableReason::NoEligibleAccounts,
                exclusions: vec![
                    Exclusion {
                        registration_id: "a".into(),
                        reason: ExclusionReason::ServerDenied
                    },
                    Exclusion {
                        registration_id: "z".into(),
                        reason: ExclusionReason::BindingMismatch
                    },
                ]
            }
        );
        let mut multi = account("z", "pz", 0, known(NOW.unix_seconds, 100.0));
        multi.observation.model_id = "wrong".into();
        let mut denied = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        if let ObservationState::Known(known) = &mut denied.observation.state {
            known.bucket_states[0].availability = ServerAvailability::Denied;
        }
        assert_eq!(
            select_account(&snapshot(vec![denied, multi]), MODEL, NOW),
            outcome
        );
        let mut zero_generation = account("a", "pa", 0, known(NOW.unix_seconds + 20, 0.0));
        zero_generation.generation = 0;
        assert_eq!(
            select_account(&snapshot(vec![zero_generation]), MODEL, NOW),
            SelectionOutcome::InvalidInventory
        );
        let mut max_generation = account("a", "pa", u64::MAX, known(NOW.unix_seconds + 20, 0.0));
        max_generation.generation = u64::MAX;
        max_generation.observation.generation = u64::MAX;
        assert_eq!(
            selected_id(select_account(
                &RegistrySnapshot {
                    revision: u64::MAX,
                    accounts: vec![max_generation]
                },
                MODEL,
                NOW
            )),
            "a"
        );
    }
}
