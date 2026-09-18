//! Fixed observations only.
use super::*;

fn ready() -> Observed {
    Observed {
        allowed_by_person: true,
        quota_known_for_every_model: true,
        remaining_percent: 80,
        reserved_percent: 40,
        run_active: false,
        candidates: 3,
    }
}

#[test]
fn spare_work_starts_only_with_permission_a_known_reading_and_room_to_spare() {
    assert_eq!(decide(&ready()), Decision::Start);
    for (observed, reason) in [
        (
            Observed {
                allowed_by_person: false,
                ..ready()
            },
            Refusal::NotAllowed,
        ),
        (
            Observed {
                quota_known_for_every_model: false,
                ..ready()
            },
            Refusal::QuotaUnknown,
        ),
        (
            Observed {
                run_active: true,
                ..ready()
            },
            Refusal::Busy,
        ),
        (
            Observed {
                candidates: 0,
                ..ready()
            },
            Refusal::NothingToDo,
        ),
        (
            Observed {
                reserved_percent: 60,
                ..ready()
            },
            Refusal::NoSpare,
        ),
        (
            Observed {
                remaining_percent: 10,
                reserved_percent: 0,
                ..ready()
            },
            Refusal::NoSpare,
        ),
    ] {
        assert_eq!(decide(&observed), Decision::Refuse { reason });
    }
}

#[test]
fn an_unknown_reading_refuses_before_anything_else_is_considered() {
    // Today's shipped profiles are unverified, so this is the live state.
    let unknown = Observed {
        quota_known_for_every_model: false,
        remaining_percent: 100,
        reserved_percent: 0,
        candidates: 10,
        ..ready()
    };
    assert_eq!(
        decide(&unknown),
        Decision::Refuse {
            reason: Refusal::QuotaUnknown
        }
    );
    // Permission is checked first, so a person who never allowed it is told
    // that, not that the reading is missing.
    assert_eq!(
        decide(&Observed {
            allowed_by_person: false,
            ..unknown
        }),
        Decision::Refuse {
            reason: Refusal::NotAllowed
        }
    );
}
