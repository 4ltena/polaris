//! What a project's build output may grow to, and what goes when it does.
//!
//! Build output is the one thing on this machine that grows without anybody
//! deciding to keep it. On 2026-09-17 a shared target directory reached
//! 122 GiB and left 268 MiB free, and the workspace tests failed with
//! `StorageFull` — not a defect in anything they were testing.
//!
//! The rules here are the ones that cleanup used by hand, because they were
//! the ones that turned out to be safe:
//!
//! - **Only what is old enough.** A fresh artefact belongs to a build that
//!   may still be running. Age is the cheapest evidence of that, and it does
//!   not need a lock or a process table.
//! - **Only what nothing else holds.** A file with more than one link is
//!   shared — Cargo hard-links between profiles and between projects — so
//!   removing it takes something away from a build that was never consulted.
//! - **Oldest first.** What has not been touched the longest is what a
//!   rebuild is least likely to want.
//!
//! Deciding is separate from deleting. Everything here is a pure function
//! over what was measured; the caller re-checks each path immediately before
//! removing it, the same discipline [`crate::scratch_registry`] uses.
//!
//! **What this replaces is `rm -rf target/debug`.** That is what the machine
//! actually gets today, and it is worse in both directions: it takes hours of
//! rebuilding that nothing needed to lose, and it takes it from whoever else
//! is using the directory. Observed on 2026-09-18, while this module was
//! being written: another session checked `pgrep -x cargo`, found nothing at
//! that instant, and removed the shared `target/debug` out from under a build
//! that was about to start. **Not running is not the same as not about to
//! run**, which is why age and link count are the evidence here and a process
//! table is not.

use std::path::PathBuf;

/// A fresh artefact belongs to a build that may still be running. Six hours
/// is long enough that a build which produced it has certainly finished, and
/// short enough that a machine filling up is still rescued the same day.
pub const MINIMUM_AGE_SECONDS: u64 = 6 * 60 * 60;

/// The absolute cap a project gets without saying anything. Decimal GB,
/// because that is the unit the size is written in.
pub const DEFAULT_BYTES: u64 = 100_000_000_000;
/// The share a project gets without saying anything.
pub const DEFAULT_PERCENT: u8 = 20;

/// How large a project's build output may grow.
///
/// Both forms may be set. **The smaller one wins**: two caps are two answers
/// to the same question, and taking the larger would let the setting that was
/// meant to protect the disk be cancelled by the one that was not. On a
/// 460 GiB disk the two defaults are within a few percent of each other, so
/// which one binds depends on the machine — that is the point of having both.
///
/// **The default is not "off".** A project that says nothing still gets
/// 100 GB / 20%, because the failure this exists to prevent happens to people
/// who never thought about it. Setting either to nothing is how a project
/// opts out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cap {
    /// An absolute size, in bytes.
    pub bytes: Option<u64>,
    /// A share of the filesystem the output sits on, 1–100. A share is what
    /// a person means by "do not let this take over the disk", and it keeps
    /// meaning that when the disk is replaced.
    pub percent: Option<u8>,
}

impl Default for Cap {
    fn default() -> Self {
        Self {
            bytes: Some(DEFAULT_BYTES),
            percent: Some(DEFAULT_PERCENT),
        }
    }
}

impl Cap {
    /// Neither cap set: nothing is ever removed. How a project opts out.
    pub const OFF: Self = Self {
        bytes: None,
        percent: None,
    };

    /// The cap in bytes on a filesystem of `total_bytes`, or `None` when both
    /// forms are unset.
    ///
    /// **`total_bytes` is an `Option` on purpose.** A share of an unknown
    /// total is not a cap of zero — it is no cap at all. Reading it as zero
    /// would make the smaller-wins rule choose it and empty the directory,
    /// which is the exact opposite of what a share was written to do.
    pub fn effective(&self, total_bytes: Option<u64>) -> Option<u64> {
        let share = self
            .percent
            .filter(|percent| (1..=100).contains(percent))
            .zip(total_bytes)
            .map(|(percent, total)| total / 100 * u64::from(percent));
        match (self.bytes, share) {
            (Some(bytes), Some(share)) => Some(bytes.min(share)),
            (held, None) | (None, held) => held,
        }
    }
}

/// One thing that could be removed, as measured.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    pub path: PathBuf,
    pub bytes: u64,
    /// Last modification, in seconds since the epoch.
    pub modified_unix_seconds: u64,
    /// How many links the file has. More than one means something else holds
    /// it too, and removing it here takes it from there as well.
    pub links: u64,
}

/// What the cap leaves no room for, oldest first.
///
/// Returns an empty plan when the cap is unset, when the total already fits,
/// or when nothing old enough and unshared is left to remove. **A plan that
/// cannot reach the cap is still returned in full**: removing what is safe
/// and remaining over the cap is a better answer than removing nothing, and
/// the caller reports the shortfall rather than hiding it.
pub fn plan(candidates: &[Candidate], cap: Option<u64>, now_unix_seconds: u64) -> Vec<PathBuf> {
    let Some(cap) = cap else {
        return Vec::new();
    };
    let total: u64 = candidates.iter().map(|held| held.bytes).sum();
    if total <= cap {
        return Vec::new();
    }
    let mut removable: Vec<&Candidate> = candidates
        .iter()
        .filter(|held| {
            held.links <= 1
                && now_unix_seconds.saturating_sub(held.modified_unix_seconds)
                    >= MINIMUM_AGE_SECONDS
        })
        .collect();
    // Oldest first, and by path where two share a timestamp so the same
    // measurement always produces the same plan.
    removable.sort_by(|left, right| {
        left.modified_unix_seconds
            .cmp(&right.modified_unix_seconds)
            .then_with(|| left.path.cmp(&right.path))
    });
    let mut freed = 0;
    let mut plan = Vec::new();
    for held in removable {
        if total.saturating_sub(freed) <= cap {
            break;
        }
        freed += held.bytes;
        plan.push(held.path.clone());
    }
    plan
}

#[cfg(test)]
#[path = "build_cache/tests.rs"]
mod tests;

/// The build output this project keeps, or `None` when it has none here.
///
/// `target` is the default because it is the one directory a build tool
/// creates, fills without bound, and can rebuild from nothing. It is also
/// Rust's name for it: a project that builds another way names its own
/// directory with `[build_cache] path`, and until it does, nothing of its is
/// touched. **Guessing at `node_modules` or `build` would be a guess about
/// what is disposable, and being wrong there deletes something a rebuild
/// cannot replace.**
pub fn directory(
    project_root: &std::path::Path,
    configured: Option<&std::path::Path>,
) -> Option<std::path::PathBuf> {
    let held = match configured {
        Some(path) if path.is_absolute() => path.to_owned(),
        Some(path) => project_root.join(path),
        None => project_root.join("target"),
    };
    held.is_dir().then_some(held)
}

/// The size of the filesystem `path` sits on, for the share form of the cap.
///
/// `None` when it cannot be read. A share of an unknown total is not a cap,
/// and guessing one would delete against a number nobody measured — so the
/// share is simply dropped and the absolute cap, if any, decides alone.
#[cfg(unix)]
pub fn filesystem_bytes(path: &std::path::Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let held = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `held` is a NUL-terminated path and `stat` is only read after
    // the call reports success.
    let stat = unsafe {
        let mut stat: libc::statvfs = std::mem::zeroed();
        (libc::statvfs(held.as_ptr(), &mut stat) == 0).then_some(stat)?
    };
    u64::from(stat.f_blocks).checked_mul(stat.f_frsize as u64)
}

#[cfg(not(unix))]
pub fn filesystem_bytes(_path: &std::path::Path) -> Option<u64> {
    None
}

/// What a sweep did, in the terms a person checks it by.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Swept {
    pub held_bytes: u64,
    pub cap_bytes: Option<u64>,
    pub removed: usize,
    pub freed_bytes: u64,
    /// What the plan named but could not remove — gone already, or refused
    /// by the re-check. Reported rather than swallowed: a sweep that removed
    /// less than it said must not read as one that removed everything.
    pub skipped: usize,
}

impl Swept {
    /// True when the sweep ran and the directory is still over its cap.
    /// Everything old enough and unshared has gone; what is left is in use.
    pub fn still_over(&self) -> bool {
        self.cap_bytes
            .is_some_and(|cap| self.held_bytes.saturating_sub(self.freed_bytes) > cap)
    }
}

/// Every regular file under `root`, with what the decision needs.
///
/// Symbolic links are counted but never followed: a link out of the build
/// directory must not make something outside it a candidate.
#[cfg(unix)]
pub fn measure(root: &std::path::Path, now_unix_seconds: u64) -> std::io::Result<Vec<Candidate>> {
    fn walk(
        directory: &std::path::Path,
        now: u64,
        into: &mut Vec<Candidate>,
    ) -> std::io::Result<()> {
        for entry in std::fs::read_dir(directory)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                walk(&entry.path(), now, into)?;
            } else if kind.is_file() {
                let held = entry.metadata()?;
                into.push(Candidate {
                    path: entry.path(),
                    bytes: held.len(),
                    modified_unix_seconds: held
                        .modified()
                        .ok()
                        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
                        .map_or(now, |since| since.as_secs()),
                    links: std::os::unix::fs::MetadataExt::nlink(&held),
                });
            }
        }
        Ok(())
    }
    let mut held = Vec::new();
    if root.is_dir() {
        walk(root, now_unix_seconds, &mut held)?;
    }
    Ok(held)
}

#[cfg(not(unix))]
pub fn measure(_root: &std::path::Path, _now_unix_seconds: u64) -> std::io::Result<Vec<Candidate>> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "build cache link identity is unavailable on this platform",
    ))
}

/// Whether what the plan named is still what the plan measured.
///
/// The measurement and the removal are different moments. In between, a build
/// may have rewritten the file or linked it into another profile — and on a
/// shared target directory, the build that does so belongs to somebody else.
/// Observed on 2026-09-18: a session that had checked for a running `cargo`
/// an instant earlier removed a shared tree out from under a build that was
/// about to start. **A plan is a proposal, not a warrant.**
#[cfg(unix)]
fn still_removable(held: &std::fs::Metadata, now_unix_seconds: u64) -> bool {
    let modified = held
        .modified()
        .ok()
        .and_then(|at| at.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(now_unix_seconds, |since| since.as_secs());
    held.is_file()
        && std::os::unix::fs::MetadataExt::nlink(held) <= 1
        && now_unix_seconds.saturating_sub(modified) >= MINIMUM_AGE_SECONDS
}

/// Measures `root`, removes what `cap` leaves no room for, and says what it
/// did.
///
/// **Every path is re-checked immediately before it is removed**, against the
/// same rules the plan used. The measurement and the removal are not the same
/// moment, and in between a build may have touched a file or linked it — the
/// plan is a proposal, not a warrant.
#[cfg(unix)]
pub fn sweep(
    root: &std::path::Path,
    cap: Cap,
    total_bytes: Option<u64>,
    now_unix_seconds: u64,
) -> std::io::Result<Swept> {
    let candidates = measure(root, now_unix_seconds)?;
    let held_bytes = candidates.iter().map(|held| held.bytes).sum();
    let cap_bytes = cap.effective(total_bytes);
    let mut swept = Swept {
        held_bytes,
        cap_bytes,
        ..Swept::default()
    };
    for path in plan(&candidates, cap_bytes, now_unix_seconds) {
        let Ok(held) = std::fs::symlink_metadata(&path) else {
            swept.skipped += 1;
            continue;
        };
        if !still_removable(&held, now_unix_seconds) || std::fs::remove_file(&path).is_err() {
            swept.skipped += 1;
            continue;
        }
        swept.removed += 1;
        swept.freed_bytes += held.len();
    }
    Ok(swept)
}

#[cfg(not(unix))]
pub fn sweep(
    _root: &std::path::Path,
    _cap: Cap,
    _total_bytes: Option<u64>,
    _now_unix_seconds: u64,
) -> std::io::Result<Swept> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "build cache cleanup is unavailable on this platform",
    ))
}
