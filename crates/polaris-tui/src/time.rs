//! A tiny, dependency-free UTC timestamp formatter — just enough to
//! label `/resume` picker rows with a `"YYYY-MM-DD HH:MM"` string,
//! without pulling in a full date/time crate for that alone.

/// `days` is the number of days since the Unix epoch (1970-01-01);
/// negative for dates before it. Returns `(year, month, day)`. Ported
/// from Howard Hinnant's public-domain `civil_from_days` algorithm
/// (http://howardhinnant.github.io/date_algorithms.html).
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Formats a Unix timestamp in milliseconds as `"YYYY-MM-DD HH:MM"` UTC.
pub fn format_unix_millis(millis: u128) -> String {
    let total_seconds = (millis / 1000) as i64;
    let days = total_seconds.div_euclid(86_400);
    let secs_of_day = total_seconds.rem_euclid(86_400);
    let (y, m, d) = civil_from_days(days);
    let hh = secs_of_day / 3600;
    let mm = (secs_of_day % 3600) / 60;
    format!("{y:04}-{m:02}-{d:02} {hh:02}:{mm:02}")
}

/// A short "N unit ago" label — codex's own `/resume`-equivalent picker
/// labels rows this way (`"42s ago"`, `"35m ago"`, `"2h ago"`) rather
/// than an absolute timestamp, verified against its real
/// `resume_picker.rs` snapshots. Falls back to `format_unix_millis` past
/// 30 days, where "N days ago" stops being a useful at-a-glance label.
/// `then_millis` in the future (a clock that ran backward, or a session
/// stamped by a clock skewed ahead) reads as `"0s ago"` rather than
/// showing a negative duration.
pub fn format_relative(then_millis: u128, now_millis: u128) -> String {
    let elapsed_secs = now_millis.saturating_sub(then_millis) / 1000;
    const MINUTE: u128 = 60;
    const HOUR: u128 = 60 * MINUTE;
    const DAY: u128 = 24 * HOUR;
    const MONTH: u128 = 30 * DAY;

    if elapsed_secs < MINUTE {
        format!("{elapsed_secs}s ago")
    } else if elapsed_secs < HOUR {
        format!("{}m ago", elapsed_secs / MINUTE)
    } else if elapsed_secs < DAY {
        format!("{}h ago", elapsed_secs / HOUR)
    } else if elapsed_secs < MONTH {
        format!("{}d ago", elapsed_secs / DAY)
    } else {
        format_unix_millis(then_millis)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_unix_epoch_formats_as_1970_01_01() {
        assert_eq!(format_unix_millis(0), "1970-01-01 00:00");
    }

    #[test]
    fn a_known_timestamp_formats_correctly() {
        // 2024-01-15T09:30:00Z
        assert_eq!(format_unix_millis(1_705_311_000_000), "2024-01-15 09:30");
    }

    #[test]
    fn seconds_within_the_millisecond_input_are_dropped_not_rounded() {
        // 2024-01-15T09:30:59.999Z should still read as 09:30, not 09:31.
        assert_eq!(format_unix_millis(1_705_311_059_999), "2024-01-15 09:30");
    }

    #[test]
    fn relative_labels_pick_the_coarsest_unit_that_still_reads_naturally() {
        let now = 1_000_000_000u128;
        assert_eq!(format_relative(now - 42_000, now), "42s ago");
        assert_eq!(format_relative(now - 35 * 60_000, now), "35m ago");
        assert_eq!(format_relative(now - 2 * 3_600_000, now), "2h ago");
        assert_eq!(format_relative(now - 3 * 86_400_000, now), "3d ago");
    }

    #[test]
    fn relative_labels_fall_back_to_an_absolute_date_past_thirty_days() {
        let now = 1_705_311_000_000u128; // 2024-01-15T09:30:00Z
        let forty_days_ago = now - 40 * 86_400_000;
        assert_eq!(
            format_relative(forty_days_ago, now),
            format_unix_millis(forty_days_ago)
        );
    }

    #[test]
    fn a_timestamp_at_or_after_now_reads_as_just_now_not_negative() {
        let now = 1_000_000_000u128;
        assert_eq!(format_relative(now, now), "0s ago");
        assert_eq!(format_relative(now + 5_000, now), "0s ago");
    }
}
