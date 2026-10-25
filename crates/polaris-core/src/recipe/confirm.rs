//! M7c-3b-2: whether a step actually did what it was asked to do.
//!
//! An exit code says a process ended. It does not say the manifest gained a
//! dependency, the configuration file appeared, or the build produced
//! anything. Every step is judged by reading the project back, and what was
//! read is kept as the evidence on the ledger entry.
use super::{Recipe, StepKind};
use std::path::Path;

/// What must hold once the step is over.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Postcondition {
    /// The package manager answered with a version.
    ManagerAnswered,
    /// The manifest names every package the step was asked to add.
    ManifestNames(Vec<String>),
    /// The file the host wrote is there.
    FileExists(String),
    /// The build left its output directory behind.
    BuildOutput(String),
}

/// How the step is judged. `Unknown` is never retried on its own: the
/// difference between "it did not happen" and "it could not be read" is the
/// difference between planning the step again and asking a person.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    Met { evidence: String },
    NotMet { missing: String },
    Unknown { reason: String },
}

/// What the step has to leave behind. The package manager does not change
/// any of these: the manifest, the configuration file and the build output
/// are named the same whichever one installed them.
pub fn postcondition(recipe: &Recipe, step: &str) -> Option<Postcondition> {
    let step = recipe.steps.iter().find(|held| held.id == step)?;
    Some(match step.kind {
        StepKind::Detect => Postcondition::ManagerAnswered,
        StepKind::Install => Postcondition::ManifestNames(
            ["next", "react", "react-dom"]
                .iter()
                .map(|held| (*held).to_owned())
                .collect(),
        ),
        StepKind::Configure => Postcondition::FileExists("next.config.mjs".into()),
        StepKind::Verify => Postcondition::BuildOutput(".next".into()),
    })
}

/// Reads the project to judge the step. `output` is what the command wrote,
/// used only where the answer is the output itself.
pub fn confirm(project_root: &Path, condition: &Postcondition, output: &str) -> Verdict {
    match condition {
        Postcondition::ManagerAnswered => {
            let answer = output.trim();
            if answer.is_empty() {
                return Verdict::NotMet {
                    missing: "The package manager returned no version".into(),
                };
            }
            Verdict::Met {
                evidence: format!("manager={answer}"),
            }
        }
        Postcondition::ManifestNames(packages) => {
            let path = project_root.join("package.json");
            let Ok(body) = std::fs::read_to_string(&path) else {
                return Verdict::Unknown {
                    reason: "Could not read package.json".into(),
                };
            };
            let Ok(manifest) = serde_json::from_str::<serde_json::Value>(&body) else {
                return Verdict::Unknown {
                    reason: "Could not parse package.json".into(),
                };
            };
            // A package counts as present only where a version is recorded
            // for it, so a name left in a comment or a script is not read
            // as a dependency.
            let has = |name: &String| {
                ["dependencies", "devDependencies"]
                    .iter()
                    .filter_map(|section| manifest.get(section))
                    .any(|section| section.get(name).is_some_and(|held| held.is_string()))
            };
            let missing: Vec<&str> = packages
                .iter()
                .filter(|name| !has(name))
                .map(String::as_str)
                .collect();
            if !missing.is_empty() {
                return Verdict::NotMet {
                    missing: format!(
                        "Dependencies missing from package.json: {}",
                        missing.join(", ")
                    ),
                };
            }
            Verdict::Met {
                evidence: format!("package.json: {}", packages.join(", ")),
            }
        }
        Postcondition::FileExists(name) | Postcondition::BuildOutput(name) => {
            let path = project_root.join(name);
            let wanted_directory = matches!(condition, Postcondition::BuildOutput(_));
            match std::fs::symlink_metadata(&path) {
                Ok(held) if held.is_dir() == wanted_directory && !held.file_type().is_symlink() => {
                    Verdict::Met {
                        evidence: format!("{name} exists"),
                    }
                }
                // Something is there but not what the step should have made.
                Ok(_) => Verdict::NotMet {
                    missing: format!("{name} has an unexpected type"),
                },
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Verdict::NotMet {
                    missing: format!("{name} is missing"),
                },
                // Permission, a broken mount, anything else: not the same as
                // the step having failed.
                Err(_) => Verdict::Unknown {
                    reason: format!("Could not read {name}"),
                },
            }
        }
    }
}

/// How the process ended, as the sandbox reported it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ended {
    pub exit_code: Option<i32>,
    pub signal: Option<i32>,
    /// The person stopped it.
    pub cancelled: bool,
}

/// The ledger terminal for a step, from how the process ended and what the
/// project says afterwards.
///
/// The two are combined rather than either one deciding alone. An exit of 0
/// with the work undone is a failure; a non-zero exit is a failure whatever
/// the project looks like, because a step that reported trouble has not
/// been shown to be safe to skip. Anything that could not be read back
/// settles as `Unknown`, which is never re-run on its own.
pub fn terminal(
    ended: Ended,
    verdict: Verdict,
    lockfile_digest_after: Option<String>,
) -> crate::desktop_store::SavedRecipeTerminal {
    use crate::desktop_store::{SavedRecipeTerminal, StepOutcome};
    let (outcome, evidence) = if ended.cancelled {
        (StepOutcome::Cancelled, None)
    } else {
        match (ended.exit_code, verdict) {
            (Some(0), Verdict::Met { evidence }) => (StepOutcome::Succeeded, Some(evidence)),
            (Some(0), Verdict::NotMet { missing }) => (StepOutcome::Failed, Some(missing)),
            (Some(0), Verdict::Unknown { reason }) => (StepOutcome::Unknown, Some(reason)),
            // A process that reported trouble, or that no exit code came
            // back for, is not read as done however the project looks.
            (Some(_), _) => (StepOutcome::Failed, None),
            (None, _) => (StepOutcome::Unknown, None),
        }
    };
    SavedRecipeTerminal {
        outcome,
        exit_code: ended.exit_code,
        signal: ended.signal,
        lockfile_digest_after,
        resolved_versions: Vec::new(),
        evidence,
        log: None,
    }
}

#[cfg(test)]
mod tests;
