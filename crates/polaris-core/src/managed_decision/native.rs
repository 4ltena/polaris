//! Bind a package-approved subject to fixed private generation paths, lazily.
use super::*;
use crate::{
    clef::{CapacityAdmission, ClefConfig, ClefDecider, ClefDevice, RUNTIME_REVISION},
    decision_capacity::{
        CapacityPolicy, ComputeKind, MemoryKind, MemoryPeak, MemoryTarget, PeakEvidence,
        ProfileMemory,
    },
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeSet, fs::File, os::unix::fs::MetadataExt, path::PathBuf, time::Duration,
};

const LOCK: &[u8] = include_bytes!(
    "../../../../apps/macos/Sources/PolarisDesktop/Resources/decision-runtime-lock-v1.json"
);

pub(super) mod download;
pub(super) mod install;
pub(super) mod plan;
pub(super) mod verification;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovedLock {
    schema_version: u32,
    profiles: Vec<ApprovedProfile>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ApprovedProfile {
    subject: Subject,
    acceptance: Acceptance,
    delivery: Option<super::DeliveryMetadata>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Acceptance {
    subject_sha256: String,
    evidence_sha256: String,
}

/// Serialize in this field order when producing the acceptance subject hash.
/// Neither an adjacent install receipt nor a project file establishes trust.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Subject {
    id: String,
    generation: String,
    platform: String,
    architecture: String,
    model_repository: String,
    model_revision: String,
    quantization: String,
    runtime_revision: String,
    protocol: u32,
    worker: LockedFile,
    model: LockedFile,
    device: Device,
    context_tokens: u32,
    request_timeout_ms: u64,
    load_timeout_ms: u64,
    peak_bytes: u64,
    peak_evidence: PeakKind,
    reserve_bytes: u64,
}

#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct LockedFile {
    bytes: u64,
    sha256: String,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum Device {
    Cpu,
    Metal,
}

#[derive(Clone, Copy, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum PeakKind {
    Measured,
    ValidatedUpperBound,
}

pub(super) fn build(profile: &str) -> Result<ClefDecider, ManagedDecisionError> {
    let config = prepare(LOCK, profile, || {
        crate::os_home::trusted_user_home().map_err(|_| ManagedDecisionError::NotInstalled)
    })?;
    ClefDecider::new(config).map_err(|_| ManagedDecisionError::Integrity)
}

fn digest(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}

fn text(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

fn select(bytes: &[u8], id: &str) -> Result<Subject, ManagedDecisionError> {
    select_profile(bytes, id).map(|p| p.subject)
}

fn select_profile(bytes: &[u8], id: &str) -> Result<ApprovedProfile, ManagedDecisionError> {
    use ManagedDecisionError::Integrity;
    if bytes.len() > 128 * 1024 || !valid_profile_id(id) {
        return Err(Integrity);
    }
    let lock: ApprovedLock = serde_json::from_slice(bytes).map_err(|_| Integrity)?;
    if lock.schema_version != 1 || lock.profiles.len() > 64 {
        return Err(Integrity);
    }
    let mut seen = BTreeSet::new();
    let mut selected = None;
    for profile in lock.profiles {
        let s = &profile.subject;
        if !valid_profile_id(&s.id)
            || !valid_profile_id(&s.generation)
            || !seen.insert((s.id.clone(), s.platform.clone(), s.architecture.clone()))
            || !digest(&profile.acceptance.evidence_sha256)
            || !digest(&profile.acceptance.subject_sha256)
            || format!(
                "{:x}",
                Sha256::digest(serde_json::to_vec(s).map_err(|_| Integrity)?)
            ) != profile.acceptance.subject_sha256
            || !text(&s.model_repository)
            || !text(&s.quantization)
            || s.model_revision.len() != 40
            || !s
                .model_revision
                .bytes()
                .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
            || s.runtime_revision != RUNTIME_REVISION
            || s.protocol != 1
            || !digest(&s.worker.sha256)
            || !digest(&s.model.sha256)
            || s.worker.bytes == 0
            || s.worker.bytes > 128 * 1024 * 1024
            || s.model.bytes == 0
            || s.model.bytes > 64 * 1024 * 1024 * 1024
            || !(1024..=16384).contains(&s.context_tokens)
            || !(1000..=120000).contains(&s.request_timeout_ms)
            || !(1000..=300000).contains(&s.load_timeout_ms)
            || s.peak_bytes == 0
            || s.peak_bytes.checked_add(s.reserve_bytes).is_none()
        {
            return Err(Integrity);
        }
        let architecture = if cfg!(target_arch = "aarch64") {
            "arm64"
        } else {
            "x86_64"
        };
        if s.id == id && s.platform == "macos" && s.architecture == architecture {
            selected = Some(profile);
        }
    }
    selected.ok_or(ManagedDecisionError::UnverifiedProfile)
}

fn prepare(
    bytes: &[u8],
    id: &str,
    home: impl FnOnce() -> Result<PathBuf, ManagedDecisionError>,
) -> Result<ClefConfig, ManagedDecisionError> {
    prepare_with_inspection(bytes, id, home, |_, _, _| Ok(()))
}

fn prepare_with_inspection(
    bytes: &[u8],
    id: &str,
    home: impl FnOnce() -> Result<PathBuf, ManagedDecisionError>,
    mut inspect: impl FnMut(&File, &std::path::Path, &LockedFile) -> Result<(), ManagedDecisionError>,
) -> Result<ClefConfig, ManagedDecisionError> {
    use ManagedDecisionError::Integrity;
    let s = select(bytes, id)?;
    // Metal is selectable only after its per-device observation exists. Never
    // fall back to a CPU profile or borrow its RSS as GPU memory evidence.
    if matches!(s.device, Device::Metal) {
        return Err(ManagedDecisionError::UnsupportedDevice);
    }
    let home = home()?;
    if !home.is_absolute() || home.canonicalize().ok().as_ref() != Some(&home) {
        return Err(Integrity);
    }
    let mut held = Vec::new();
    let opened = crate::project_mutation::open_directory_nofollow(&home).map_err(|_| Integrity)?;
    let metadata = opened.metadata().map_err(|_| Integrity)?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o7022 != 0 {
        return Err(Integrity);
    }
    held.push((home.clone(), opened));
    for component in [".polaris", "decision-runtime"] {
        descend(&mut held, component)?;
    }
    let runtime = held.last().ok_or(Integrity)?.0.clone();
    descend(&mut held, "generations")?;
    descend(&mut held, &s.generation)?;
    let (generation, directory) = held.last().ok_or(Integrity)?;
    let worker = generation.join("worker");
    let model = generation.join("model.gguf");
    let mut held_files = Vec::new();
    for (path, name, expected, executable) in [
        (&worker, "worker", &s.worker, true),
        (&model, "model.gguf", &s.model, false),
    ] {
        let before = path.symlink_metadata().map_err(io_error)?;
        let file = polaris_tools::path_policy::tree_admission::open_at(
            directory,
            name.as_ref(),
            false,
            path,
        )
        .map_err(|_| Integrity)?;
        let metadata = file.metadata().map_err(|_| Integrity)?;
        if !metadata.is_file()
            || metadata.nlink() != 1
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o7077 != 0
            || metadata.len() != expected.bytes
            || (metadata.dev(), metadata.ino()) != (before.dev(), before.ino())
            || (executable && metadata.mode() & 0o100 == 0)
        {
            return Err(Integrity);
        }
        inspect(&file, path, expected)?;
        held_files.push((path.clone(), file, metadata));
        // The existing worker snapshot/native model FD checks validate full
        // hashes before use. Avoid hashing gigabytes during ordinary startup.
    }
    let root = runtime.join("runs");
    let runs = crate::project_mutation::open_directory_nofollow(&root).map_err(io_error)?;
    private(&runs)?;
    held.push((root.clone(), runs));
    // Keep both descriptors pinned until every digest is complete. A later
    // inspection may not replace an earlier leaf and still produce a report.
    for (path, file, before) in &held_files {
        let after = file.metadata().map_err(|_| Integrity)?;
        let current = path.symlink_metadata().map_err(|_| Integrity)?;
        if !same_file_metadata(before, &after) || !same_file_metadata(&after, &current) {
            return Err(Integrity);
        }
    }
    for (path, file) in &held {
        let current = path.symlink_metadata().map_err(|_| Integrity)?;
        let pinned = file.metadata().map_err(|_| Integrity)?;
        if !current.is_dir() || (current.dev(), current.ino()) != (pinned.dev(), pinned.ino()) {
            return Err(Integrity);
        }
    }
    let memory = if cfg!(target_arch = "aarch64") {
        MemoryKind::UnifiedMemory
    } else {
        MemoryKind::SystemRam
    };
    Ok(ClefConfig {
        model_identity: Some(crate::clef::ClefModelIdentity {
            repository: s.model_repository.clone(),
            revision: s.model_revision.clone(),
            quantization: s.quantization.clone(),
        }),
        worker,
        worker_sha256: s.worker.sha256,
        model,
        model_bytes: s.model.bytes,
        model_sha256: s.model.sha256,
        generation: s.generation,
        device: ClefDevice::Cpu,
        context_tokens: s.context_tokens,
        request_timeout_ms: s.request_timeout_ms,
        load_timeout_ms: s.load_timeout_ms,
        private_root: root,
        admission: Some(CapacityAdmission {
            profile: ProfileMemory {
                accepted: true,
                target: MemoryTarget {
                    device_id: "host-cpu".into(),
                    memory,
                    compute: ComputeKind::Cpu,
                },
                max_context_tokens: s.context_tokens,
                max_parallel: 1,
                peak: Some(MemoryPeak {
                    bytes: s.peak_bytes,
                    evidence: match s.peak_evidence {
                        PeakKind::Measured => PeakEvidence::Measured,
                        PeakKind::ValidatedUpperBound => PeakEvidence::ValidatedUpperBound,
                    },
                }),
            },
            policy: CapacityPolicy {
                reserve_bytes: s.reserve_bytes,
                colocated_bytes: 0,
                budget_limit_bytes: None,
                max_age: Duration::from_secs(15),
            },
        }),
    })
}

fn same_file_metadata(a: &std::fs::Metadata, b: &std::fs::Metadata) -> bool {
    let stamp = |m: &std::fs::Metadata| {
        (
            m.dev(),
            m.ino(),
            m.len(),
            m.mode(),
            m.uid(),
            m.nlink(),
            m.mtime(),
            m.mtime_nsec(),
            m.ctime(),
            m.ctime_nsec(),
        )
    };
    stamp(a) == stamp(b)
}

fn io_error(error: std::io::Error) -> ManagedDecisionError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ManagedDecisionError::NotInstalled
    } else {
        ManagedDecisionError::Integrity
    }
}

fn private(file: &File) -> Result<(), ManagedDecisionError> {
    let m = file
        .metadata()
        .map_err(|_| ManagedDecisionError::Integrity)?;
    if !m.is_dir() || m.uid() != unsafe { libc::geteuid() } || m.mode() & 0o7077 != 0 {
        return Err(ManagedDecisionError::Integrity);
    }
    Ok(())
}

fn descend(held: &mut Vec<(PathBuf, File)>, name: &str) -> Result<(), ManagedDecisionError> {
    let (path, parent) = held.last().ok_or(ManagedDecisionError::Integrity)?;
    let path = path.join(name);
    let before = path.symlink_metadata().map_err(io_error)?;
    let file =
        polaris_tools::path_policy::tree_admission::open_at(parent, name.as_ref(), true, &path)
            .map_err(|_| ManagedDecisionError::Integrity)?;
    private(&file)?;
    let pinned = file
        .metadata()
        .map_err(|_| ManagedDecisionError::Integrity)?;
    if (before.dev(), before.ino()) != (pinned.dev(), pinned.ino()) {
        return Err(ManagedDecisionError::Integrity);
    }
    held.push((path, file));
    Ok(())
}

#[cfg(test)]
mod tests;
