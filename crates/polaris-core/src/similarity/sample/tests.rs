//! Samples real temporary directories. No file is opened.
use super::*;

fn write(root: &Path, relative: &str, body: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("the parent");
    }
    std::fs::write(path, body).expect("the file");
}

#[test]
fn names_and_sizes_come_back_and_nothing_else() {
    let dir = tempfile::tempdir().expect("a temporary folder");
    let main_key = Path::new("src").join("main.rs").display().to_string();
    write(dir.path(), "src/main.rs", "fn main() {}");
    write(dir.path(), "README.md", "hello");
    let sampled = folder("one", dir.path()).expect("the folder samples");
    assert!(!sampled.truncated);
    assert_eq!(
        sampled.folder.entries.keys().collect::<Vec<_>>(),
        vec!["README.md", main_key.as_str()]
    );
    assert_eq!(sampled.folder.entries["README.md"], 5);
    assert_eq!(sampled.folder.entries[main_key.as_str()], 12);
    assert_eq!(sampled.folder.id, "one");
}

#[test]
fn a_missing_or_non_directory_root_is_not_an_empty_sample() {
    let dir = tempfile::tempdir().expect("a temporary folder");
    let missing = dir.path().join("missing");
    let file = dir.path().join("file.txt");
    std::fs::write(&file, "not a folder").expect("a regular file");

    for root in [&missing, &file] {
        assert!(
            folder("one", root).is_err(),
            "{root:?} must not produce a successful empty comparison"
        );
    }
}

#[test]
#[cfg(unix)]
fn an_unreadable_root_is_an_error_when_permissions_are_enforced() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("a temporary folder");
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o000))
        .expect("locked root");

    let read_is_denied = std::fs::read_dir(dir.path()).is_err();
    let sampled = folder("one", dir.path());
    std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))
        .expect("unlocked root");

    if read_is_denied {
        assert!(
            sampled.is_err(),
            "an unreadable root must not sample as empty"
        );
    }
}

#[test]
fn what_a_build_left_behind_is_not_what_the_project_is() {
    let dir = tempfile::tempdir().expect("a temporary folder");
    let main_key = Path::new("src").join("main.rs").display().to_string();
    write(dir.path(), "src/main.rs", "fn main() {}");
    for skipped in SKIPPED {
        write(dir.path(), &format!("{skipped}/thing.js"), "x");
    }
    let sampled = folder("one", dir.path()).expect("the folder samples");
    assert_eq!(
        sampled.folder.entries.keys().collect::<Vec<_>>(),
        vec![main_key.as_str()],
        "including build output would make every project resemble every other"
    );
}

#[test]
#[cfg(unix)]
fn a_symlink_out_of_the_folder_is_not_sampled_as_part_of_it() {
    let dir = tempfile::tempdir().expect("a temporary folder");
    let elsewhere = tempfile::tempdir().expect("another folder");
    write(elsewhere.path(), "secret.txt", "someone else's");
    write(dir.path(), "own.txt", "mine");
    std::os::unix::fs::symlink(elsewhere.path(), dir.path().join("linked")).expect("a symlink");
    std::os::unix::fs::symlink(
        elsewhere.path().join("secret.txt"),
        dir.path().join("l.txt"),
    )
    .expect("a symlink");

    let sampled = folder("one", dir.path()).expect("the folder samples");
    assert_eq!(
        sampled.folder.entries.keys().collect::<Vec<_>>(),
        vec!["own.txt"]
    );
}

#[test]
fn a_folder_larger_than_the_bound_says_so_rather_than_comparing_a_slice() {
    let dir = tempfile::tempdir().expect("a temporary folder");
    for index in 0..MAX_ENTRIES + 10 {
        write(dir.path(), &format!("f{index:05}.txt"), "x");
    }
    let sampled = folder("one", dir.path()).expect("the folder samples");
    assert!(sampled.truncated);
    assert_eq!(sampled.folder.entries.len(), MAX_ENTRIES);
}

#[test]
fn sampling_stops_at_the_depth_bound() {
    let dir = tempfile::tempdir().expect("a temporary folder");
    let deep = "a/b/c/d/e/f/g/h/deep.txt";
    let deep_key = Path::new(deep)
        .components()
        .collect::<std::path::PathBuf>()
        .display()
        .to_string();
    write(dir.path(), deep, "x");
    write(dir.path(), "shallow.txt", "x");
    let sampled = folder("one", dir.path()).expect("the folder samples");
    assert!(sampled.folder.entries.contains_key("shallow.txt"));
    assert!(
        !sampled.folder.entries.contains_key(deep_key.as_str()),
        "{:?}",
        sampled.folder.entries.keys().collect::<Vec<_>>()
    );
}

#[test]
#[cfg(unix)]
fn an_unreadable_corner_does_not_make_the_folder_uncomparable() {
    use std::os::unix::fs::PermissionsExt;
    let dir = tempfile::tempdir().expect("a temporary folder");
    write(dir.path(), "own.txt", "mine");
    let locked = dir.path().join("locked");
    std::fs::create_dir(&locked).expect("a directory");
    write(&locked, "inside.txt", "x");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("locked");

    let sampled = folder("one", dir.path()).expect("the folder still samples");
    assert!(sampled.folder.entries.contains_key("own.txt"));

    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o700)).expect("unlocked");
}

#[test]
fn the_same_folder_samples_the_same_way_twice() {
    let dir = tempfile::tempdir().expect("a temporary folder");
    write(dir.path(), "src/a.rs", "a");
    write(dir.path(), "src/b.rs", "bb");
    write(dir.path(), "c.rs", "ccc");
    let first = folder("one", dir.path()).expect("the folder samples");
    let second = folder("one", dir.path()).expect("the folder samples again");
    assert_eq!(first, second, "the comparison has to be reproducible");
}
