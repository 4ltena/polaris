//! Turning real quota observations into what the decision needs.
//!
//! The decision itself is pure and stays that way. This is the one place
//! that reads observations, and it refuses to guess: a model whose reading
//! is missing, failed, unmapped or unsupported leaves the whole set unknown,
//! because spending quota nobody has measured is exactly what the decision
//! is there to prevent.
use super::Observed;
use polaris_auth::quota::{ObservationState, QuotaObservation};

/// What the person allowed, and what their own work is expected to need.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Allowance {
    pub allowed_by_person: bool,
    /// Share of the window kept for the work the person is doing, 0–100.
    pub reserved_percent: u32,
}

/// Builds the decision's input. `observations` is one per model the work
/// would use — an empty set is not "everything is known".
pub fn observed(
    observations: &[QuotaObservation],
    allowance: Allowance,
    run_active: bool,
    candidates: usize,
) -> Observed {
    let known: Vec<&polaris_auth::quota::KnownQuota> = observations
        .iter()
        .filter_map(|held| match &held.state {
            ObservationState::Known(quota) => Some(quota),
            // Missing, failed, unmapped, unsupported: all the same here.
            // None of them is a reading.
            _ => None,
        })
        .collect();
    let every_model_known = !observations.is_empty() && known.len() == observations.len();
    // The window closest to full decides what is left. Averaging them would
    // let a fresh window hide one that is nearly spent.
    // Seeded at 0, so a negative reading from an adapter cannot make the
    // window look emptier than empty.
    let highest_usage = known
        .iter()
        .flat_map(|quota| quota.windows.iter())
        .map(|window| window.usage_percent)
        .fold(0.0_f64, f64::max);
    Observed {
        allowed_by_person: allowance.allowed_by_person,
        quota_known_for_every_model: every_model_known,
        // Rounded down, so a partly used window is never reported as having
        // more room than it has. A reading above 100 makes this negative,
        // and the cast saturates to 0 — no room, which is the safe reading.
        remaining_percent: (100.0 - highest_usage) as u32,
        reserved_percent: allowance.reserved_percent.min(100),
        run_active,
        candidates,
    }
}

#[cfg(test)]
mod tests;
