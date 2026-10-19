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
use super::{confirm, git, ignore_rules, session, template};
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
        Err("このプラットフォームでは設定操作に必要な保護を確認できません。".into())
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
            .ok_or("op が必要です。")?;
        match op {
            "plan" => self.plan(),
            "tidy" => self.tidy(sandbox, gate, approver).await,
            "scaffold" => self.scaffold(sandbox, gate, approver).await,
            _ => Err("対応していない op です。".into()),
        }
    }

    /// What the recipes would do here, and what git looks like. Reads only.
    #[cfg(unix)]
    fn plan(&self) -> Result<String, String> {
        let looked = super::observe(&self.root, &super::NEXT_JS, Vec::new(), false);
        let nextjs = match super::manager_for(&looked) {
            Err(reason) => serde_json::json!({ "applicable": false, "reason": reason }),
            Ok(manager) => {
                // Steps are reported with the install included, so the model
                // can see the whole shape; running it still needs the
                // person's confirmation, which this tool never gives.
                let observed = super::observe(&self.root, &super::NEXT_JS, Vec::new(), true);
                let steps = match super::plan(&super::NEXT_JS, &observed) {
                    super::Plan::NotApplicable { reason } => {
                        return Ok(serde_json::json!({
                            "nextjs": { "applicable": false, "reason": reason },
                            "git": self.git_state(),
                        })
                        .to_string());
                    }
                    super::Plan::Run { steps, .. } => steps,
                };
                let (write, skip) = template::plan(&self.root, template::NEXT_JS);
                serde_json::json!({
                    "applicable": true,
                    "manager": manager.program(),
                    "steps": steps.iter().map(|step| serde_json::json!({
                        "id": step,
                        "postcondition_met": matches!(
                            confirm::postcondition(&super::NEXT_JS, step)
                                .map(|held| confirm::confirm(&self.root, &held, "")),
                            Some(confirm::Verdict::Met { .. })
                        ),
                    })).collect::<Vec<_>>(),
                    "templates_to_write": write.iter().map(|held| held.path).collect::<Vec<_>>(),
                    "templates_left_alone": skip.iter().map(|(path, _)| *path).collect::<Vec<_>>(),
                    // The install is the person's decision, made in the
                    // terminal. Saying so here stops the model asking again.
                    "install_requires_person": true,
                    "execution_unimplemented": true,
                })
            }
        };
        Ok(serde_json::json!({ "nextjs": nextjs, "git": self.git_state() }).to_string())
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
            std::fs::File::open(&self.root).map_err(|_| "対象を開けません。".to_owned())?;
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
    /// **The package install stays the person's.** This writes files the
    /// project does not have; it never runs a package manager, so a
    /// scaffolded project still does not build until the person installs.
    /// Saying so in the reply stops the model asking again.
    ///
    /// A path that exists is left exactly as it is, including a symlink or
    /// a directory where a file was expected: overwriting what someone
    /// already put there is the one thing a scaffold must not do.
    #[cfg(unix)]
    async fn scaffold(
        &self,
        sandbox: &SandboxPolicy,
        gate: &mut Gate,
        approver: &mut dyn Approver,
    ) -> Result<String, String> {
        let looked = super::observe(&self.root, &super::NEXT_JS, Vec::new(), false);
        let manager = match super::manager_for(&looked) {
            Err(reason) => {
                return Ok(serde_json::json!({ "written": false, "reason": reason }).to_string());
            }
            Ok(manager) => manager,
        };
        let (write, skip) = template::plan(&self.root, template::NEXT_JS);
        if write.is_empty() {
            return Ok(serde_json::json!({
                "written": false,
                "reason": "nothing_missing",
                "templates_left_alone": skip.iter().map(|(path, _)| *path).collect::<Vec<_>>(),
            })
            .to_string());
        }
        // Ordinary writes, so they ask the way every other write asks —
        // once per path, before any of them is made.
        for template in &write {
            gate.check_async(sandbox, &self.root.join(template.path), approver)
                .await?;
        }
        self.record_scaffold(manager, &write, &skip)
    }

    #[cfg(unix)]
    fn record_scaffold(
        &self,
        manager: super::Manager,
        write: &[template::Template],
        skip: &[(&'static str, template::Skipped)],
    ) -> Result<String, String> {
        let fail = |e: crate::desktop_store::StoreError| session::refusal(&e).to_owned();
        let (mut writer, _, held) = session::open(&self.root).map_err(fail)?;
        let request = format!(
            "tool-scaffold-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|held| held.as_nanos())
                .unwrap_or_default()
        );
        let target = session::start_run(&mut writer, &held, &request).map_err(fail)?;
        let operation = session::operation("scaffold").map_err(fail)?;
        // The digest is over what is being written, so re-running with the
        // same templates is recognised as the same step and re-running
        // after the templates change is not.
        let mut digest = Vec::new();
        for template in write {
            digest.extend_from_slice(template.path.as_bytes());
            digest.push(0);
            digest.extend_from_slice(template.body.as_bytes());
            digest.push(0);
        }
        writer
            .record_recipe_intent(
                &target,
                operation.clone(),
                crate::desktop_store::SavedRecipeIntent {
                    schema_version: crate::desktop_store::RECIPE_SCHEMA_VERSION,
                    recipe_id: "nextjs".into(),
                    recipe_version: super::SCHEMA_VERSION,
                    step_id: "scaffold".into(),
                    after: Vec::new(),
                    input_digest: crate::conversation_state::content_hash(&digest),
                    lockfile_digest_before: None,
                    source: None,
                    requested_versions: write
                        .iter()
                        .map(|template| template.path.to_owned())
                        .collect(),
                    environment: std::env::consts::OS.into(),
                },
            )
            .map_err(fail)?;

        let written = template::write(&self.root, write);
        // Writing returning success is not the same as the files being
        // there with the content that was meant, so each one is read back.
        let missing: Vec<&str> = write
            .iter()
            .filter(|template| {
                !std::fs::read_to_string(self.root.join(template.path))
                    .is_ok_and(|body| body == template.body)
            })
            .map(|template| template.path)
            .collect();
        let verdict = match &written {
            Err(reason) => confirm::Verdict::Unknown {
                reason: reason.to_string(),
            },
            Ok(_) if missing.is_empty() => confirm::Verdict::Met {
                evidence: format!(
                    "templates: {}",
                    write
                        .iter()
                        .map(|template| template.path)
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            },
            Ok(_) => confirm::Verdict::NotMet {
                missing: missing.join(", "),
            },
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
            "templates_written": write
                .iter()
                .map(|template| template.path)
                .collect::<Vec<_>>(),
            "templates_left_alone": skip.iter().map(|(path, _)| *path).collect::<Vec<_>>(),
            "manager": manager.program(),
            // Writing the files does not make the project build.
            "install_requires_person": true,
        })
        .to_string())
    }

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
                reason: ".gitignore を読み書きできませんでした".into(),
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
