//! Offline compiled recipe execution on an existing controlled-process owner. Call on the
//! host worker, never on the TUI event thread or a disposable model future.
use polaris_sandbox::{ControlledEnd, PendingCleanup, SandboxMode, SandboxPolicy};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs,
    path::Path,
    time::{Duration, Instant},
};

#[derive(Debug, thiserror::Error)]
pub enum ProcessError {
    #[error("environment offline sandbox is unavailable")]
    Sandbox,
    #[error("environment runtime does not match its fixed version")]
    Runtime,
    #[error("environment runtime version probe did not match (output SHA-256 {observed_sha256})")]
    VersionMismatch { observed_sha256: String },
    #[error("environment process failed; no online or version fallback was attempted")]
    Failed,
    #[error(
        "environment process exited with {exit_code}: {diagnostic}; no online or version fallback was attempted"
    )]
    CommandFailed { exit_code: i32, diagnostic: String },
    #[error("environment process was cancelled or exceeded its approved time limit")]
    Cancelled,
    #[error("environment child cleanup remains pending")]
    CleanupPending,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessReceipt {
    pub exit_code: i32,
    pub stdout_sha256: String,
    pub stderr_sha256: String,
}

/// Pending children stay attached to this owner. Moving receipts out does not
/// transfer shutdown readiness: callers must also retain this owner.
#[derive(Default)]
pub struct ProcessOwner {
    pending: Vec<PendingCleanup>,
}

impl ProcessOwner {
    /// Execute one compiled operation on the retained host worker. The caller
    /// has persisted its intent and verified extracted runtimes and file proofs.
    /// A pending cleanup keeps this owner busy and prevents another spawn.
    #[cfg(unix)]
    // Keep the consent-bound plan, fresh snapshot and owned process controls explicit.
    #[allow(clippy::too_many_arguments)]
    pub fn run_planned(
        &mut self,
        definition: &crate::definition::Definition,
        plan: &crate::adapter::ResolvedPlan,
        fresh: &crate::adapter::Snapshot,
        candidate: usize,
        operation: usize,
        completed: &std::collections::BTreeSet<crate::adapter::RecipeId>,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<ProcessReceipt, ProcessError> {
        if self.cleanup_pending() {
            return Err(ProcessError::CleanupPending);
        }
        let selected = plan
            .candidates
            .iter()
            .find(|entry| entry.index == candidate)
            .ok_or(ProcessError::Runtime)?;
        let published = matches!(
            selected.operations.get(operation),
            Some(crate::adapter::Operation::Probe {
                probe: crate::adapter::ProbeId::PublishedPython
            })
        );
        if published {
            plan.digest(definition).map_err(|_| ProcessError::Runtime)?;
            let before = &plan.snapshot;
            if fresh.target != before.target
                || fresh.private_root != before.private_root
                || fresh.runner_binary_sha256 != before.runner_binary_sha256
                || fresh.host_os_revision != before.host_os_revision
                || fresh.platform != before.platform
                || fresh.host_prerequisites != before.host_prerequisites
            {
                return Err(ProcessError::Runtime);
            }
        } else {
            crate::adapter::revalidate(definition, plan, fresh)
                .map_err(|_| ProcessError::Runtime)?;
        }
        let command = selected
            .commands
            .get(&operation)
            .ok_or(ProcessError::Runtime)?;
        let tuple = definition
            .candidates
            .get(candidate)
            .ok_or(ProcessError::Runtime)?;
        let layout = crate::invocation::Layout::for_plan(plan, candidate)
            .map_err(|_| ProcessError::Runtime)?;
        let invocation = crate::invocation::render(command, tuple, &layout, fresh, completed)
            .map_err(|_| ProcessError::Runtime)?;
        let mut readable = vec![layout.artifacts()];
        let mut bins = Vec::new();
        for runtime in &selected.paths.private_stable {
            let root = layout.runtime(runtime);
            let bin = root.join("bin");
            if bin.is_dir() {
                bins.push(bin);
            }
            readable.push(root);
        }
        for root in &plan.system_read_roots {
            readable.push(
                Path::new(root)
                    .canonicalize()
                    .map_err(|_| ProcessError::Sandbox)?,
            );
        }
        for prerequisite in fresh.host_prerequisites.values() {
            readable.push(prerequisite.identity.canonical_path.clone().into());
        }
        let workspace = if published {
            Path::new(&fresh.target.canonical_path).join(".venv")
        } else {
            layout.stage()
        };
        let mode = if published {
            SandboxMode::ReadOnly
        } else {
            SandboxMode::WorkspaceWrite
        };
        let policy = SandboxPolicy::isolated_registered(mode, &workspace, &readable)
            .and_then(|policy| {
                policy.with_isolated_private_environment(
                    &layout.scratch(),
                    &layout.home(),
                    &layout.tmp(),
                )
            })
            .and_then(|policy| policy.with_isolated_runtime_bins(&bins))
            .and_then(|policy| policy.with_isolated_fixed_values(invocation.environment))
            .map_err(|_| ProcessError::Sandbox)?;
        let mut last_disk_check = Instant::now();
        if !within_disk_budget(
            layout.root(),
            definition.limits.disk_bytes,
            definition.limits.archive_entries,
        ) {
            return Err(ProcessError::Failed);
        }
        let (receipt, stdout) = self.controlled_command(
            &policy,
            &workspace,
            &invocation.program,
            &invocation.arguments,
            command.seconds,
            command.output_bytes_per_stream,
            &mut || {
                if cancelled() {
                    return true;
                }
                if last_disk_check.elapsed() >= Duration::from_millis(200) {
                    last_disk_check = Instant::now();
                    return !within_disk_budget(
                        layout.root(),
                        definition.limits.disk_bytes,
                        definition.limits.archive_entries,
                    );
                }
                false
            },
        )?;
        if !within_disk_budget(
            layout.root(),
            definition.limits.disk_bytes,
            definition.limits.archive_entries,
        ) {
            return Err(ProcessError::Failed);
        }
        if !matches_success(&command.success, &stdout) {
            return if matches!(
                selected.operations.get(operation),
                Some(crate::adapter::Operation::Probe {
                    probe: crate::adapter::ProbeId::NodeVersion
                        | crate::adapter::ProbeId::NpmVersion
                        | crate::adapter::ProbeId::PythonVersion
                        | crate::adapter::ProbeId::RustcVersion
                        | crate::adapter::ProbeId::CargoVersion
                        | crate::adapter::ProbeId::GoVersion
                })
            ) {
                Err(ProcessError::VersionMismatch {
                    observed_sha256: receipt.stdout_sha256,
                })
            } else {
                Err(ProcessError::Failed)
            };
        }
        Ok(receipt)
    }

    pub fn cleanup_pending(&self) -> bool {
        !self.pending.is_empty()
    }

    pub fn poll_cleanup(&mut self) -> Result<bool, ProcessError> {
        let mut index = 0;
        while index < self.pending.len() {
            match self.pending[index].try_stop_and_reap() {
                Ok(Some(_)) => {
                    drop(self.pending.swap_remove(index));
                }
                Ok(None) => {
                    index += 1;
                }
                Err(_) => return Err(ProcessError::CleanupPending),
            }
        }
        Ok(self.pending.is_empty())
    }

    #[cfg(unix)]
    #[allow(clippy::too_many_arguments)]
    fn controlled_command(
        &mut self,
        policy: &SandboxPolicy,
        stage: &Path,
        program: &Path,
        arguments: &[String],
        seconds: u32,
        output_bytes: u32,
        cancelled: &mut impl FnMut() -> bool,
    ) -> Result<(ProcessReceipt, String), ProcessError> {
        let start = Instant::now();
        let limit = Duration::from_secs(seconds.into());
        let mut stopped = false;
        let mut outcome = polaris_sandbox::run_confined_controlled_in_dir(
            policy,
            program,
            arguments,
            None,
            Some(stage),
            || {
                stopped |= cancelled() || start.elapsed() >= limit;
                stopped
            },
        )
        .map_err(|_| ProcessError::Sandbox)?;
        if let Some(pending) = outcome.pending.take() {
            self.pending.push(pending);
        }
        if self.cleanup_pending() {
            return Err(ProcessError::CleanupPending);
        }
        if stopped || outcome.end != ControlledEnd::Exited {
            return Err(ProcessError::Cancelled);
        }
        if outcome.stdout_truncated
            || outcome.stderr_truncated
            || !outcome.stdout_eof
            || !outcome.stderr_eof
            || outcome.stdout.len() > output_bytes as usize
            || outcome.stderr.len() > output_bytes as usize
            || outcome.problem.is_some()
        {
            return Err(ProcessError::Failed);
        }
        if !outcome.status.is_some_and(|status| status.success()) {
            let diagnostic = outcome
                .stderr
                .lines()
                .filter(|line| !line.trim().is_empty())
                .take(6)
                .collect::<Vec<_>>()
                .join(" | ")
                .chars()
                .filter(|character| !character.is_control())
                .take(1024)
                .collect();
            return Err(ProcessError::CommandFailed {
                exit_code: outcome
                    .status
                    .and_then(|status| status.code())
                    .unwrap_or(-1),
                diagnostic,
            });
        }
        Ok((
            ProcessReceipt {
                exit_code: outcome
                    .status
                    .and_then(|status| status.code())
                    .unwrap_or(-1),
                stdout_sha256: format!("{:x}", Sha256::digest(outcome.stdout.as_bytes())),
                stderr_sha256: format!("{:x}", Sha256::digest(outcome.stderr.as_bytes())),
            },
            outcome.stdout,
        ))
    }
}

fn matches_success(success: &crate::recipes::Success, stdout: &str) -> bool {
    match success {
        crate::recipes::Success::ExitZero => true,
        crate::recipes::Success::Stdout { expected } => stdout.trim() == expected,
        crate::recipes::Success::VersionToken { prefix, version } => stdout
            .trim()
            .strip_prefix(prefix)
            .and_then(|value| value.split_whitespace().next())
            .is_some_and(|value| value == version),
    }
}

#[cfg(unix)]
fn within_disk_budget(root: &Path, maximum: u64, entries: u32) -> bool {
    let mut pending = vec![root.to_owned()];
    let mut count = 0u32;
    let mut bytes = 0u64;
    while let Some(path) = pending.pop() {
        count = match count.checked_add(1) {
            Some(count) if count <= entries => count,
            _ => return false,
        };
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            // Package managers atomically rename and remove private scratch
            // entries while running; an already removed file consumes no disk.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(_) => return false,
        };
        if metadata.is_file() {
            bytes = match bytes.checked_add(metadata.len()) {
                Some(bytes) if bytes <= maximum => bytes,
                _ => return false,
            };
        } else if metadata.is_dir() {
            let children = match fs::read_dir(&path) {
                Ok(children) => children,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return false,
            };
            for child in children {
                let Ok(child) = child else { return false };
                if pending.len() >= entries as usize {
                    return false;
                }
                pending.push(child.path());
            }
        } else if !metadata.file_type().is_symlink() {
            return false;
        }
    }
    true
}
