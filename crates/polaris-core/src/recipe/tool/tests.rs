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
    assert_eq!(answer["nextjs"]["applicable"], true);
    assert_eq!(answer["nextjs"]["manager"], "npm");
    assert_eq!(
        answer["nextjs"]["install_requires_person"], true,
        "the tool must never be the thing that confirms an install"
    );
    assert_eq!(answer["nextjs"]["execution_unimplemented"], true);
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

/// `nextjs_setting` applies only where a Node project already is, so the
/// manifest is what makes the fixture eligible.
fn node_project(fixture: &Fixture) {
    std::fs::write(
        fixture.root.join("package.json"),
        "{\"name\":\"fixture\",\"private\":true}\n",
    )
    .expect("a manifest");
}

#[test]
fn a_scaffold_writes_what_is_missing_and_records_the_step() {
    let fixture = Fixture::new();
    fixture.git(&["init"]);
    node_project(&fixture);
    let answer = fixture
        .call(serde_json::json!({ "op": "scaffold" }), Decision::Allow)
        .expect("the scaffold runs");
    let answer: serde_json::Value = serde_json::from_str(&answer).expect("JSON");
    assert_eq!(answer["written"], true, "{answer}");
    assert!(
        fixture.root.join("app/page.tsx").is_file(),
        "a nested template creates its directory"
    );
    // Writing the files does not make the project build; saying so stops
    // the model treating the scaffold as a finished setup.
    assert_eq!(answer["install_requires_person"], true);

    // Running again finds nothing missing rather than rewriting.
    let again = fixture
        .call(serde_json::json!({ "op": "scaffold" }), Decision::Allow)
        .expect("the second call answers");
    let again: serde_json::Value = serde_json::from_str(&again).expect("JSON");
    assert_eq!(again["written"], false);
    assert_eq!(again["reason"], "nothing_missing");
}

#[test]
fn a_scaffold_never_overwrites_what_is_already_there() {
    let fixture = Fixture::new();
    fixture.git(&["init"]);
    node_project(&fixture);
    let kept = "// 利用者が書いたもの\n";
    std::fs::write(fixture.root.join("next.config.mjs"), kept).expect("an existing file");
    let answer = fixture
        .call(serde_json::json!({ "op": "scaffold" }), Decision::Allow)
        .expect("the scaffold runs");
    let answer: serde_json::Value = serde_json::from_str(&answer).expect("JSON");
    assert_eq!(
        std::fs::read_to_string(fixture.root.join("next.config.mjs")).expect("still readable"),
        kept,
        "overwriting what someone already put there is the one thing a scaffold must not do"
    );
    assert!(
        answer["templates_left_alone"]
            .as_array()
            .expect("a list")
            .iter()
            .any(|held| held == "next.config.mjs"),
        "{answer}"
    );
}

#[test]
fn a_refused_scaffold_writes_nothing() {
    let fixture = Fixture::new();
    fixture.git(&["init"]);
    node_project(&fixture);
    assert!(
        fixture
            .call(serde_json::json!({ "op": "scaffold" }), Decision::Deny)
            .is_err(),
        "a refused write is a refusal, not a quiet skip"
    );
    assert!(!fixture.root.join("next.config.mjs").exists());
    assert!(!fixture.root.join("app").exists());
}

#[test]
fn what_the_reply_lists_as_written_is_what_is_on_disk() {
    // The read-back inside `record_scaffold` cannot be made to fail from
    // out here: `template::plan` already excludes every path it cannot
    // write, so by the time the write runs there is nothing left to
    // obstruct. It stays because the gap between planning and writing is
    // a real one — a path can appear in it — and a guard against a race
    // is not dead code merely because a test cannot produce the race.
    // What is checkable is the weaker property: the reply does not claim
    // a file it did not write.
    let fixture = Fixture::new();
    fixture.git(&["init"]);
    node_project(&fixture);
    std::fs::write(fixture.root.join("app"), "not a directory").expect("an obstruction");
    let answer = fixture
        .call(serde_json::json!({ "op": "scaffold" }), Decision::Allow)
        .expect("the scaffold answers");
    let answer: serde_json::Value = serde_json::from_str(&answer).expect("JSON");
    for path in answer["templates_written"].as_array().expect("a list") {
        let path = path.as_str().expect("a path");
        assert!(
            fixture.root.join(path).is_file(),
            "{path} was reported written: {answer}"
        );
    }
    for path in answer["templates_left_alone"].as_array().expect("a list") {
        let path = path.as_str().expect("a path");
        assert!(
            !fixture.root.join(path).is_file(),
            "{path} was reported left alone but exists: {answer}"
        );
    }
}
