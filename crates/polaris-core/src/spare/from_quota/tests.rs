//! Readings only; no account is contacted and nothing is spent.
use super::*;
use polaris_auth::quota::{KnownQuota, QuotaWindow, WindowKey};

fn window(usage: f64) -> QuotaWindow {
    QuotaWindow {
        key: WindowKey {
            bucket_key: "b".into(),
            window_slot_key: "w".into(),
        },
        usage_percent: usage,
        window_millis: 3_600_000,
        reset_unix_seconds: 0,
    }
}

fn observation(model: &str, state: ObservationState) -> QuotaObservation {
    QuotaObservation {
        registration_id: "r".into(),
        generation: 1,
        model_id: model.into(),
        process_epoch: 1,
        captured_monotonic_millis: 0,
        captured_unix_seconds: 0,
        state,
    }
}

fn known(usages: &[f64]) -> ObservationState {
    ObservationState::Known(KnownQuota {
        profile_id: "p".into(),
        expected_windows: Vec::new(),
        windows: usages.iter().copied().map(window).collect(),
        bucket_states: Vec::new(),
    })
}

fn allowed() -> Allowance {
    Allowance {
        allowed_by_person: true,
        reserved_percent: 20,
    }
}

#[test]
fn one_model_without_a_reading_leaves_the_whole_set_unknown() {
    for missing in [
        ObservationState::Missing,
        ObservationState::FetchFailed,
        ObservationState::MappingUnknown,
        ObservationState::Unsupported,
    ] {
        let held = observed(
            &[
                observation("a", known(&[10.0])),
                observation("b", missing.clone()),
            ],
            allowed(),
            false,
            3,
        );
        assert!(
            !held.quota_known_for_every_model,
            "{missing:?} is not a reading"
        );
        assert_eq!(
            super::super::decide(&held),
            super::super::Decision::Refuse {
                reason: super::super::Refusal::QuotaUnknown
            }
        );
    }
}

#[test]
fn no_observations_at_all_is_not_everything_being_known() {
    let held = observed(&[], allowed(), false, 3);
    assert!(
        !held.quota_known_for_every_model,
        "an empty set must not read as full coverage"
    );
}

#[test]
fn the_window_closest_to_full_decides_what_is_left() {
    // Averaging 10 and 90 would report 50 and let the nearly spent window
    // be used anyway.
    let held = observed(
        &[observation("a", known(&[10.0, 90.0]))],
        allowed(),
        false,
        3,
    );
    assert_eq!(held.remaining_percent, 10);
    assert_eq!(
        super::super::decide(&held),
        super::super::Decision::Refuse {
            reason: super::super::Refusal::NoSpare
        }
    );
}

#[test]
fn a_partly_used_window_is_never_reported_as_having_more_room_than_it_has() {
    let held = observed(&[observation("a", known(&[30.5]))], allowed(), false, 3);
    assert_eq!(held.remaining_percent, 69, "69.5 rounds down, not up");
}

#[test]
fn a_reading_outside_the_range_does_not_produce_a_nonsense_remainder() {
    // Below zero: the window is not emptier than empty.
    let below = observed(&[observation("a", known(&[-5.0]))], allowed(), false, 3);
    assert_eq!(below.remaining_percent, 100);
    // Above a hundred: no room at all, which is the safe reading, and not
    // a number that wraps around into plenty.
    let above = observed(&[observation("a", known(&[140.0]))], allowed(), false, 3);
    assert_eq!(above.remaining_percent, 0);
    assert_eq!(
        super::super::decide(&above),
        super::super::Decision::Refuse {
            reason: super::super::Refusal::NoSpare
        }
    );
}

#[test]
fn a_full_reading_with_room_and_permission_is_the_only_way_through() {
    let held = observed(&[observation("a", known(&[10.0]))], allowed(), false, 3);
    assert_eq!(super::super::decide(&held), super::super::Decision::Start);

    // Every one of these on its own is enough to stop it.
    let refused = observed(
        &[observation("a", known(&[10.0]))],
        Allowance {
            allowed_by_person: false,
            ..allowed()
        },
        false,
        3,
    );
    assert!(matches!(
        super::super::decide(&refused),
        super::super::Decision::Refuse { .. }
    ));
    let busy = observed(&[observation("a", known(&[10.0]))], allowed(), true, 3);
    assert!(matches!(
        super::super::decide(&busy),
        super::super::Decision::Refuse { .. }
    ));
    let nothing = observed(&[observation("a", known(&[10.0]))], allowed(), false, 0);
    assert!(matches!(
        super::super::decide(&nothing),
        super::super::Decision::Refuse { .. }
    ));
}
