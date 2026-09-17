//! Real repositories in a temporary directory; nothing is initialized in a
//! project and no commit is ever made outside these fixtures.
use super::*;
use std::process::Command;

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

fn at(dir: &Path) -> Result<Placement, GitReadError> {
    placement(&File::open(dir).expect("the directory opens"))
}

#[test]
fn a_directory_with_no_repository_is_never_read_as_one() {
    let plain = tempfile::tempdir().expect("a temporary directory");
    assert_eq!(at(plain.path()), Ok(Placement::None));
    assert_eq!(
        plan(at(plain.path())),
        GitPlan::NotApplicable {
            reason: GitRefusal::NoRepository
        }
    );
}

#[test]
fn a_repository_with_nothing_committed_is_still_a_repository() {
    let fresh = tempfile::tempdir().expect("a temporary directory");
    git(fresh.path(), &["init", "--quiet", "--initial-branch=main"]);
    // No commit yet. This is exactly the case that must not be initialized
    // again just because HEAD does not resolve.
    assert_eq!(at(fresh.path()), Ok(Placement::Own { head: Head::Unborn }));
    assert_eq!(plan(at(fresh.path())), GitPlan::TidyOnly);

    std::fs::write(fresh.path().join("a.txt"), "a").expect("a file");
    git(fresh.path(), &["add", "a.txt"]);
    git(fresh.path(), &["commit", "--quiet", "-m", "a"]);
    assert_eq!(
        at(fresh.path()),
        Ok(Placement::Own {
            head: Head::Committed
        })
    );
}

#[test]
fn a_directory_inside_someone_elses_repository_is_left_alone() {
    let outer = tempfile::tempdir().expect("a temporary directory");
    git(outer.path(), &["init", "--quiet", "--initial-branch=main"]);
    let inner = outer.path().join("packages").join("app");
    std::fs::create_dir_all(&inner).expect("the subdirectory");
    let Ok(Placement::Inside { top_level }) = at(&inner) else {
        panic!("a subdirectory belongs to the repository above it");
    };
    assert!(
        std::path::Path::new(&top_level).ends_with(
            outer
                .path()
                .canonicalize()
                .expect("canonical")
                .file_name()
                .expect("a name")
        ),
        "{top_level}"
    );
    assert!(matches!(
        plan(at(&inner)),
        GitPlan::NotApplicable {
            reason: GitRefusal::InsideAnother { .. }
        }
    ));
}

#[test]
fn a_linked_worktree_is_told_apart_from_an_ordinary_repository() {
    let main = tempfile::tempdir().expect("a temporary directory");
    git(main.path(), &["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(main.path().join("a.txt"), "a").expect("a file");
    git(main.path(), &["add", "a.txt"]);
    git(main.path(), &["commit", "--quiet", "-m", "a"]);
    let linked = main.path().join("linked");
    git(
        main.path(),
        &[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "side",
            linked.to_str().expect("utf-8"),
        ],
    );
    // `.git` here is a file, so anything written into a git dir would land
    // in the main repository instead.
    assert!(linked.join(".git").is_file());
    assert_eq!(
        at(&linked),
        Ok(Placement::Worktree {
            head: Head::Committed
        })
    );
    assert_eq!(plan(at(&linked)), GitPlan::TidyOnly);
}

#[test]
fn a_bare_repository_has_no_work_tree_to_set_up() {
    let bare = tempfile::tempdir().expect("a temporary directory");
    git(bare.path(), &["init", "--quiet", "--bare"]);
    assert_eq!(at(bare.path()), Ok(Placement::Bare));
    assert_eq!(
        plan(at(bare.path())),
        GitPlan::NotApplicable {
            reason: GitRefusal::BareRepository
        }
    );
}

#[test]
fn a_git_that_cannot_be_read_is_not_an_empty_directory() {
    assert_eq!(
        plan(Err(GitReadError)),
        GitPlan::NotApplicable {
            reason: GitRefusal::Unreadable
        },
        "refusing to read must never become a reason to initialize"
    );
}
