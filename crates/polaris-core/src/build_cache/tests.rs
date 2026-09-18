//! Each test names one thing that must hold before this is allowed to delete
//! a person's build output.
use super::*;

const HOUR: u64 = 60 * 60;
const NOW: u64 = 1_000 * HOUR;

fn old(path: &str, bytes: u64, hours_ago: u64) -> Candidate {
    Candidate {
        path: PathBuf::from(path),
        bytes,
        modified_unix_seconds: NOW - hours_ago * HOUR,
        links: 1,
    }
}

/// Nothing set means nothing is removed. This deletes files, so it does not
/// start without being asked.
#[test]
fn an_unset_cap_removes_nothing() {
    let cap = Cap::OFF;
    assert_eq!(cap.effective(Some(1_000)), None);
    assert_eq!(
        plan(&[old("a", 1_000, 99)], None, NOW),
        Vec::<PathBuf>::new()
    );
}

/// Two caps are two answers to one question. Taking the larger would let the
/// setting meant to protect the disk be cancelled by the one that was not.
#[test]
fn the_smaller_of_the_two_caps_is_the_one_that_applies() {
    let total = 1_000_000;
    let absolute = Cap {
        bytes: Some(100),
        percent: None,
    };
    let share = Cap {
        bytes: None,
        percent: Some(50),
    };
    assert_eq!(absolute.effective(Some(total)), Some(100));
    assert_eq!(share.effective(Some(total)), Some(500_000));
    let both = Cap {
        bytes: Some(100),
        percent: Some(50),
    };
    assert_eq!(both.effective(Some(total)), Some(100));
    let other_way = Cap {
        bytes: Some(900_000),
        percent: Some(50),
    };
    assert_eq!(other_way.effective(Some(total)), Some(500_000));
}

/// A share out of range is not a share. It is ignored rather than clamped:
/// clamping 0 to 1% would delete almost everything on a typo.
#[test]
fn a_share_outside_one_to_a_hundred_is_not_read() {
    for percent in [0, 101, 255] {
        let cap = Cap {
            bytes: None,
            percent: Some(percent),
        };
        assert_eq!(cap.effective(Some(1_000_000)), None, "{percent}");
    }
}

/// What has not been touched the longest is what a rebuild is least likely
/// to want, and removal stops the moment the cap is met.
#[test]
fn the_oldest_go_first_and_only_until_it_fits() {
    let candidates = [
        old("newest", 100, 10),
        old("oldest", 100, 100),
        old("middle", 100, 50),
    ];
    // 300 held, 150 allowed: the two oldest are enough.
    assert_eq!(
        plan(&candidates, Some(150), NOW),
        vec![PathBuf::from("oldest"), PathBuf::from("middle")]
    );
    // Already fitting: nothing goes.
    assert_eq!(plan(&candidates, Some(300), NOW), Vec::<PathBuf>::new());
}

/// A fresh artefact belongs to a build that may still be running.
#[test]
fn nothing_younger_than_the_minimum_age_is_touched() {
    let fresh = Candidate {
        path: PathBuf::from("fresh"),
        bytes: 1_000,
        modified_unix_seconds: NOW - (MINIMUM_AGE_SECONDS - 1),
        links: 1,
    };
    let just_old_enough = Candidate {
        path: PathBuf::from("aged"),
        bytes: 1_000,
        modified_unix_seconds: NOW - MINIMUM_AGE_SECONDS,
        links: 1,
    };
    assert_eq!(
        plan(std::slice::from_ref(&fresh), Some(0), NOW),
        Vec::<PathBuf>::new(),
        "a build may still be writing it"
    );
    assert_eq!(
        plan(&[just_old_enough], Some(0), NOW),
        vec![PathBuf::from("aged")]
    );
}

/// Cargo hard-links between profiles and between projects. Removing a shared
/// file takes it from a build that was never consulted.
#[test]
fn nothing_another_link_holds_is_touched() {
    let shared = Candidate {
        links: 2,
        ..old("shared", 1_000, 99)
    };
    assert_eq!(plan(&[shared], Some(0), NOW), Vec::<PathBuf>::new());
}

/// Removing what is safe and staying over the cap beats removing nothing.
/// The caller reports the shortfall; it is not hidden here.
#[test]
fn a_plan_that_cannot_reach_the_cap_still_removes_what_it_may() {
    let candidates = [
        old("removable", 100, 99),
        Candidate {
            links: 3,
            ..old("held", 10_000, 99)
        },
    ];
    assert_eq!(
        plan(&candidates, Some(10), NOW),
        vec![PathBuf::from("removable")]
    );
}

/// The same measurement must always give the same plan, or two runs disagree
/// about what they deleted.
#[test]
fn candidates_sharing_a_timestamp_are_ordered_by_path() {
    let candidates = [old("b", 100, 99), old("a", 100, 99), old("c", 100, 99)];
    assert_eq!(
        plan(&candidates, Some(100), NOW),
        vec![PathBuf::from("a"), PathBuf::from("b")]
    );
}

/// The two settings a person writes, read the way they wrote them.
#[test]
fn the_configuration_reads_both_forms_and_refuses_what_it_cannot_read() {
    let dir = tempfile::tempdir().unwrap();
    let write = |body: &str| {
        let path = dir.path().join("config.toml");
        std::fs::write(&path, body).unwrap();
        path
    };

    let path = write("[build_cache]\nmax_bytes = \"80GiB\"\nmax_percent = 20\n");
    let held = crate::config::try_load_from(None, Some(&path)).unwrap();
    assert_eq!(held.build_cache.bytes, Some(80 * (1 << 30)));
    assert_eq!(held.build_cache.percent, Some(20));

    // `GB` is decimal. Reading it as 2^30 would overstate the cap by 7%.
    let path = write("[build_cache]\nmax_bytes = \"500MB\"\n");
    let held = crate::config::try_load_from(None, Some(&path)).unwrap();
    assert_eq!(held.build_cache.bytes, Some(500_000_000));

    // A bare number is bytes.
    let path = write("[build_cache]\nmax_bytes = \"1024\"\n");
    assert_eq!(
        crate::config::try_load_from(None, Some(&path))
            .unwrap()
            .build_cache
            .bytes,
        Some(1024)
    );

    // A unit nobody uses for disk is refused rather than guessed: being
    // wrong about the unit deletes the wrong amount.
    let path = write("[build_cache]\nmax_bytes = \"80 furlongs\"\n");
    assert!(crate::config::try_load_from(None, Some(&path)).is_err());

    // A share outside 1–100 is a mistake, not a clamp.
    for percent in ["0", "101"] {
        let path = write(&format!("[build_cache]\nmax_percent = {percent}\n"));
        assert!(
            crate::config::try_load_from(None, Some(&path)).is_err(),
            "{percent}"
        );
    }

    // A project that says nothing still gets the defaults: the failure this
    // exists to prevent happens to people who never thought about it.
    let path = write("[decision]\nenabled = false\n");
    let held = crate::config::try_load_from(None, Some(&path)).unwrap();
    assert_eq!(held.build_cache.bytes, Some(DEFAULT_BYTES));
    assert_eq!(held.build_cache.percent, Some(DEFAULT_PERCENT));
}

fn age(path: &std::path::Path, hours_ago: u64) {
    let when =
        std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(NOW - hours_ago * HOUR);
    let times = std::fs::FileTimes::new().set_modified(when);
    std::fs::File::options()
        .write(true)
        .open(path)
        .unwrap()
        .set_times(times)
        .unwrap();
}

/// Measuring reaches into subdirectories and reports what the decision needs.
#[test]
fn measuring_walks_the_tree_and_reads_age_and_links() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir(dir.path().join("deps")).unwrap();
    std::fs::write(dir.path().join("deps/a.rlib"), vec![0; 100]).unwrap();
    std::fs::write(dir.path().join("b.rlib"), vec![0; 50]).unwrap();
    std::fs::hard_link(dir.path().join("b.rlib"), dir.path().join("c.rlib")).unwrap();
    age(&dir.path().join("deps/a.rlib"), 99);

    let held = measure(dir.path(), NOW).unwrap();
    assert_eq!(held.len(), 3);
    let deep = held
        .iter()
        .find(|held| held.path.ends_with("a.rlib"))
        .unwrap();
    assert_eq!(deep.bytes, 100);
    assert_eq!(deep.modified_unix_seconds, NOW - 99 * HOUR);
    assert_eq!(deep.links, 1);
    // The hard-linked pair report two links each, which is what keeps them.
    assert!(
        held.iter()
            .filter(|held| held.bytes == 50)
            .all(|held| held.links == 2)
    );
}

/// The whole thing, on a real directory.
#[test]
fn a_sweep_removes_the_oldest_and_says_what_it_did() {
    let dir = tempfile::tempdir().unwrap();
    for (name, hours) in [("old", 99), ("older", 200), ("fresh", 1)] {
        std::fs::write(dir.path().join(name), vec![0; 100]).unwrap();
        age(&dir.path().join(name), hours);
    }
    let cap = Cap {
        bytes: Some(150),
        percent: None,
    };
    let swept = sweep(dir.path(), cap, Some(1_000_000), NOW).unwrap();
    assert_eq!(swept.held_bytes, 300);
    assert_eq!(swept.cap_bytes, Some(150));
    // 300 held against a cap of 150: one removal leaves 200, so a second is
    // needed. Removal stops there rather than emptying the directory.
    assert_eq!(swept.removed, 2);
    assert_eq!(swept.freed_bytes, 200);
    assert_eq!(swept.skipped, 0);
    // Oldest first, and the fresh one was never a candidate.
    assert!(!dir.path().join("older").exists());
    assert!(!dir.path().join("old").exists());
    assert!(dir.path().join("fresh").exists());
    assert!(!swept.still_over());
}

/// When everything left is in use, the sweep says so rather than reporting
/// a success it did not have.
#[test]
fn a_sweep_that_cannot_reach_the_cap_reports_it() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("held"), vec![0; 1_000]).unwrap();
    std::fs::hard_link(dir.path().join("held"), dir.path().join("held-too")).unwrap();
    age(&dir.path().join("held"), 99);
    let cap = Cap {
        bytes: Some(10),
        percent: None,
    };
    let swept = sweep(dir.path(), cap, Some(1_000_000), NOW).unwrap();
    assert_eq!(swept.removed, 0);
    assert!(swept.still_over());
    assert!(dir.path().join("held").exists());
}

/// Nothing configured means nothing is touched, whatever is in the directory.
#[test]
fn a_sweep_without_a_cap_touches_nothing() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("big"), vec![0; 10_000]).unwrap();
    age(&dir.path().join("big"), 999);
    let swept = sweep(dir.path(), Cap::OFF, Some(1_000), NOW).unwrap();
    assert_eq!(swept.removed, 0);
    assert_eq!(swept.cap_bytes, None);
    assert!(!swept.still_over());
    assert!(dir.path().join("big").exists());
}

/// The re-check is what stands between the plan and somebody else's build.
/// The race it guards cannot be staged inside one process, so the rule is
/// tested directly: what the plan named must still be a lone, old, ordinary
/// file at the moment it is removed.
#[test]
fn the_recheck_refuses_anything_that_changed_since_it_was_measured() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("artefact");
    std::fs::write(&path, vec![0; 10]).unwrap();
    age(&path, 99);
    assert!(still_removable(
        &std::fs::symlink_metadata(&path).unwrap(),
        NOW
    ));

    // A build linked it into another profile between the two moments.
    std::fs::hard_link(&path, dir.path().join("linked")).unwrap();
    assert!(!still_removable(
        &std::fs::symlink_metadata(&path).unwrap(),
        NOW
    ));
    std::fs::remove_file(dir.path().join("linked")).unwrap();

    // A build rewrote it between the two moments.
    age(&path, 0);
    assert!(!still_removable(
        &std::fs::symlink_metadata(&path).unwrap(),
        NOW
    ));

    // What is no longer an ordinary file is not removed as one.
    let directory = dir.path().join("subdir");
    std::fs::create_dir(&directory).unwrap();
    assert!(!still_removable(
        &std::fs::symlink_metadata(&directory).unwrap(),
        NOW
    ));
}

/// `target` by default, the configured path when there is one, and nothing
/// at all when neither exists. Guessing at `node_modules` or `build` would
/// be a guess about what is disposable.
#[test]
fn the_swept_directory_is_target_unless_the_project_names_another() {
    let root = tempfile::tempdir().unwrap();
    // Nothing there: nothing is swept, rather than the project root itself.
    assert_eq!(directory(root.path(), None), None);

    std::fs::create_dir(root.path().join("target")).unwrap();
    assert_eq!(
        directory(root.path(), None),
        Some(root.path().join("target"))
    );

    // A relative path is read against the project root.
    std::fs::create_dir(root.path().join("build")).unwrap();
    assert_eq!(
        directory(root.path(), Some(std::path::Path::new("build"))),
        Some(root.path().join("build"))
    );

    // An absolute one is taken as written: a shared target directory lives
    // outside the project, which is exactly the case that filled the disk.
    let shared = tempfile::tempdir().unwrap();
    assert_eq!(
        directory(root.path(), Some(shared.path())),
        Some(shared.path().to_owned())
    );

    // A configured path that is not there is not created, and not swept.
    assert_eq!(
        directory(root.path(), Some(std::path::Path::new("absent"))),
        None
    );
}

/// The share needs a real total. An unreadable one drops the share rather
/// than deleting against a number nobody measured.
#[test]
fn the_filesystem_total_is_read_or_the_share_is_dropped() {
    let dir = tempfile::tempdir().unwrap();
    let total = filesystem_bytes(dir.path()).expect("a real filesystem");
    assert!(total > 0);
    assert_eq!(
        filesystem_bytes(std::path::Path::new("/does/not/exist")),
        None
    );

    // A share of an unknown total is no cap at all. Reading it as zero would
    // make the smaller-wins rule choose it and empty the directory — the
    // exact opposite of what a share is written to do.
    let share_only = Cap {
        bytes: None,
        percent: Some(20),
    };
    assert_eq!(share_only.effective(None), None);
    // The absolute cap still decides alone.
    let both = Cap {
        bytes: Some(1_000),
        percent: Some(20),
    };
    assert_eq!(both.effective(None), Some(1_000));
    // A genuinely empty filesystem is a different thing from an unknown one.
    assert_eq!(share_only.effective(Some(0)), Some(0));
}
