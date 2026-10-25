//! Real directories and a real ledger. Nothing is installed and no command
//! is run.
use super::*;
use crate::approval::{ApprovalPolicy, Decision};
use polaris_sandbox::SandboxMode;

struct Answers(Decision);

impl Approver for Answers {
    fn ask(&mut self, _reason: &str) -> Decision {
        self.0
    }
}

/// `POLARIS_DATA_DIR` is process-wide, so these run one at a time.
use crate::project::DATA_DIR_SERIAL as SERIAL;

struct Fixture {
    _data: tempfile::TempDir,
    _project: tempfile::TempDir,
    root: PathBuf,
    _held: std::sync::MutexGuard<'static, ()>,
    previous: Option<std::ffi::OsString>,
}

impl Fixture {
    fn new() -> Self {
        let held = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
        let data = tempfile::tempdir().expect("a data directory");
        let project = tempfile::tempdir().expect("a project directory");
        let previous = std::env::var_os("POLARIS_DATA_DIR");
        // SAFETY: the lock above keeps this the only thread changing it.
        unsafe { std::env::set_var("POLARIS_DATA_DIR", data.path()) };
        let root = project.path().canonicalize().expect("canonical");
        Self {
            root,
            _data: data,
            _project: project,
            _held: held,
            previous,
        }
    }

    fn git(&self, args: &[&str]) {
        let status = std::process::Command::new("git")
            .args(args)
            .current_dir(&self.root)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_SYSTEM", "/dev/null")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?}");
    }

    fn call(&self, arguments: serde_json::Value, decision: Decision) -> Result<String, String> {
        let tool = SettingTool::new(self.root.clone());
        let sandbox = SandboxPolicy::new(
            SandboxMode::WorkspaceWrite,
            std::slice::from_ref(&self.root),
        )
        .expect("a policy");
        let mut gate = Gate::new(ApprovalPolicy::Always);
        let mut approver = Answers(decision);
        tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("a runtime")
            .block_on(tool.call(&arguments, &sandbox, &mut gate, &mut approver))
    }

    fn gitignore(&self) -> String {
        std::fs::read_to_string(self.root.join(".gitignore")).unwrap_or_default()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        // SAFETY: still holding the lock.
        unsafe {
            match self.previous.take() {
                Some(value) => std::env::set_var("POLARIS_DATA_DIR", value),
                None => std::env::remove_var("POLARIS_DATA_DIR"),
            }
        }
    }
}

fn parsed(body: &str) -> serde_json::Value {
    serde_json::from_str(body).expect("the tool answers with JSON")
}

#[test]
fn the_plan_says_what_would_happen_and_changes_nothing() {
    let fixture = Fixture::new();
    fixture.git(&["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(fixture.root.join("package.json"), r#"{"name":"demo"}"#).expect("a manifest");

    let answer = parsed(
        &fixture
            .call(serde_json::json!({"op":"plan"}), Decision::Allow)
            .expect("the plan"),
    );
    assert!(answer.get("nextjs").is_none());
    assert_eq!(answer["git"]["applicable"], true);
    assert!(
        answer["git"]["rules_to_add"]
            .as_array()
            .expect("the rules")
            .contains(&serde_json::json!(".next/"))
    );
    // Reading changed nothing.
    assert!(!fixture.root.join(".gitignore").exists());
    assert!(!fixture.root.join("next.config.mjs").exists());
}

#[test]
fn a_refused_write_leaves_the_file_alone_and_says_so() {
    let fixture = Fixture::new();
    fixture.git(&["init", "--quiet", "--initial-branch=main"]);
    let refused = fixture.call(serde_json::json!({"op":"tidy"}), Decision::Deny);
    assert!(refused.is_err(), "{refused:?}");
    assert_eq!(fixture.gitignore(), "", "a denied write must write nothing");
}

#[test]
fn an_allowed_tidy_writes_reads_back_and_records_the_step() {
    let fixture = Fixture::new();
    fixture.git(&["init", "--quiet", "--initial-branch=main"]);
    std::fs::write(
        fixture.root.join(".gitignore"),
        "# 自分の規則\n!coverage/summary.json\n",
    )
    .expect("the file");

    let answer = parsed(
        &fixture
            .call(serde_json::json!({"op":"tidy"}), Decision::Allow)
            .expect("the write"),
    );
    assert_eq!(answer["written"], true);
    assert_eq!(answer["outcome"], "succeeded");
    let after = fixture.gitignore();
    assert!(
        after.starts_with("# 自分の規則\n!coverage/summary.json\n"),
        "{after:?}"
    );
    assert!(after.contains(".next/"));
    assert!(
        !after.contains("\ncoverage/"),
        "a rule contradicting the negation was written: {after:?}"
    );

    // A second call has nothing left to do.
    let again = parsed(
        &fixture
            .call(serde_json::json!({"op":"tidy"}), Decision::Allow)
            .expect("the second call"),
    );
    assert_eq!(again["written"], false);
    assert_eq!(again["reason"], "nothing_missing");
    assert_eq!(fixture.gitignore(), after);
}

#[test]
fn a_directory_with_no_repository_is_not_tidied() {
    let fixture = Fixture::new();
    let answer = parsed(
        &fixture
            .call(serde_json::json!({"op":"tidy"}), Decision::Allow)
            .expect("the answer"),
    );
    assert_eq!(answer["written"], false);
    assert_eq!(answer["reason"], "no_repository");
    assert!(!fixture.root.join(".gitignore").exists());
    assert!(
        !fixture.root.join(".git").exists(),
        "a repository must never be created on the model's word"
    );
}

#[test]
fn an_operation_nobody_defined_is_refused() {
    let fixture = Fixture::new();
    for arguments in [
        serde_json::json!({"op":"install"}),
        serde_json::json!({"op":"run","command":"npm install next"}),
        serde_json::json!({}),
    ] {
        assert!(
            fixture.call(arguments.clone(), Decision::Allow).is_err(),
            "{arguments}"
        );
    }
}

#[test]
fn legacy_next_scaffold_is_rejected_without_writing() {
    let fixture = Fixture::new();
    assert!(
        fixture
            .call(serde_json::json!({"op":"scaffold"}), Decision::Allow)
            .is_err()
    );
    assert!(!fixture.root.join("app").exists());
}
