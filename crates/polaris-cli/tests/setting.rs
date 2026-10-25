//! The bundled setting commands from the terminal. Real directories and a
//! real ledger; no package is installed and nothing is sent.
#![cfg(unix)]

use std::fs;
use std::path::PathBuf;
use std::process::Command;

fn bin() -> PathBuf {
    let mut path = std::env::current_exe().expect("this test binary");
    path.pop();
    if path.ends_with("deps") {
        path.pop();
    }
    path.join("polaris")
}

struct Project {
    _data: tempfile::TempDir,
    _project: tempfile::TempDir,
    root: PathBuf,
    data: PathBuf,
}

impl Project {
    fn new() -> Self {
        let data = tempfile::tempdir().expect("a data directory");
        let project = tempfile::tempdir().expect("a project directory");
        let root = project.path().canonicalize().expect("canonical");
        Self {
            data: data.path().canonicalize().expect("canonical"),
            root,
            _data: data,
            _project: project,
        }
    }

    fn git(&self, args: &[&str]) {
        let status = Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    fn run(&self, args: &[&str]) -> (bool, String) {
        let output = Command::new(bin())
            .args(["setting"])
            .args(args)
            .arg("--project")
            .arg(&self.root)
            .env("POLARIS_DATA_DIR", &self.data)
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

    fn gitignore(&self) -> String {
        fs::read_to_string(self.root.join(".gitignore")).unwrap_or_default()
    }
}

#[test]
fn a_directory_with_no_repository_is_left_alone() {
    let project = Project::new();
    let (ok, text) = project.run(&["git"]);
    assert!(ok, "{text}");
    assert!(text.contains("No Git repository"), "{text}");
    assert!(
        !project.root.join(".git").exists(),
        "nothing may be initialized on the person's behalf"
    );
    assert!(!project.root.join(".gitignore").exists());
}

#[test]
fn a_repository_with_no_commits_is_tidied_rather_than_initialized() {
    let project = Project::new();
    project.git(&["init", "--quiet", "--initial-branch=main"]);
    let head_before = fs::read_to_string(project.root.join(".git/HEAD")).expect("HEAD");

    let (ok, text) = project.run(&["git"]);
    assert!(ok, "{text}");
    assert!(text.contains("no commits"), "{text}");
    assert_eq!(
        fs::read_to_string(project.root.join(".git/HEAD")).expect("HEAD"),
        head_before,
        "the repository must not be re-initialized"
    );
    assert!(
        project.gitignore().contains(".next/"),
        "{}",
        project.gitignore()
    );
}

#[test]
fn existing_rules_keep_their_order_and_a_negation_holds_a_rule_back() {
    let project = Project::new();
    project.git(&["init", "--quiet", "--initial-branch=main"]);
    let before = "# build output\n!coverage/summary.json\nnode_modules/\n";
    fs::write(project.root.join(".gitignore"), before).expect("the file");

    let (ok, text) = project.run(&["git"]);
    assert!(ok, "{text}");
    assert!(text.contains("deferred due to conflict"), "{text}");
    let after = project.gitignore();
    assert!(after.starts_with(before), "{after:?}");
    assert!(
        !after.contains("\ncoverage/"),
        "a rule that would undo the negation was written: {after:?}"
    );

    // Running it again adds nothing: the ledger and the file agree.
    let (ok, text) = project.run(&["git"]);
    assert!(ok, "{text}");
    assert!(text.contains("No additions needed"), "{text}");
    assert_eq!(project.gitignore(), after);
}

#[test]
fn a_subdirectory_of_another_repository_is_refused() {
    let project = Project::new();
    project.git(&["init", "--quiet", "--initial-branch=main"]);
    let inner = project.root.join("packages/app");
    fs::create_dir_all(&inner).expect("the subdirectory");

    let output = Command::new(bin())
        .args(["setting", "git", "--project"])
        .arg(&inner)
        .env("POLARIS_DATA_DIR", &project.data)
        .output()
        .expect("the binary runs");
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    // `resolve_root` walks up to the repository, so the subdirectory is
    // treated as the project it belongs to rather than a project of its own.
    assert!(output.status.success(), "{text}");
    assert!(
        !inner.join(".git").exists(),
        "nothing may be created in a subdirectory"
    );
}

#[test]
fn a_second_run_on_the_same_project_waits_rather_than_opening_a_second_ledger() {
    // Two terminals in the same project must not each hold their own record
    // of what is finished.
    let project = Project::new();
    project.git(&["init", "--quiet", "--initial-branch=main"]);
    fs::write(project.root.join("package.json"), r#"{"name":"demo"}"#).expect("a manifest");

    // `open` reads the store location from the environment, so it is set
    // here before the writer is taken and restored afterwards.
    let previous = std::env::var_os("POLARIS_DATA_DIR");
    // SAFETY: this is the only test in this binary that changes it, and
    // integration tests in one file share a process serially per test only
    // when they do not touch process-wide state — this one does, so it
    // restores what it found.
    unsafe { std::env::set_var("POLARIS_DATA_DIR", &project.data) };
    let held = polaris_core::recipe::session::open(&project.root).expect("the session opens");

    let (ok, text) = project.run(&["git"]);
    assert!(!ok, "a second writer must not open: {text}");
    assert!(text.contains("setup is running elsewhere"), "{text}");

    drop(held);
    unsafe {
        match previous {
            Some(value) => std::env::set_var("POLARIS_DATA_DIR", value),
            None => std::env::remove_var("POLARIS_DATA_DIR"),
        }
    }
    // Once the first writer lets go, the same command works.
    let (ok, text) = project.run(&["git"]);
    assert!(ok, "{text}");
    assert!(project.gitignore().contains(".next/"), "{text}");
}
