//! Explicit full-digest check of an existing pinned generation; no mutation or launch.
use super::*;
use std::{io::Read, path::Path};

pub(in crate::managed_decision) fn verify(
    id: &str,
    cancelled: impl FnMut() -> bool,
) -> Result<super::super::VerifiedGeneration, ManagedDecisionError> {
    verify_with(
        LOCK,
        id,
        || crate::os_home::trusted_user_home().map_err(|_| ManagedDecisionError::NotInstalled),
        cancelled,
    )
}

fn verify_with(
    bytes: &[u8],
    id: &str,
    home: impl FnOnce() -> Result<PathBuf, ManagedDecisionError>,
    mut cancelled: impl FnMut() -> bool,
) -> Result<super::super::VerifiedGeneration, ManagedDecisionError> {
    if cancelled() {
        return Err(ManagedDecisionError::Cancelled);
    }
    let mut verified_bytes = 0u64;
    let config = prepare_with_inspection(bytes, id, home, |file, path, expected| {
        verify_file(file, path, expected, &mut cancelled)?;
        verified_bytes = verified_bytes
            .checked_add(expected.bytes)
            .ok_or(ManagedDecisionError::Integrity)?;
        Ok(())
    })?;
    if cancelled() {
        return Err(ManagedDecisionError::Cancelled);
    }
    let labels = config
        .model_identity
        .ok_or(ManagedDecisionError::Integrity)?;
    Ok(super::super::VerifiedGeneration {
        profile: id.into(),
        generation: config.generation,
        model_repository: labels.repository,
        model_revision: labels.revision,
        quantization: labels.quantization,
        runtime_revision: RUNTIME_REVISION.into(),
        worker_sha256: config.worker_sha256,
        model_sha256: config.model_sha256,
        verified_bytes,
    })
}

fn verify_file(
    mut file: &File,
    path: &Path,
    expected: &LockedFile,
    cancelled: &mut impl FnMut() -> bool,
) -> Result<(), ManagedDecisionError> {
    use ManagedDecisionError::{Cancelled, Integrity};
    let before = file.metadata().map_err(|_| Integrity)?;
    let mut hash = Sha256::new();
    let mut count = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        if cancelled() {
            return Err(Cancelled);
        }
        let size = file.read(&mut buffer).map_err(|_| Integrity)?;
        if size == 0 {
            break;
        }
        count = count.checked_add(size as u64).ok_or(Integrity)?;
        if count > expected.bytes {
            return Err(Integrity);
        }
        hash.update(&buffer[..size]);
    }
    let after = file.metadata().map_err(|_| Integrity)?;
    let current = path.symlink_metadata().map_err(|_| Integrity)?;
    if count != expected.bytes
        || format!("{:x}", hash.finalize()) != expected.sha256
        || !same_file_metadata(&before, &after)
        || !same_file_metadata(&after, &current)
    {
        return Err(Integrity);
    }
    Ok(())
}

#[cfg(test)]
mod tests;
