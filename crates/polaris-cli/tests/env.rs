//! Direct management stays independent of model credentials and preserves the
//! parent's refusal even with an explicit noninteractive plan approval.
#![cfg(target_os = "macos")]
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output, Stdio},
};

struct Fixture {
    _temporary: tempfile::TempDir,
    root: PathBuf,
    project: PathBuf,
}
impl Fixture {
    fn new() -> Self {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().canonicalize().unwrap();
        let project = root.join("project");
        fs::create_dir(&project).unwrap();
        fs::create_dir(root.join("home")).unwrap();
        Self {
            _temporary: temporary,
            root,
            project,
        }
    }
    fn run(&self, arguments: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_polaris"))
            .args(arguments)
            .current_dir(&self.project)
            .env_clear()
            .env("HOME", self.root.join("home"))
            .env("POLARIS_DATA_DIR", self.root.join("data"))
            .env("PATH", "/usr/bin:/bin")
            .env("TMPDIR", &self.root)
            .env("POLARIS_PROVIDER", "must-not-start-a-provider")
            .stdin(Stdio::null())
            .output()
            .unwrap()
    }
    fn unchanged(&self) {
        assert!(fs::read_dir(&self.project).unwrap().next().is_none());
    }
}

#[test]
fn status_lists_all_profiles_without_model_or_account_startup() {
    let fixture = Fixture::new();
    let result = fixture.run(&["env", "status"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let body = String::from_utf8(result.stdout).unwrap();
    assert_eq!(
        body.lines()
            .filter(|line| line.contains("\tavailable\tunverified\t"))
            .count(),
        11
    );
    assert!(!fixture.root.join("data").exists());
    fixture.unchanged();
}

#[test]
fn never_and_read_only_refuse_set_before_creating_state() {
    for prefix in [["--approval", "never"], ["--sandbox", "read-only"]] {
        let fixture = Fixture::new();
        let result = fixture.run(&[
            prefix[0],
            prefix[1],
            "env",
            "set",
            "node",
            "--plan-directory",
            "/does-not-exist",
            "--approve-plan",
            &"a".repeat(64),
        ]);
        assert!(!result.status.success());
        assert!(String::from_utf8_lossy(&result.stderr).contains("forbidden"));
        assert!(!fixture.root.join("data").exists());
        fixture.unchanged();
    }
}

#[test]
fn a_saved_plan_can_be_inspected_but_an_unmatched_approval_cannot_start_it() {
    let fixture = Fixture::new();
    let result = fixture.run(&["env", "plan", "node"]);
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let body = String::from_utf8(result.stdout).unwrap();
    let retained = body
        .lines()
        .find_map(|line| line.strip_prefix("Retained state: "))
        .unwrap();
    assert!(Path::new(retained).join("plan.json").is_file());
    assert!(!Path::new(retained).join("artifacts").exists());
    let rejected = fixture.run(&[
        "env",
        "set",
        "node",
        "--plan-directory",
        retained,
        "--approve-plan",
        &"0".repeat(64),
    ]);
    assert!(!rejected.status.success());
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("approval hash"));
    assert!(!Path::new(retained).join("artifacts").exists());
    fixture.unchanged();
}
