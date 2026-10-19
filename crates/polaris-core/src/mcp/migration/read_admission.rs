//! Live private guard binding around MCP's non-authoritative read observations.
use super::entry_gate::{EntryAnchorV2, EntryMigrationGuardV2};
use polaris_mcp::{
    Error,
    migration_observation::{
        CompleteTreeProjectionV2, MappedStagingViewV2, MigrationParentObservationV2,
        StagingRecordV2, StoreMigrationRecordV2,
    },
    registration::Registry,
    store::documents_v2::RegistryDocumentV2,
};
use std::{collections::BTreeMap, fs::File};

pub(super) fn inspect_mapped_stage<'a>(
    guard: &EntryMigrationGuardV2,
    record: &'a StagingRecordV2,
    inventory: &'a CompleteTreeProjectionV2,
) -> Result<MappedStagingViewV2<'a>, Error> {
    record.validate()?;
    let verify = || -> Result<(), Error> {
        let (_, id, manifest, hash) = guard.verified_binding().map_err(protection)?;
        let binding = &record.retired.binding;
        if id != binding.migration_id
            || hash != binding.entry_manifest_hash
            || *manifest != binding.entry_manifest
            || !same_parent(&record.canonical_parent_anchor, &anchor(&guard.parent)?)
        {
            return Err(Error::Protection);
        }
        Ok(())
    };
    verify()?;
    let (home, _, _, _) = guard.verified_binding().map_err(protection)?;
    let namespace = MigrationParentObservationV2::inspect(home, &guard.parent)?;
    let view = MappedStagingViewV2::inspect_descriptor(namespace, record, inventory)?;
    observation_complete();
    verify()?;
    Ok(view)
}

/// The returned success is a read admission only. The physical owner must hold
/// every observation and separately establish fresh native zero before mutation.
pub(super) fn validate_unregistered_pins(
    guard: &EntryMigrationGuardV2,
    record: &StoreMigrationRecordV2,
    source_registry: Option<&Registry>,
    registry: &RegistryDocumentV2,
    pins: &BTreeMap<String, (&File, &File)>,
) -> Result<(), Error> {
    record.validate()?;
    let verify = || -> Result<(), Error> {
        let (_, id, _, hash) = guard.verified_binding().map_err(protection)?;
        if id != record.binding.migration_id
            || hash != record.binding.entry_manifest_hash
            || !same_parent(
                &record.binding.canonical_parent_anchor,
                &anchor(&guard.parent)?,
            )
        {
            return Err(Error::Protection);
        }
        Ok(())
    };
    verify()?;
    let (home, _, _, _) = guard.verified_binding().map_err(protection)?;
    let namespace = MigrationParentObservationV2::inspect(home, &guard.parent)?;
    record.validate_unregistered_pin_descriptors(&namespace, source_registry, registry, pins)?;
    observation_complete();
    verify()
}

pub(super) fn validate_unregistered_pin_reader(
    guard: &EntryMigrationGuardV2,
    record: &StoreMigrationRecordV2,
    source_registry: Option<&Registry>,
    registry: &RegistryDocumentV2,
    pins: impl FnMut(&str) -> Result<(File, File), Error>,
) -> Result<(), Error> {
    record.validate()?;
    let verify = || -> Result<(), Error> {
        let (_, id, _, hash) = guard.verified_binding().map_err(protection)?;
        if id != record.binding.migration_id
            || hash != record.binding.entry_manifest_hash
            || !same_parent(
                &record.binding.canonical_parent_anchor,
                &anchor(&guard.parent)?,
            )
        {
            return Err(Error::Protection);
        }
        Ok(())
    };
    verify()?;
    let (home, _, _, _) = guard.verified_binding().map_err(protection)?;
    let namespace = MigrationParentObservationV2::inspect(home, &guard.parent)?;
    record.validate_unregistered_pin_reader(&namespace, source_registry, registry, pins)?;
    observation_complete();
    verify()
}

fn anchor(file: &File) -> Result<EntryAnchorV2, Error> {
    Ok(EntryAnchorV2::of(
        &file.metadata().map_err(|_| Error::Storage)?,
    ))
}
fn same_parent(a: &EntryAnchorV2, b: &EntryAnchorV2) -> bool {
    (a.device, a.inode, a.uid, a.mode) == (b.device, b.inode, b.uid, b.mode)
}
fn protection<T>(_: T) -> Error {
    Error::Protection
}
fn observation_complete() {
    #[cfg(test)]
    AFTER_OBSERVATION.with(|slot| {
        if let Some(action) = slot.borrow_mut().take() {
            action();
        }
    });
}
#[cfg(test)]
thread_local! {
    static AFTER_OBSERVATION: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
mod tests;
