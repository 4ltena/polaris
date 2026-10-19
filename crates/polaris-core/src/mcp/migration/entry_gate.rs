//! Exclusive migration entry-gate ownership and retained home binding.
pub(super) use polaris_tools::entry_kernel::*;
use serde::Serialize;
use std::{
    fs::File,
    io::Write,
    os::fd::AsRawFd,
    path::{Path, PathBuf},
};
fn storage(_: std::io::Error) -> EntryGateErrorV2 {
    EntryGateErrorV2::Storage
}
/// Root-owned migration admission. It never opens an MCP document, retires a
/// root, or activates V2 by itself; those steps belong to the protocol owner.
pub(crate) struct EntryMigrationGuardV2 {
    pub(crate) parent: File,
    _lock: File,
    home: PathBuf,
    manifest: ProductionEntryManifestV2,
    migration_id: String,
    manifest_hash: String,
    lock_anchor: EntryAnchorV2,
    parent_anchor: EntryAnchorV2,
    home_guard: Option<std::sync::Arc<super::account_home::TrustedUserHomeV2>>,
}

impl EntryMigrationGuardV2 {
    pub(crate) fn begin_trusted(
        home: std::sync::Arc<super::account_home::TrustedUserHomeV2>,
        migration_id: &str,
        manifest: ProductionEntryManifestV2,
    ) -> Result<Self, EntryGateErrorV2> {
        let path = home
            .verified_path()
            .map_err(|_| EntryGateErrorV2::Identity)?
            .to_owned();
        let file = home
            .verified_directory()
            .map_err(|_| EntryGateErrorV2::Identity)?;
        Self::begin_with_home(&path, file, Some(home), migration_id, manifest)
    }
    #[cfg(test)]
    pub(crate) fn begin(
        home: &Path,
        migration_id: &str,
        manifest: ProductionEntryManifestV2,
    ) -> Result<Self, EntryGateErrorV2> {
        Self::begin_with_home(
            home,
            physical_directory(home)?,
            None,
            migration_id,
            manifest,
        )
    }
    fn begin_with_home(
        home: &Path,
        home_fd: File,
        home_guard: Option<std::sync::Arc<super::account_home::TrustedUserHomeV2>>,
        migration_id: &str,
        manifest: ProductionEntryManifestV2,
    ) -> Result<Self, EntryGateErrorV2> {
        if !valid_digest(migration_id) {
            return Err(EntryGateErrorV2::InvalidGate);
        }
        let manifest_hash = manifest.canonical_hash()?;
        owned_directory(&home_fd, false)?;
        if let Some(home) = &home_guard {
            home.verify_current()
                .map_err(|_| EntryGateErrorV2::Identity)?;
        }
        let parent = match child(&home_fd, ".polaris", libc::O_RDONLY | libc::O_DIRECTORY) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if unsafe { libc::mkdirat(home_fd.as_raw_fd(), c".polaris".as_ptr(), 0o700) } != 0 {
                    return Err(EntryGateErrorV2::Storage);
                }
                home_fd.sync_all().map_err(storage)?;
                child(&home_fd, ".polaris", libc::O_RDONLY | libc::O_DIRECTORY).map_err(storage)?
            }
            result => result.map_err(storage)?,
        };
        owned_directory(&parent, false)?;
        if let Some(home) = &home_guard {
            home.verify_parent(&parent)
                .map_err(|_| EntryGateErrorV2::Identity)?;
        }
        let parent_anchor = EntryAnchorV2::of(&parent.metadata().map_err(storage)?);
        let lock = child(&parent, GATE_LOCK, libc::O_RDWR | libc::O_CREAT).map_err(storage)?;
        private_file(&lock)?;
        lock_exclusive(&lock)?;
        lock.sync_all().map_err(storage)?;
        parent.sync_all().map_err(storage)?;
        let lock_anchor = EntryAnchorV2::of(&lock.metadata().map_err(storage)?);
        let guard = Self {
            parent,
            _lock: lock,
            home: home.to_owned(),
            manifest,
            migration_id: migration_id.to_owned(),
            manifest_hash,
            lock_anchor,
            parent_anchor,
            home_guard,
        };
        let expected_gate = ProductionEntryGateV2::MigratingV2 {
            protocol_version: 2,
            migration_id: guard.migration_id.clone(),
            entry_manifest_hash: guard.manifest_hash.clone(),
        };
        if let Some(bytes) = read_optional(&guard.parent, ENTRY_MANIFEST)? {
            let existing: ProductionEntryManifestV2 = decode(&bytes)?;
            if existing != guard.manifest {
                return Err(EntryGateErrorV2::InvalidGate);
            }
        } else {
            write_exclusive(&guard.parent, ENTRY_MANIFEST, &guard.manifest)?;
        }
        if let Some(bytes) = read_optional(&guard.parent, GATE_FILE)? {
            let existing: ProductionEntryGateV2 = decode(&bytes)?;
            if existing != expected_gate {
                return Err(EntryGateErrorV2::InvalidGate);
            }
        } else {
            write_exclusive(&guard.parent, GATE_FILE, &expected_gate)?;
        }
        guard.verify_migrating()?;
        Ok(guard)
    }

    pub(crate) fn verify_migrating(&self) -> Result<(), EntryGateErrorV2> {
        if let Some(home) = &self.home_guard {
            home.verify_current()
                .map_err(|_| EntryGateErrorV2::Identity)?;
        }
        let parent = self.verified_named_parent()?;
        let lock = child(&parent, GATE_LOCK, libc::O_RDONLY).map_err(storage)?;
        private_file(&lock)?;
        if EntryAnchorV2::of(&lock.metadata().map_err(storage)?) != self.lock_anchor {
            return Err(EntryGateErrorV2::InvalidGate);
        }
        let gate: ProductionEntryGateV2 =
            decode(&read_optional(&parent, GATE_FILE)?.ok_or(EntryGateErrorV2::InvalidGate)?)?;
        let manifest: ProductionEntryManifestV2 =
            decode(&read_optional(&parent, ENTRY_MANIFEST)?.ok_or(EntryGateErrorV2::InvalidGate)?)?;
        if gate
            != (ProductionEntryGateV2::MigratingV2 {
                protocol_version: 2,
                migration_id: self.migration_id.clone(),
                entry_manifest_hash: self.manifest_hash.clone(),
            })
            || manifest != self.manifest
            || manifest.canonical_hash()? != self.manifest_hash
        {
            return Err(EntryGateErrorV2::InvalidGate);
        }
        #[cfg(test)]
        if let Some(hook) = AFTER_MIGRATING_CONTENT.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
        let closing = self.verified_named_parent()?;
        let lock = child(&closing, GATE_LOCK, libc::O_RDONLY).map_err(storage)?;
        private_file(&lock)?;
        if EntryAnchorV2::of(&lock.metadata().map_err(storage)?) != self.lock_anchor {
            return Err(EntryGateErrorV2::InvalidGate);
        }
        Ok(())
    }
    fn verified_named_parent(&self) -> Result<File, EntryGateErrorV2> {
        let parent = match &self.home_guard {
            Some(home) => home
                .verified_polaris_parent()
                .map_err(|_| EntryGateErrorV2::Identity)?,
            None => child(
                &physical_directory(&self.home)?,
                ".polaris",
                libc::O_RDONLY | libc::O_DIRECTORY,
            )
            .map_err(storage)?,
        };
        for file in [&self.parent, &parent] {
            owned_directory(file, false)?;
            let anchor = EntryAnchorV2::of(&file.metadata().map_err(storage)?);
            if (anchor.device, anchor.inode, anchor.uid, anchor.mode)
                != (
                    self.parent_anchor.device,
                    self.parent_anchor.inode,
                    self.parent_anchor.uid,
                    self.parent_anchor.mode,
                )
            {
                return Err(EntryGateErrorV2::InvalidGate);
            }
        }
        Ok(parent)
    }

    /// Borrows this live guard's exact binding after rechecking its parent,
    /// admission lock, Migrating gate and physical entry manifest. The tuple
    /// supplies observations only; it cannot replace a fresh stop proof.
    pub(crate) fn verified_binding(
        &self,
    ) -> Result<(&Path, &str, &ProductionEntryManifestV2, &str), EntryGateErrorV2> {
        self.verify_migrating()?;
        Ok((
            self.home.as_path(),
            self.migration_id.as_str(),
            &self.manifest,
            self.manifest_hash.as_str(),
        ))
    }

    pub(crate) fn prove_stopped(&self) -> Result<ZeroProductionProcessesV2, EntryGateErrorV2> {
        self.verify_migrating()?;
        let coordinator =
            ProductionBinaryIdentityV2::inspect(&std::env::current_exe().map_err(storage)?)?;
        let inspected = process_inventory::verify_stop_zero(&self.manifest)?;
        self.verify_migrating()?;
        Ok(ZeroProductionProcessesV2 {
            migration_id: self.migration_id.clone(),
            manifest_hash: self.manifest_hash.clone(),
            coordinator,
            inspected,
        })
    }
}

/// A native complete inventory proof bound to the still-held gate. No boolean,
/// path-only inventory or synthetic observer can construct this capability.
pub(crate) struct ZeroProductionProcessesV2 {
    migration_id: String,
    manifest_hash: String,
    coordinator: ProductionBinaryIdentityV2,
    inspected: usize,
}
impl ZeroProductionProcessesV2 {
    pub(super) fn observation(&self) -> polaris_mcp::migration_observation::StopObservationV2 {
        use polaris_mcp::migration_observation::{StopObservationResultV2, StopObservationV2};
        StopObservationV2 {
            result: StopObservationResultV2::ZeroProductionProcesses,
            coordinator: self.coordinator.clone(),
            inspected_processes: self.inspected as u64,
        }
    }
    pub(crate) fn verify_for(&self, gate: &EntryMigrationGuardV2) -> Result<(), EntryGateErrorV2> {
        gate.verify_migrating()?;
        self.coordinator.verify()?;
        if self.migration_id != gate.migration_id
            || self.manifest_hash != gate.manifest_hash
            || self.inspected == 0
        {
            return Err(EntryGateErrorV2::ProcessUnresolved);
        }
        // The exclusive admission lock prevents cooperating new entries. The
        // inventory is nevertheless repeated at the irreversible retirement.
        process_inventory::verify_stop_zero(&gate.manifest)?;
        Ok(())
    }
}

fn write_exclusive<T: Serialize>(
    parent: &File,
    name: &str,
    value: &T,
) -> Result<(), EntryGateErrorV2> {
    let bytes = serde_json::to_vec(value).map_err(|_| EntryGateErrorV2::InvalidGate)?;
    if bytes.len() as u64 > MAX_GATE_BYTES {
        return Err(EntryGateErrorV2::InvalidGate);
    }
    let mut file =
        child(parent, name, libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL).map_err(storage)?;
    private_file(&file)?;
    file.write_all(&bytes).map_err(storage)?;
    file.sync_all().map_err(storage)?;
    parent.sync_all().map_err(storage)?;
    if read_optional(parent, name)?.as_deref() != Some(bytes.as_slice()) {
        return Err(EntryGateErrorV2::InvalidGate);
    }
    Ok(())
}

#[path = "entry_gate/process_inventory.rs"]
mod process_inventory;

#[cfg(test)]
thread_local! {
    pub(crate) static AFTER_MIGRATING_CONTENT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const {std::cell::RefCell::new(None)};
}

#[cfg(test)]
mod tests;
