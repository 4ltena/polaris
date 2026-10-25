//! The three commands that had no caller until now: comparing folders,
//! handing an implementation to a person, and judging spare quota. Real
//! directories; nothing is sent and no account is contacted.
use std::fs;
use std::path::Path;
#[cfg(unix)]
use std::process::Command;

#[path = "support/product_binary.rs"]
mod product_binary;

fn run(args: &[&str], cwd: &Path) -> (bool, String) {
    let output = product_binary::command()
        .args(args)
        .current_dir(cwd)
        .output()
        .expect("the binary runs");
    (
        output.status.success(),
        format!(
            "{}{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        ),
    )
}

fn write(root: &Path, relative: &str, body: &str) {
    let path = root.join(relative);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).expect("the parent");
    }
    fs::write(path, body).expect("the file");
}

#[cfg(unix)]
fn git(dir: &Path, args: &[&str]) {
    let status = Command::new("git")
        .args(args)
        .current_dir(dir)
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_SYSTEM", "/dev/null")
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?}");
}

#[test]
fn two_copies_of_the_same_work_are_reported_and_an_unrelated_folder_is_not() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    for folder in ["a", "b"] {
        write(dir.path(), &format!("{folder}/src/main.rs"), "fn main() {}");
        write(dir.path(), &format!("{folder}/README.md"), "hello");
    }
    write(dir.path(), "c/unrelated.md", "nothing alike");

    let (ok, text) = run(&["compare", "a", "b", "c"], dir.path());
    assert!(ok, "{text}");
    assert!(text.contains('a') && text.contains('b'), "{text}");
    assert!(
        text.contains("Nothing is moved or deleted"),
        "a suggestion must not read as an instruction: {text}"
    );
    // Nothing is moved or removed by looking.
    assert!(dir.path().join("a/src/main.rs").is_file());
    assert!(dir.path().join("b/src/main.rs").is_file());
    assert!(dir.path().join("c/unrelated.md").is_file());
}

#[test]
fn folders_with_nothing_in_common_come_back_as_nothing_to_look_at() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    write(dir.path(), "a/only-here.rs", "x");
    write(dir.path(), "b/only-there.py", "y");
    let (ok, text) = run(&["compare", "a", "b"], dir.path());
    assert!(ok, "{text}");
    assert!(text.contains("No similar folders found"), "{text}");
}

#[test]
#[cfg(unix)]
fn the_handoff_names_the_generation_and_the_files_git_says_changed() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let root = dir.path().canonicalize().expect("canonical");
    git(&root, &["init", "--quiet", "--initial-branch=main"]);
    write(&root, "a.txt", "a");
    git(&root, &["add", "a.txt"]);
    git(&root, &["commit", "--quiet", "-m", "first"]);
    // One tracked change and one file git has never seen.
    write(&root, "a.txt", "a changed");
    write(&root, "new.txt", "new");

    let (ok, text) = run(
        &[
            "handoff",
            "--summary",
            "入力欄の折返しを直した",
            "--check",
            "長い行を貼って折り返るか",
        ],
        &root,
    );
    assert!(ok, "{text}");
    assert!(text.contains("- a.txt"), "{text}");
    assert!(
        text.contains("- new.txt"),
        "an untracked file is still something to try: {text}"
    );
    assert!(text.contains("Implementation generation: run "), "{text}");
    assert!(
        text.contains("does not establish acceptance on a real device"),
        "the prompt must not read as a passing result: {text}"
    );
}

#[test]
#[cfg(unix)]
fn a_run_that_changed_nothing_produces_no_handoff() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    let root = dir.path().canonicalize().expect("canonical");
    git(&root, &["init", "--quiet", "--initial-branch=main"]);
    write(&root, "a.txt", "a");
    git(&root, &["add", "a.txt"]);
    git(&root, &["commit", "--quiet", "-m", "first"]);

    let (ok, text) = run(
        &["handoff", "--summary", "何もしていない", "--check", "何か"],
        &root,
    );
    assert!(!ok, "{text}");
    assert!(text.contains("No changed files"), "{text}");
}

#[test]
fn spare_quota_is_refused_for_the_reason_that_actually_applies() {
    let dir = tempfile::tempdir().expect("a temporary directory");
    // Permission is checked before anything else, so that is the answer
    // until it is given.
    let (ok, text) = run(&["spare"], dir.path());
    assert!(ok, "{text}");
    assert!(text.contains("not authorized"), "{text}");

    // With permission, the missing quota reading stops it — and that is the
    // intended state until a real account provides one.
    let (ok, text) = run(&["spare", "--allow", "--candidates", "3"], dir.path());
    assert!(ok, "{text}");
    assert!(
        text.contains("No usage limit observation"),
        "a missing reading must never be read as spare quota: {text}"
    );
    assert!(!text.contains("permits cleanup"), "{text}");
}
