//! M7a-1: the prompt that hands a finished implementation to someone who can
//! try it on a real machine. Polaris writes what was changed and what to
//! check; it does not claim the check passed. The implementation being
//! finished and the real-device check passing are separate states.
use serde::Serialize;

pub const MAX_PATHS: usize = 32;
pub const MAX_SUMMARY_BYTES: usize = 2048;
pub const MAX_CHECK_BYTES: usize = 1024;

/// What the run produced, as the host recorded it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Implementation {
    pub run_id: String,
    pub attempt_id: String,
    pub project_root: String,
    /// Relative paths the run changed.
    pub paths: Vec<String>,
    pub summary: String,
    /// What a person should try, one line each.
    pub checks: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Refusal {
    /// Nothing was changed, so there is nothing to try.
    NoChanges,
    /// Without a check to run, a prompt would only describe the change.
    NoChecks,
    Bounds,
}

/// Builds the prompt. It names the generation it came from, so a later run
/// cannot be mistaken for the one that was checked.
pub fn prompt(implementation: &Implementation) -> Result<String, Refusal> {
    if implementation.paths.is_empty() {
        return Err(Refusal::NoChanges);
    }
    if implementation.checks.is_empty() {
        return Err(Refusal::NoChecks);
    }
    if implementation.paths.len() > MAX_PATHS
        || implementation.summary.len() > MAX_SUMMARY_BYTES
        || implementation.checks.iter().map(String::len).sum::<usize>() > MAX_CHECK_BYTES
        || implementation.project_root.is_empty()
        || implementation.run_id.is_empty()
    {
        return Err(Refusal::Bounds);
    }
    let paths = implementation
        .paths
        .iter()
        .map(|path| format!("- {path}"))
        .collect::<Vec<_>>()
        .join("\n");
    let checks = implementation
        .checks
        .iter()
        .enumerate()
        .map(|(index, check)| format!("{}. {check}", index + 1))
        .collect::<Vec<_>>()
        .join("\n");
    Ok(format!(
        "Please verify on a real device.\n\nTarget: {root}\nImplementation generation: run {run} / attempt {attempt}\n\nChanged files:\n{paths}\n\nImplementation summary:\n{summary}\n\nChecks requested:\n{checks}\n\nReport pass or fail, with reproduction steps for failures. This request indicates only that implementation is complete; it does not establish acceptance on a real device.",
        root = implementation.project_root,
        run = implementation.run_id,
        attempt = implementation.attempt_id,
        summary = implementation.summary,
    ))
}

#[cfg(test)]
mod tests;
