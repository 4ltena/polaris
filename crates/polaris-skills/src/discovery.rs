//! Skill discovery. A single corrupt skill must not take down the whole
//! search, so anything unreadable is skipped. It is not simply discarded,
//! though: skipped skills and their reasons are returned to the caller as
//! `Discovered::skipped`. The caller (the future CLI) is responsible for
//! displaying them.

use std::path::{Path, PathBuf};

use crate::frontmatter;
use crate::{Skill, SkillError};

/// The reason a single skill was skipped. Whether `SKILL.md` itself could
/// not be read, versus was read but failed validation, are distinct
/// failures, so they are carried separately.
#[derive(Debug, thiserror::Error)]
pub enum SkipCause {
    /// `SKILL.md` exists but cannot be read (permissions, or the path
    /// itself is a directory, etc.). Parsing was never reached, so no
    /// `SkillError` is available.
    #[error("cannot read SKILL.md: {0}")]
    Unreadable(std::io::Error),
    /// `SKILL.md` was read but failed frontmatter validation.
    #[error(transparent)]
    Invalid(SkillError),
}

/// A single skipped skill. Carries the directory name and the reason. For
/// the same reason `SkillError` includes its own identifier in its message,
/// this also keeps the directory name as an explicit field — so the caller
/// can consistently pull out "which skill was skipped" regardless of the
/// kind of reason.
#[derive(Debug, thiserror::Error)]
#[error("{dir_name}: {cause}")]
pub struct Skipped {
    pub dir_name: String,
    #[source]
    pub cause: SkipCause,
}

/// The result of `discover`/`discover_in`. Carries both the skills that
/// loaded successfully and, with reasons, the skills that were skipped.
#[derive(Debug, Default)]
pub struct Discovered {
    pub skills: Vec<Skill>,
    pub skipped: Vec<Skipped>,
}

/// Walks the given directories in order. On a name collision, the first one
/// found wins. Skills that cannot be read or fail validation are pushed
/// onto `skipped` with a reason, and the search itself continues. Entries
/// within a directory are sorted by file name before processing — the order
/// `read_dir` returns is not reproducible run to run, so the order of
/// discovery results cannot be left to the filesystem. The order of the
/// search directories themselves (the outer loop) is used exactly as the
/// caller passed it.
pub fn discover_in(dirs: &[PathBuf]) -> Discovered {
    let mut skills: Vec<Skill> = Vec::new();
    let mut skipped: Vec<Skipped> = Vec::new();
    for dir in dirs {
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            continue;
        };
        let mut entries: Vec<std::fs::DirEntry> = read_dir.flatten().collect();
        entries.sort_by_key(std::fs::DirEntry::file_name);
        for entry in entries {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            let Some(dir_name) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let manifest = path.join("SKILL.md");
            let text = match std::fs::read_to_string(&manifest) {
                Ok(text) => text,
                // `SKILL.md` being absent means "this directory is not a
                // skill," not "this is a corrupt skill." Any other read
                // failure (permissions, or the path being a directory,
                // etc.) is reported, since it is the same hole as a skill
                // vanishing before it ever reaches parsing.
                Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
                Err(err) => {
                    skipped.push(Skipped {
                        dir_name: dir_name.to_string(),
                        cause: SkipCause::Unreadable(err),
                    });
                    continue;
                }
            };
            match frontmatter::parse(&text, dir_name) {
                Ok((name, description, body)) => {
                    if skills.iter().any(|s| s.name == name) {
                        continue;
                    }
                    skills.push(Skill {
                        name,
                        description,
                        body,
                        path: manifest,
                    });
                }
                Err(err) => skipped.push(Skipped {
                    dir_name: dir_name.to_string(),
                    cause: SkipCause::Invalid(err),
                }),
            }
        }
    }
    Discovered { skills, skipped }
}

/// Walks the two default locations plus any places added by configuration,
/// in this order.
pub fn discover(project_root: &Path, extra_paths: &[PathBuf]) -> Discovered {
    let mut dirs = vec![project_root.join(".polaris").join("skills")];
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(Path::new(&home).join(".polaris").join("skills"));
    }
    dirs.extend_from_slice(extra_paths);
    discover_in(&dirs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn put(root: &std::path::Path, name: &str, desc: &str, body: &str) {
        let d = root.join(name);
        std::fs::create_dir_all(&d).expect("could not create");
        std::fs::write(
            d.join("SKILL.md"),
            format!("---\nname: {name}\ndescription: {desc}\n---\n{body}\n"),
        )
        .expect("could not write");
    }

    /// `HOME` is shared across the whole process. cargo test runs tests
    /// concurrently within the same process, so unless the tests that swap
    /// it out are serialized against each other, one test can read the
    /// `HOME` another test set.
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Swaps out `HOME`, and restores it on leaving scope (even on panic).
    struct HomeGuard {
        prev: Option<std::ffi::OsString>,
        _lock: std::sync::MutexGuard<'static, ()>,
    }

    impl Drop for HomeGuard {
        fn drop(&mut self) {
            // SAFETY: only rewritten while holding HOME_LOCK. `discover` is
            // the only thing in this crate that reads HOME, and every test
            // that calls it takes this same lock.
            unsafe {
                match &self.prev {
                    Some(p) => std::env::set_var("HOME", p),
                    None => std::env::remove_var("HOME"),
                }
            }
        }
    }

    fn set_home(home: Option<&std::path::Path>) -> HomeGuard {
        // Even if the previous test panicked while holding the lock, don't
        // drag the next one down with it (the poisoned contents are just
        // `()`).
        let lock = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let prev = std::env::var_os("HOME");
        // SAFETY: same as above.
        unsafe {
            match home {
                Some(h) => std::env::set_var("HOME", h),
                None => std::env::remove_var("HOME"),
            }
        }
        HomeGuard { prev, _lock: lock }
    }

    #[test]
    fn the_project_directory_is_searched_before_home() {
        // This is the search order the specification fixes. If the order
        // were swapped, the winner of a name collision between a personal
        // skill and a project skill would silently flip — discover_in's
        // collision test only checks that "whichever is passed first wins,"
        // so it would not notice the order being reversed on the discover
        // side that builds the order to pass.
        let project = tempfile::tempdir().expect("temp dir");
        let home = tempfile::tempdir().expect("temp dir");
        put(
            &project.path().join(".polaris").join("skills"),
            "dup",
            "project side",
            "PROJECT",
        );
        put(
            &home.path().join(".polaris").join("skills"),
            "dup",
            "home side",
            "HOME",
        );

        let _guard = set_home(Some(home.path()));
        let found = discover(project.path(), &[]);

        assert_eq!(
            found.skills.len(),
            1,
            "a shared name should resolve to 1 entry"
        );
        assert_eq!(
            found.skills[0].body.trim(),
            "PROJECT",
            "search order differs from the specification: the project's skill lost to ~/.polaris/skills"
        );
    }

    #[test]
    fn home_is_searched_before_the_configured_extra_paths() {
        // Locations added by configuration come third. This also confirms
        // that the extra path is actually being searched at all — if it
        // weren't, the order claim would pass vacuously, so looking only at
        // the winner would be meaningless.
        let project = tempfile::tempdir().expect("temp dir");
        let home = tempfile::tempdir().expect("temp dir");
        let extra = tempfile::tempdir().expect("temp dir");
        put(
            &home.path().join(".polaris").join("skills"),
            "dup",
            "home side",
            "HOME",
        );
        put(extra.path(), "dup", "configured side", "EXTRA");
        put(
            extra.path(),
            "only-in-extra",
            "a location added only via configuration",
            "EXTRA-ONLY",
        );

        let _guard = set_home(Some(home.path()));
        let found = discover(project.path(), &[extra.path().to_path_buf()]);

        assert!(
            found.skills.iter().any(|s| s.name == "only-in-extra"),
            "the configured skills.paths was not searched: {:?}",
            found.skills.iter().map(|s| &s.name).collect::<Vec<_>>()
        );
        let dup = found
            .skills
            .iter()
            .find(|s| s.name == "dup")
            .expect("dup not found");
        assert_eq!(
            dup.body.trim(),
            "HOME",
            "search order differs from the specification: ~/.polaris/skills lost to the configured extra path"
        );
    }

    #[test]
    fn home_skills_are_found_when_the_project_has_none() {
        // The two tests above only look at who wins a collision, so even if
        // ~/.polaris/skills were dropped entirely, the "project wins" side
        // would still pass. This pins down, independently, that the second
        // default location is actually searched at all.
        let project = tempfile::tempdir().expect("temp dir");
        let home = tempfile::tempdir().expect("temp dir");
        put(
            &home.path().join(".polaris").join("skills"),
            "personal",
            "a personal skill",
            "HOME",
        );

        let _guard = set_home(Some(home.path()));
        let found = discover(project.path(), &[]);

        let names: Vec<&str> = found.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["personal"],
            "~/.polaris/skills was not searched"
        );
    }

    #[test]
    fn a_missing_home_does_not_stop_the_project_from_being_searched() {
        // Even in an environment with no HOME, the project's skill must
        // still be readable.
        let project = tempfile::tempdir().expect("temp dir");
        put(
            &project.path().join(".polaris").join("skills"),
            "alpha",
            "alpha",
            "PROJECT",
        );

        let _guard = set_home(None);
        let found = discover(project.path(), &[]);

        let names: Vec<&str> = found.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names, vec!["alpha"]);
        assert!(found.skipped.is_empty());
    }

    #[test]
    fn finds_skills_in_each_directory() {
        let a = tempfile::tempdir().expect("temp dir");
        let b = tempfile::tempdir().expect("temp dir");
        put(a.path(), "alpha", "alpha", "A");
        put(b.path(), "beta", "beta", "B");

        let found = discover_in(&[a.path().to_path_buf(), b.path().to_path_buf()]);
        let mut names: Vec<&str> = found.skills.iter().map(|s| s.name.as_str()).collect();
        names.sort_unstable();
        assert_eq!(names, vec!["alpha", "beta"]);
        assert!(found.skipped.is_empty());
    }

    #[test]
    fn first_directory_wins_on_a_name_collision() {
        let first = tempfile::tempdir().expect("temp dir");
        let second = tempfile::tempdir().expect("temp dir");
        put(first.path(), "dup", "first", "FIRST");
        put(second.path(), "dup", "second", "SECOND");

        let found = discover_in(&[first.path().to_path_buf(), second.path().to_path_buf()]);
        assert_eq!(found.skills.len(), 1);
        assert_eq!(found.skills[0].body.trim(), "FIRST");
    }

    #[test]
    fn an_invalid_skill_is_skipped_without_killing_the_others() {
        let root = tempfile::tempdir().expect("temp dir");
        put(root.path(), "good", "good", "OK");
        let bad = root.path().join("Bad-Name");
        std::fs::create_dir_all(&bad).expect("could not create");
        std::fs::write(
            bad.join("SKILL.md"),
            "---\nname: Bad-Name\ndescription: x\n---\n",
        )
        .ok();

        let found = discover_in(&[root.path().to_path_buf()]);
        assert_eq!(
            found.skills.len(),
            1,
            "one corrupt skill must not bring down all of them"
        );
        assert_eq!(found.skills[0].name, "good");
    }

    #[test]
    fn a_missing_directory_is_not_an_error() {
        let found = discover_in(&[std::path::PathBuf::from("/does/not/exist")]);
        assert!(found.skills.is_empty());
        assert!(found.skipped.is_empty());
    }

    #[test]
    fn a_directory_without_skill_md_is_ignored() {
        let root = tempfile::tempdir().expect("temp dir");
        std::fs::create_dir_all(root.path().join("notaskill")).expect("could not create");
        let found = discover_in(&[root.path().to_path_buf()]);
        assert!(found.skills.is_empty());
        assert!(found.skipped.is_empty());
    }

    #[test]
    fn a_skipped_skill_names_the_directory_that_failed() {
        // If discover_in silently discarded a corrupt skill, nothing would
        // reach the caller no matter how carefully frontmatter::parse
        // upstream carries the skill name. Confirm that an error lands on
        // the skipped side, and that its message contains the name of the
        // corrupt directory.
        let root = tempfile::tempdir().expect("temp dir");
        let bad = root.path().join("Bad-Name");
        std::fs::create_dir_all(&bad).expect("could not create");
        std::fs::write(
            bad.join("SKILL.md"),
            "---\nname: Bad-Name\ndescription: x\n---\n",
        )
        .expect("could not write");

        let found = discover_in(&[root.path().to_path_buf()]);
        assert_eq!(
            found.skipped.len(),
            1,
            "one corrupt skill should be reported"
        );
        let message = found.skipped[0].to_string();
        assert!(
            message.contains("Bad-Name"),
            "the error does not contain the name of the corrupt skill: {message}"
        );
    }

    #[test]
    fn a_skill_md_that_cannot_be_read_is_reported_not_forgotten() {
        // Making SKILL.md a directory causes read_to_string to fail, but
        // this is not NotFound — something by that file's name exists, it
        // just cannot be read. Parsing is never reached, so no
        // frontmatter::parse error is available either. Even so, if this
        // is not recorded with a reason in skipped, the same hole opens one
        // step earlier: "a skill that failed parsing is reported, but a
        // skill that could not even be read before that point silently
        // vanishes."
        let root = tempfile::tempdir().expect("temp dir");
        let bad = root.path().join("unreadable-skill");
        std::fs::create_dir_all(bad.join("SKILL.md")).expect("could not create");

        let found = discover_in(&[root.path().to_path_buf()]);
        assert!(found.skills.is_empty());
        assert_eq!(
            found.skipped.len(),
            1,
            "one unreadable skill should be reported"
        );
        assert_eq!(found.skipped[0].dir_name, "unreadable-skill");
        let message = found.skipped[0].to_string();
        assert!(
            message.contains("unreadable-skill"),
            "the error does not contain the name of the unreadable skill: {message}"
        );
    }

    #[test]
    fn processing_order_within_a_directory_is_sorted_not_creation_order() {
        // Create them in close to the reverse of sorted order, so that any
        // accidental reliance on the order read_dir happens to return would
        // not go unnoticed — deliberately a non-monotonic order.
        let root = tempfile::tempdir().expect("temp dir");
        put(root.path(), "zeta", "zeta", "Z");
        put(root.path(), "mid", "mid", "M");
        put(root.path(), "alpha", "alpha", "A");

        let found = discover_in(&[root.path().to_path_buf()]);
        let names: Vec<&str> = found.skills.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["alpha", "mid", "zeta"],
            "processing order within the directory is not sorted by name"
        );
    }
}
