//! M7c: the build recipes Polaris ships. A recipe is a fixed definition, not
//! something a model writes: it names the conditions it applies under, the
//! steps in order, and how each step is judged. Deciding what to run is pure;
//! running it is the caller's, through the existing execution owner, and a
//! real install stays behind the user's confirmation.
use serde::Serialize;

/// Bumped when the shape of a definition changes, so a saved ledger entry is
/// never reinterpreted under a different meaning.
pub const SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Manager {
    Npm,
    Pnpm,
    Yarn,
}

impl Manager {
    pub fn lockfile(self) -> &'static str {
        match self {
            Self::Npm => "package-lock.json",
            Self::Pnpm => "pnpm-lock.yaml",
            Self::Yarn => "yarn.lock",
        }
    }

    pub fn program(self) -> &'static str {
        match self {
            Self::Npm => "npm",
            Self::Pnpm => "pnpm",
            Self::Yarn => "yarn",
        }
    }
}

/// One step of a recipe. `install` is the only kind that changes the machine,
/// which is why it is the one a person confirms.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StepKind {
    /// Reads the project and the machine; changes nothing.
    Detect,
    /// Adds or updates dependencies.
    Install,
    /// Writes project configuration.
    Configure,
    /// Builds or starts, to judge the result.
    Verify,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Step {
    pub id: &'static str,
    pub kind: StepKind,
    /// The step this one needs, if any.
    pub after: Option<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Recipe {
    pub id: &'static str,
    pub schema_version: u32,
    /// The manifest that must exist for this recipe to apply.
    pub manifest: &'static str,
    pub steps: &'static [Step],
}

pub const NEXT_JS: Recipe = Recipe {
    id: "nextjs",
    schema_version: SCHEMA_VERSION,
    manifest: "package.json",
    steps: &[
        Step {
            id: "detect",
            kind: StepKind::Detect,
            after: None,
        },
        Step {
            id: "install",
            kind: StepKind::Install,
            after: Some("detect"),
        },
        Step {
            id: "configure",
            kind: StepKind::Configure,
            after: Some("install"),
        },
        Step {
            id: "verify",
            kind: StepKind::Verify,
            after: Some("configure"),
        },
    ],
};

pub const RECIPES: &[Recipe] = &[NEXT_JS];

/// What the host saw in the project before deciding anything.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub has_manifest: bool,
    /// The lockfiles present. More than one means the project mixes managers.
    pub lockfiles: Vec<Manager>,
    /// Steps already finished for this recipe and input, from the ledger.
    pub completed: Vec<String>,
    /// The user confirmed a real install for this run.
    pub install_confirmed: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "plan", rename_all = "snake_case")]
pub enum Plan {
    /// Nothing to do: the recipe does not apply here.
    NotApplicable { reason: Refusal },
    /// The steps still to run, in order, with the manager to use.
    Run {
        manager: Manager,
        steps: Vec<&'static str>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    NoManifest,
    /// Two lockfiles: choosing one would change how the project resolves.
    MixedManagers,
    /// Every step is done; re-running would repeat finished work.
    AlreadyComplete,
    /// The install step needs the person's confirmation.
    InstallNotConfirmed,
}

/// The manager the project's own lockfile chooses. A project with none has
/// not chosen, so the default applies; two lockfiles mean picking one would
/// change how the project resolves, which is not this command's call.
pub fn manager_for(observed: &Observed) -> Result<Manager, Refusal> {
    match observed.lockfiles.as_slice() {
        [] => Ok(Manager::Npm),
        [only] => Ok(*only),
        _ => Err(Refusal::MixedManagers),
    }
}

/// The steps left to run. An existing lockfile decides the manager, so a
/// project's own choice is never replaced, and finished steps are not redone.
pub fn plan(recipe: &Recipe, observed: &Observed) -> Plan {
    let refuse = |reason| Plan::NotApplicable { reason };
    if !observed.has_manifest {
        return refuse(Refusal::NoManifest);
    }
    let manager = match manager_for(observed) {
        Ok(manager) => manager,
        Err(reason) => return refuse(reason),
    };
    let remaining: Vec<&'static str> = recipe
        .steps
        .iter()
        .filter(|step| !observed.completed.iter().any(|done| done == step.id))
        .map(|step| step.id)
        .collect();
    if remaining.is_empty() {
        return refuse(Refusal::AlreadyComplete);
    }
    // A step whose predecessor is not finished cannot start, so the plan
    // stops at the first install unless it was confirmed.
    if !observed.install_confirmed
        && remaining.iter().any(|id| {
            recipe
                .steps
                .iter()
                .any(|step| step.id == *id && step.kind == StepKind::Install)
        })
    {
        let before_install: Vec<&'static str> = remaining
            .iter()
            .copied()
            .take_while(|id| {
                recipe
                    .steps
                    .iter()
                    .any(|step| step.id == *id && step.kind != StepKind::Install)
            })
            .collect();
        if before_install.is_empty() {
            return refuse(Refusal::InstallNotConfirmed);
        }
        return Plan::Run {
            manager,
            steps: before_install,
        };
    }
    Plan::Run {
        manager,
        steps: remaining,
    }
}

/// Reads the project to fill in what the plan needs. Only the manifest and
/// the lockfile names are looked at; no command is run and nothing is
/// installed. `completed` comes from the ledger, not from here.
pub fn observe(
    project_root: &std::path::Path,
    recipe: &Recipe,
    completed: Vec<String>,
    install_confirmed: bool,
) -> Observed {
    Observed {
        has_manifest: project_root.join(recipe.manifest).is_file(),
        lockfiles: [Manager::Npm, Manager::Pnpm, Manager::Yarn]
            .into_iter()
            .filter(|manager| project_root.join(manager.lockfile()).is_file())
            .collect(),
        completed,
        install_confirmed,
    }
}

/// The command one step runs, as program and arguments. The host executes it
/// through the existing execution owner; nothing is run from here, and a
/// model never writes these strings.
pub fn command(
    recipe: &Recipe,
    step: &str,
    manager: Manager,
) -> Option<(&'static str, Vec<String>)> {
    let step = recipe.steps.iter().find(|held| held.id == step)?;
    let program = manager.program();
    Some(match step.kind {
        // Reading what is installed changes nothing.
        StepKind::Detect => (program, vec!["--version".into()]),
        StepKind::Install => (
            program,
            match manager {
                Manager::Npm => vec![
                    "install".into(),
                    "next".into(),
                    "react".into(),
                    "react-dom".into(),
                ],
                Manager::Pnpm | Manager::Yarn => {
                    vec![
                        "add".into(),
                        "next".into(),
                        "react".into(),
                        "react-dom".into(),
                    ]
                }
            },
        ),
        // Configuration is written by the host, not by a package command.
        StepKind::Configure => return None,
        StepKind::Verify => (
            program,
            match manager {
                Manager::Npm => vec!["run".into(), "build".into()],
                Manager::Pnpm | Manager::Yarn => vec!["build".into()],
            },
        ),
    })
}

/// What the project looks like right now, for deciding what the ledger may
/// let this run skip. The manager decides the lockfile to read; the
/// environment is only what is known before anything has run, so it does
/// not change halfway through and invalidate the reuse it just allowed.
/// The versions actually resolved are recorded on each step's terminal.
pub fn current(
    project_root: &std::path::Path,
    recipe: &Recipe,
    manager: Manager,
) -> crate::desktop_store::RecipeCurrent {
    let hash = crate::conversation_state::content_hash;
    crate::desktop_store::RecipeCurrent {
        recipe_id: recipe.id.to_owned(),
        recipe_version: recipe.schema_version,
        // The normalized request: this recipe at this version against this
        // manager. Nothing about where it was launched from.
        input_digest: hash(
            format!(
                "{}/{}/{}",
                recipe.id,
                recipe.schema_version,
                manager.program()
            )
            .as_bytes(),
        ),
        lockfile_digest: std::fs::read(project_root.join(manager.lockfile()))
            .ok()
            .map(|body| hash(&body)),
        environment: format!("{}/{}", std::env::consts::OS, manager.program()),
    }
}

/// The steps this project has already finished, out of what the session's
/// ledger holds. A step whose inputs moved is not in the list, so it is
/// planned again rather than carried over.
pub fn reusable_steps_for(
    writer: &crate::desktop_store::Writer,
    recipe: &Recipe,
    project_root: &std::path::Path,
    manager: Manager,
) -> Vec<String> {
    let saved = writer.recipe_steps();
    crate::desktop_store::reusable_steps(&saved, &current(project_root, recipe, manager))
}

pub mod confirm;
/// Unix only: detection spawns git bound to an open directory.
#[cfg(any(target_os = "macos", target_os = "linux"))]
pub mod git;
pub mod ignore_rules;
pub mod session;
pub mod template;
pub mod tool;

#[cfg(test)]
mod tests;
