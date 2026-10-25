//! M7c-3b-3: the bundled setting commands as an internal tool.
//!
//! The model passes an operation name and a recipe name. It does not write
//! a shell command, a path, or a package name: everything the host runs or
//! writes comes from the shipped recipe. The same core functions the
//! terminal entry uses are called here, so the two cannot drift.
//!
//! Nothing is installed. `plan` only reads; `tidy` appends missing rules to
//! `.gitignore` and goes through the ordinary write approval first.
#[cfg(unix)]
use super::{confirm, git, ignore_rules, session};
use crate::approval::{Approver, Gate};
use polaris_sandbox::SandboxPolicy;
#[cfg(unix)]
use std::path::Path;
use std::path::PathBuf;

/// The rules `git_setting` keeps a project's ignore file holding. Fixed, so
/// a model cannot ask for an arbitrary pattern to be ignored.
#[cfg(unix)]
const IGNORES: &[&str] = &[".next/", "node_modules/", "coverage/"];

pub struct SettingTool {
    root: PathBuf,
}

impl SettingTool {
    #[cfg(not(unix))]
    pub async fn call(
        &self,
        _arguments: &serde_json::Value,
        _sandbox: &SandboxPolicy,
        _gate: &mut Gate,
        _approver: &mut dyn Approver,
    ) -> Result<String, String> {
        Err("Cannot verify the protections required for setup operations on this platform.".into())
    }

    /// `project_root` is the resolved project, not wherever the run started.
    pub fn new(project_root: PathBuf) -> Self {
        Self { root: project_root }
    }

    /// Runs one operation. Errors come back as a plain reason for the model
    /// to read; none of them names a path outside the project.
    #[cfg(unix)]
    pub async fn call(
        &self,
        arguments: &serde_json::Value,
        sandbox: &SandboxPolicy,
        gate: &mut Gate,
        approver: &mut dyn Approver,
    ) -> Result<String, String> {
        let op = arguments
            .get("op")
            .and_then(serde_json::Value::as_str)
            .ok_or("op is required.")?;
        match op {
            "plan" => self.plan(),
            "tidy" => self.tidy(sandbox, gate, approver).await,
            _ => Err("Unsupported op.".into()),
        }
    }

    /// What the recipes would do here, and what git looks like. Reads only.
    #[cfg(unix)]
    fn plan(&self) -> Result<String, String> {
        Ok(serde_json::json!({ "git": self.git_state() }).to_string())
    }

    #[cfg(unix)]
    fn git_state(&self) -> serde_json::Value {
        let Ok(opened) = std::fs::File::open(&self.root) else {
            return serde_json::json!({ "readable": false });
        };
        match git::plan(git::placement(&opened)) {
            git::GitPlan::NotApplicable { reason } => {
                serde_json::json!({ "applicable": false, "reason": reason })
            }
            git::GitPlan::TidyOnly => {
                let existing =
                    std::fs::read_to_string(self.root.join(".gitignore")).unwrap_or_default();
                let (add, withheld) = ignore_rules::plan_additions(&existing, IGNORES);
                serde_json::json!({
                    "applicable": true,
                    "rules_to_add": add,
                    "rules_withheld": withheld.iter().map(|held| &held.rule).collect::<Vec<_>>(),
                })
            }
        }
    }

    /// Appends the missing ignore rules. The only thing this tool changes.
    #[cfg(unix)]
    async fn tidy(
        &self,
        sandbox: &SandboxPolicy,
        gate: &mut Gate,
        approver: &mut dyn Approver,
    ) -> Result<String, String> {
        let opened =
            std::fs::File::open(&self.root).map_err(|_| "Cannot open the target.".to_owned())?;
        if let git::GitPlan::NotApplicable { reason } = git::plan(git::placement(&opened)) {
            return Ok(serde_json::json!({ "written": false, "reason": reason }).to_string());
        }
        let path = self.root.join(".gitignore");
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let (add, withheld) = ignore_rules::plan_additions(&existing, IGNORES);
        if add.is_empty() {
            return Ok(serde_json::json!({
                "written": false,
                "reason": "nothing_missing",
                "rules_withheld": withheld.iter().map(|held| &held.rule).collect::<Vec<_>>(),
            })
            .to_string());
        }
        // An ordinary write, so it asks the way every other write asks.
        gate.check_async(sandbox, &path, approver).await?;
        self.record(&add, &path)
    }

    /// Writes the Next.js templates this project is missing, and nothing
    /// else.
    ///
    /// Writes, reads back, and saves the step on the project's ledger.
    #[cfg(unix)]
    fn record(&self, add: &[&str], path: &Path) -> Result<String, String> {
        let existing = std::fs::read_to_string(path).unwrap_or_default();
        let fail = |e: crate::desktop_store::StoreError| session::refusal(&e).to_owned();
        let (mut writer, _, held) = session::open(&self.root).map_err(fail)?;
        let request = format!(
            "tool-tidy-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|held| held.as_nanos())
                .unwrap_or_default()
        );
        let target = session::start_run(&mut writer, &held, &request).map_err(fail)?;
        let operation = session::operation("gitignore").map_err(fail)?;
        writer
            .record_recipe_intent(
                &target,
                operation.clone(),
                crate::desktop_store::SavedRecipeIntent {
                    schema_version: crate::desktop_store::RECIPE_SCHEMA_VERSION,
                    recipe_id: "git".into(),
                    recipe_version: super::SCHEMA_VERSION,
                    step_id: "gitignore".into(),
                    after: Vec::new(),
                    input_digest: crate::conversation_state::content_hash(existing.as_bytes()),
                    lockfile_digest_before: None,
                    source: None,
                    requested_versions: add.iter().map(|held| (*held).to_owned()).collect(),
                    environment: std::env::consts::OS.into(),
                },
            )
            .map_err(fail)?;

        let written = ignore_rules::append(&self.root, add);
        // Writing returning success is not the same as the rules being there.
        let after = std::fs::read_to_string(path);
        let verdict = match (&written, &after) {
            (Err(_), _) | (_, Err(_)) => confirm::Verdict::Unknown {
                reason: "Could not read or write .gitignore".into(),
            },
            (Ok(()), Ok(body)) => {
                let missing: Vec<&str> = add
                    .iter()
                    .copied()
                    .filter(|rule| !body.lines().any(|line| line.trim() == *rule))
                    .collect();
                if missing.is_empty() {
                    confirm::Verdict::Met {
                        evidence: format!(".gitignore: {}", add.join(", ")),
                    }
                } else {
                    confirm::Verdict::NotMet {
                        missing: missing.join(", "),
                    }
                }
            }
        };
        let terminal = confirm::terminal(
            confirm::Ended {
                exit_code: Some(if written.is_ok() { 0 } else { 1 }),
                signal: None,
                cancelled: false,
            },
            verdict,
            None,
        );
        let outcome = terminal.outcome;
        writer
            .record_recipe_terminal(&target, &operation, terminal)
            .map_err(fail)?;
        Ok(serde_json::json!({
            "written": outcome == crate::desktop_store::StepOutcome::Succeeded,
            "outcome": outcome,
            "rules_added": add,
        })
        .to_string())
    }
}

#[cfg(all(test, unix))]
mod tests;
#[cfg(all(test, windows))]
mod windows_tests;
