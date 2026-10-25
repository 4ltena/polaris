//! Reconcile retained evidence without transferring, spawning or publishing.
use crate::management_effect::*;
use polaris_envkit::{
    Stage, adapter::ResolvedPlan, definition::Definition, journal::Journal, publication,
    runner::Receipt,
};
use serde::Serialize;
use std::{fs, io::Read, path::Path};

#[derive(Debug, Serialize)]
pub struct Report {
    pub plan_sha256: String,
    pub stage: Stage,
    pub all_outputs_match: bool,
    pub conflicts: Vec<String>,
    pub pending: Vec<String>,
    pub receipt_sha256: Option<String>,
    pub outcome: String,
}

pub fn inspect(project: &Path, state: &Path, root: &Path) -> Result<Report, String> {
    let (definition, plan, journal) = load(project, state, root)?;
    let digest = plan.digest(&definition).map_err(text)?;
    let mut conflicts = Vec::new();
    if plan.snapshot.runner_binary_sha256 != super::owner::binary_digest()?
        || plan.snapshot.host_os_revision != super::owner::os_revision()?
        || plan.snapshot.host_prerequisites
            != super::native::refresh(&plan.snapshot.host_prerequisites, || false)?
    {
        conflicts.push("Runner, OS or native prerequisite identity changed".into());
    }
    let mut pending = Vec::new();
    let mut evidence = None;
    let verified = if journal.progress().stage == Stage::Verified {
        let bytes = read_private(&root.join("receipt.json"), 32 * 1024 * 1024)?;
        let receipt: Receipt = serde_json::from_slice(&bytes).map_err(text)?;
        if receipt.schema != 2
            || receipt.plan_sha256 != digest
            || receipt.candidate != journal.progress().candidate
        {
            return Err("Environment receipt binding changed".into());
        }
        match super::owner::snapshot(
            &definition,
            &plan,
            super::native::refresh(&plan.snapshot.host_prerequisites, || false)?,
        ) {
            Ok(live)
                if polaris_envkit::runner::validate_publishing_snapshot(
                    &plan,
                    &live,
                    &receipt.postimages,
                )
                .is_ok() => {}
            _ => conflicts.push("Target identity or admitted source bytes changed".into()),
        }
        let candidate = plan
            .candidates
            .iter()
            .find(|candidate| candidate.index == receipt.candidate)
            .ok_or("Unknown environment candidate")?;
        if receipt.probes.len() != candidate.commands.len()
            || receipt.probes.iter().any(|probe| {
                probe.exit_code != 0
                    || !valid_digest(&probe.stdout_sha256)
                    || !valid_digest(&probe.stderr_sha256)
            })
        {
            return Err("Environment process evidence is incomplete".into());
        }
        if receipt.postimages.len() != candidate.paths.publish_target.len() {
            return Err("Incomplete environment postimage inventory".into());
        }
        for binding in candidate.paths.publish_target.values() {
            let expected = receipt
                .postimages
                .get(&binding.relative)
                .ok_or("Incomplete environment postimage inventory")?;
            match publication::inspect(
                &Path::new(&plan.snapshot.target.canonical_path).join(&binding.relative),
                &definition.limits,
            ) {
                Ok(actual) if &actual == expected => {}
                _ => conflicts.push(binding.relative.clone()),
            }
        }
        let layout =
            polaris_envkit::invocation::Layout::for_plan(&plan, receipt.candidate).map_err(text)?;
        if receipt.runtime_images.len() != candidate.paths.private_stable.len() {
            return Err("Incomplete environment runtime inventory".into());
        }
        for runtime in &candidate.paths.private_stable {
            let path = layout.runtime(runtime);
            let path_string = path.to_str().ok_or("Invalid environment runtime path")?;
            let expected = receipt
                .runtime_images
                .get(path_string)
                .ok_or("Incomplete environment runtime inventory")?;
            if publication::inspect(&path, &definition.limits)
                .as_ref()
                .ok()
                != Some(expected)
            {
                conflicts.push(path_string.into());
            }
        }
        evidence = Some(ManagementEffectHashV1::digest(&bytes).as_hex());
        conflicts.is_empty()
    } else {
        for candidate in &plan.candidates {
            if candidate.index != journal.progress().candidate {
                continue;
            }
            for (id, binding) in &candidate.paths.publish_target {
                if root.join(format!("publish_{}.receipt.json", id.0)).exists() {
                    pending.push(format!(
                        "{}: publication receipt present; final verification incomplete",
                        binding.relative
                    ));
                } else {
                    pending.push(format!(
                        "{}: no completed publication receipt",
                        binding.relative
                    ));
                }
            }
        }
        false
    };
    let failed = root.join("failure.json").exists();
    if failed {
        let failure: serde_json::Value =
            serde_json::from_slice(&read_private(&root.join("failure.json"), 65536)?)
                .map_err(text)?;
        if failure
            .get("plan_sha256")
            .and_then(serde_json::Value::as_str)
            != Some(digest.as_str())
            || failure.get("outcome").and_then(serde_json::Value::as_str) != Some("failed")
            || failure
                .get("cleanup_confirmed")
                .and_then(serde_json::Value::as_bool)
                != Some(true)
            || !matches!(
                journal.progress().stage,
                Stage::Prepared | Stage::Started | Stage::Staged
            )
        {
            return Err("Environment failure evidence changed".into());
        }
        pending.clear();
    }
    Ok(Report {
        plan_sha256: digest,
        stage: journal.progress().stage,
        all_outputs_match: verified,
        conflicts,
        pending,
        receipt_sha256: evidence,
        outcome: if failed {
            "failed"
        } else if verified {
            "verified"
        } else {
            "unresolved"
        }
        .into(),
    })
}

/// Only close a verified operation whose final management append was lost.
/// Partial publication remains unknown; this method never repeats a step.
pub fn reconcile(
    project: &Path,
    state: &Path,
    root: &Path,
    store: &ManagementEffectStoreV1,
    lease: &crate::project_mutation::ProjectMutationLeaseV1,
) -> Result<Report, String> {
    let report = inspect(project, state, root)?;
    if !report.all_outputs_match {
        return Err("Environment evidence is incomplete or changed; no steps were repeated".into());
    }
    let hash = ManagementEffectHashV1::from_hex(&report.plan_sha256).map_err(text)?;
    let evidence = ManagementEffectHashV1::from_hex(
        report
            .receipt_sha256
            .as_deref()
            .ok_or("Missing environment receipt")?,
    )
    .map_err(text)?;
    let matches: Vec<_> = store
        .effects()
        .map_err(text)?
        .into_iter()
        .filter(|effect| {
            effect.request.inventory.encoded() == "C43"
                && effect.request.request_hash == hash
                && matches!(
                    effect.state,
                    ManagementEffectStateV1::Completed { .. }
                        | ManagementEffectStateV1::OutcomeUnknown { .. }
                )
        })
        .collect();
    if matches.len() != 1 {
        return Err("No unique environment management effect matches this receipt".into());
    }
    let effect = &matches[0];
    match &effect.state {
        ManagementEffectStateV1::Completed { evidence_hash } if *evidence_hash == evidence => {}
        ManagementEffectStateV1::OutcomeUnknown { .. } => {
            let unknown = store
                .unknown_effect(effect.request.operation_id)
                .map_err(text)?
                .ok_or("Environment outcome changed")?;
            store
                .complete_unknown(lease, &unknown, evidence)
                .map_err(text)?;
        }
        _ => return Err("Environment management evidence does not permit completion".into()),
    }
    Ok(report)
}

pub(super) fn load(
    project: &Path,
    state: &Path,
    root: &Path,
) -> Result<(Definition, ResolvedPlan, Journal), String> {
    use std::os::unix::fs::MetadataExt;
    let physical_state = state.canonicalize().map_err(text)?;
    if root.canonicalize().map_err(text)? != root
        || root.parent() != Some(physical_state.as_path())
        || !root
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with("envkit-"))
    {
        return Err("Environment state is outside the project operation store".into());
    }
    let owner: ManagementEffectHashV1 =
        serde_json::from_slice(&read_private(&root.join("owner.json"), 1024)?).map_err(text)?;
    if owner != ManagementEffectHashV1::digest(project.as_os_str().as_encoded_bytes()) {
        return Err("Environment project scope changed".into());
    }
    let plan: ResolvedPlan =
        serde_json::from_slice(&read_private(&root.join("plan.json"), 2 * 1024 * 1024)?)
            .map_err(text)?;
    let definition = polaris_envkit::bundled::load(plan.profile).map_err(text)?;
    let saved: Definition = serde_json::from_slice(&read_private(
        &root.join("definition.json"),
        4 * 1024 * 1024,
    )?)
    .map_err(text)?;
    if saved != definition || !Path::new(&plan.snapshot.target.canonical_path).starts_with(project)
    {
        return Err("Environment definition or target scope changed".into());
    }
    let (directory, _) =
        polaris_tools::path_policy::tree_admission::open_root_with_ancestors(root).map_err(text)?;
    let metadata = directory.metadata().map_err(text)?;
    if (metadata.dev(), metadata.ino())
        != (
            plan.snapshot.private_root.device,
            plan.snapshot.private_root.inode,
        )
        || plan.snapshot.private_root.canonical_path
            != root.to_str().ok_or("Invalid environment state path")?
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err("Environment operation directory identity changed".into());
    }
    let digest = plan.digest(&definition).map_err(text)?;
    let journal = Journal::open(root, &digest).map_err(text)?;
    Ok((definition, plan, journal))
}

pub(super) fn read_private(path: &Path, maximum: u64) -> Result<Vec<u8>, String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let file = fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .map_err(text)?;
    let metadata = file.metadata().map_err(text)?;
    if !metadata.is_file()
        || metadata.nlink() != 1
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
        || metadata.len() > maximum
    {
        return Err("Unsafe environment evidence file".into());
    }
    let mut bytes = Vec::new();
    file.take(maximum + 1)
        .read_to_end(&mut bytes)
        .map_err(text)?;
    if bytes.len() as u64 > maximum {
        return Err("Environment evidence exceeds its limit".into());
    }
    Ok(bytes)
}
fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn valid_digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}
