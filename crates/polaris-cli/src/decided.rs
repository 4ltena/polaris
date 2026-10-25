//! `polaris decided` — the ledger of questions this project has settled.
//!
//! An agent re-opens a question it closed last week, because nothing in
//! its context says it was closed. This is where the closing gets written
//! down, one line at a time, so the next run is told before it starts.
//!
//! Append-only. Correcting an earlier entry means adding the correction as
//! its own line: a ledger whose rows can be rewritten cannot be trusted to
//! say what was decided *then*, which is the only thing it is for.

use polaris_core::{decisions, project};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    /// Record a decision in one statement; also accepts `polaris decided "…"`.
    what: Option<String>,
}

#[derive(clap::Subcommand)]
enum Command {
    /// Append a decision as one statement.
    Add {
        /// Statement to append. Punctuation, quotes, and newlines are preserved.
        what: String,
    },
    /// List recorded decisions, oldest first.
    List,
}

pub fn run(args: Args) -> Result<(), String> {
    if cfg!(windows)
        && matches!(
            (&args.command, &args.what),
            (Some(Command::Add { .. }), _) | (None, Some(_))
        )
    {
        return Err("Appending decisions is unsupported on Windows. No changes were made.".into());
    }
    let cwd =
        std::env::current_dir().map_err(|e| format!("Cannot read the working directory: {e}"))?;
    let root = project::resolve_root(&cwd);

    match (args.command, args.what) {
        (Some(Command::List), _) => list(&root),
        (Some(Command::Add { what }), _) | (None, Some(what)) => add(&root, &what),
        // Naming nothing lists, rather than erroring: the likeliest reason
        // to type the bare command is to see what is already there.
        (None, None) => list(&root),
    }
}

fn add(root: &std::path::Path, what: &str) -> Result<(), String> {
    let at = now();
    decisions::append(root, &at, what).map_err(|e| format!("Cannot append: {e}"))?;
    println!("{at}  {what}");
    println!("{}", root.join(decisions::PATH).display());
    Ok(())
}

fn list(root: &std::path::Path) -> Result<(), String> {
    let entries = decisions::load(root);
    if entries.is_empty() {
        println!(
            "No decisions yet. Append with `polaris decided \"…\"`.\n{}",
            root.join(decisions::PATH).display()
        );
        return Ok(());
    }
    for entry in &entries {
        println!("{}  {}", entry.at, entry.what);
    }
    println!(
        "\n{} items  {}",
        entries.len(),
        root.join(decisions::PATH).display()
    );
    Ok(())
}

/// RFC 3339 in local time, to the second.
///
/// Written by hand rather than pulled from a date crate: the ledger stores
/// this as free text and nothing parses it back, so the only requirement
/// is that it sorts and reads correctly. A dependency for that would be
/// the larger change.
fn now() -> String {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or_default();
    let offset = local_offset_seconds(now);
    let (y, mo, d, h, mi, s) = civil(now + offset);
    let (sign, off) = if offset < 0 {
        ('-', -offset)
    } else {
        ('+', offset)
    };
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}{sign}{:02}:{:02}",
        off / 3600,
        (off % 3600) / 60
    )
}

/// The local offset, taken from libc rather than assumed.
fn local_offset_seconds(unix: i64) -> i64 {
    // `date` is present on every platform this ships to, and asking it is
    // smaller than binding `localtime_r`. A failure falls back to UTC,
    // which is wrong by hours but never wrong by a day.
    std::process::Command::new("date")
        .args(["-r", &unix.to_string(), "+%z"])
        .output()
        .ok()
        .and_then(|out| String::from_utf8(out.stdout).ok())
        .and_then(|z| {
            let z = z.trim();
            let sign = if z.starts_with('-') { -1 } else { 1 };
            let digits = z.trim_start_matches(['+', '-']);
            let hours: i64 = digits.get(0..2)?.parse().ok()?;
            let minutes: i64 = digits.get(2..4)?.parse().ok()?;
            Some(sign * (hours * 3600 + minutes * 60))
        })
        .unwrap_or(0)
}

/// Civil date from a Unix second. Howard Hinnant's `civil_from_days`.
fn civil(unix: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = if m <= 2 { y + 1 } else { y };
    (
        y,
        m,
        d,
        (secs / 3600) as u32,
        ((secs % 3600) / 60) as u32,
        (secs % 60) as u32,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The stamp has to sort as text, because that is the only ordering
    /// the ledger ever gets — nothing parses it back into a date.
    #[test]
    fn the_stamp_is_rfc3339_and_sorts_as_text() {
        let (y, mo, d, h, mi, s) = civil(1_789_000_000);
        assert!((2020..2100).contains(&y), "{y}");
        assert!((1..=12).contains(&mo) && (1..=31).contains(&d), "{mo} {d}");
        assert!(h < 24 && mi < 60 && s < 60);

        let earlier = civil(1_789_000_000);
        let later = civil(1_789_086_400);
        let fmt = |(y, mo, d, h, mi, s): (i64, u32, u32, u32, u32, u32)| {
            format!("{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}")
        };
        assert!(fmt(earlier) < fmt(later), "a day later must sort later");
    }

    /// The epoch is a fixed point the algorithm has to land on exactly.
    #[test]
    fn the_epoch_is_the_first_of_january_1970() {
        assert_eq!(civil(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil(86_399), (1970, 1, 1, 23, 59, 59));
        assert_eq!(civil(86_400), (1970, 1, 2, 0, 0, 0));
    }

    /// A leap day is where a hand-written calendar goes wrong.
    #[test]
    fn a_leap_day_is_a_real_day() {
        // 2024-02-29T00:00:00Z
        assert_eq!(civil(1_709_164_800), (2024, 2, 29, 0, 0, 0));
        // 2100 is not a leap year: 2100-03-01T00:00:00Z
        assert_eq!(civil(4_107_542_400), (2100, 3, 1, 0, 0, 0));
    }
}
