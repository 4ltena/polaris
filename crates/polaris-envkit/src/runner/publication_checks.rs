//! Publication snapshot checks without changing the public runner entry point.
use super::*;

pub fn validate_publishing_snapshot(
    plan: &ResolvedPlan,
    live: &Snapshot,
    postimages: &BTreeMap<String, Inventory>,
) -> Result<(), Error> {
    let before = &plan.snapshot;
    if live.target != before.target
        || live.private_root != before.private_root
        || live.runner_binary_sha256 != before.runner_binary_sha256
        || live.host_os_revision != before.host_os_revision
        || live.platform != before.platform
        || live.host_prerequisites != before.host_prerequisites
    {
        return Err(Error::StalePlan);
    }
    for original in before.inputs.values() {
        if !live.inputs.values().any(|current| current == original) {
            return Err(Error::StalePlan);
        }
    }
    for (path, hash) in &before.installations {
        if live.installations.get(path) != Some(hash) {
            return Err(Error::StalePlan);
        }
    }
    for (path, hash) in &live.installations {
        if before.installations.get(path) == Some(hash) {
            continue;
        }
        if postimages
            .get(path)
            .and_then(|image| image.digest().ok())
            .as_ref()
            != Some(hash)
        {
            return Err(Error::StalePlan);
        }
    }
    for current in live.inputs.values() {
        if before.inputs.values().any(|original| current == original) {
            continue;
        }
        let Some(image) = postimages.get(&current.relative) else {
            return Err(Error::StalePlan);
        };
        if !matches!(image.entries.as_slice(), [publication::ImageEntry::File { sha256, .. }] if sha256 == &current.sha256)
        {
            return Err(Error::StalePlan);
        }
    }
    Ok(())
}
