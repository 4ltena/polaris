//! Physical source retirement. Failure preserves all on-disk evidence and the
//! Migrating gate; this owner never publishes or grants an Active capability.
use super::super::entry_gate::{EntryAnchorV2, child, private_file};
use super::intent_file::{ExternalIntentFileV2, verify_mount};
use super::*;
use polaris_mcp::{
    admission::same_metadata,
    migration_observation::*,
    store::ledger_v1::{RetiredLedgerAnchorV1, TombstoneLedgerAnchorV1},
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    ffi::CString,
    fs::{File, Metadata},
    io::Write,
    os::{fd::AsRawFd, unix::fs::FileExt},
};
pub(super) mod source_payload;
mod staging;

pub(super) struct RetiredMigrationV2 {
    admission: MigrationAdmissionV2,
    intent: ExternalIntentFileV2,
    source_lock: SourceWriterLockV2,
    tombstone: TombstonePin,
}
struct SourceWriterLockV2(Option<(File, Metadata)>);
#[derive(Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct TombstoneV2 {
    protocol_version: u32,
    migration_id: String,
    retired_source_anchor: EntryAnchorV2,
}
struct TombstonePin {
    file: File,
    metadata: Metadata,
    bytes: Vec<u8>,
}
impl MigrationAdmissionV2 {
    pub(super) fn retire_source(mut self) -> Result<RetiredMigrationV2, Error> {
        self.verify_before_retire()?;
        let source_lock = SourceWriterLockV2::acquire(self.source.held_root())?;
        source_lock.verify(self.source.held_root())?;
        self.verify_before_retire()?;
        // The record is derived from actual native inventory, never a caller's
        // decoded historical stop DTO. V1 has no persisted V2 store identity.
        let stopped = self.guard.prove_stopped().map_err(protection)?;
        let (_, id, manifest, hash) = self.guard.verified_binding().map_err(protection)?;
        let mut random = [0; 32];
        getrandom::fill(&mut random).map_err(|_| Error::Storage)?;
        let binding = MigrationIntentBindingV2 {
            schema_version: 2,
            migration_id: id.into(),
            store_identity: random.iter().map(|b| format!("{b:02x}")).collect(),
            source_store_identity: None,
            layout: ProtocolLayoutV2::new(id)?,
            canonical_parent_anchor: anchor(&self.guard.parent)?,
            source_root_anchor: anchor(self.source.held_root())?,
            source_non_secret_tree_hash: self.source.source_digest().into(),
            entry_manifest: manifest.clone(),
            entry_manifest_hash: hash.into(),
            stop: stopped.observation(),
        };
        let initial = ExternalMigrationIntentV2::retire_prepared(binding.clone())?;
        for name in [
            binding.layout.retired.component(),
            binding.layout.staging.component(),
            binding.layout.active.component(),
        ] {
            absent(&self.guard.parent, &name)?;
        }
        self.verify_before_retire()?;
        source_lock.verify(self.source.held_root())?;
        let intent = ExternalIntentFileV2::create(&self.guard, initial.clone())?;
        fault("after-intent")?;
        self.verify_before_retire()?;
        source_lock.verify(self.source.held_root())?;
        intent.revalidate(&self.guard)?;
        fault("before-rename")?;
        rename_no_replace(
            &self.guard.parent,
            "mcp",
            &binding.layout.retired.component(),
        )?;
        fault("after-rename")?;
        self.guard.parent.sync_all().map_err(storage)?;
        fault("after-rename-fsync")?;
        absent(&self.guard.parent, "mcp")?;
        // The original observation is consumed. An interrupted reinspection
        // cannot be reused to perform another rename or erase source evidence.
        self.source = self.source.reobserve_after_rename(&self.guard)?;
        self.verify_before_retire()?;
        source_lock.verify(self.source.held_root())?;
        intent.revalidate(&self.guard)?;
        let tombstone = TombstonePin::create(
            &self.guard.parent,
            TombstoneV2 {
                protocol_version: 2,
                migration_id: binding.migration_id.clone(),
                retired_source_anchor: anchor(self.source.held_root())?,
            },
        )?;
        fault("after-tombstone")?;
        let next = initial.retired(
            anchor(&self.guard.parent)?,
            RetiredLedgerAnchorV1 {
                anchor: anchor(self.source.held_root())?,
                non_secret_tree_hash: self.source.source_digest().into(),
            },
            tombstone.record()?,
        )?;
        let intent = intent.advance_with_check(&self.guard, next, || {
            self.verify_before_retire()?;
            source_lock.verify(self.source.held_root())?;
            tombstone.verify(&self.guard.parent)
        })?;
        fault("after-retired-intent")?;
        let retired = RetiredMigrationV2 {
            admission: self,
            intent,
            source_lock,
            tombstone,
        };
        retired.verify_current()?;
        Ok(retired)
    }
}
impl RetiredMigrationV2 {
    pub(super) fn stage_management_projects(mut self) -> Result<Self, Error> {
        self.verify_current()?;
        let (_, id, _, _) = self
            .admission
            .guard
            .verified_binding()
            .map_err(protection)?;
        let name = format!("management-effects-v2.staging-{id}");
        let paths = self
            .admission
            .roots
            .management_staging_paths()
            .map_err(protection)?;
        for path in paths {
            self.verify_current()?;
            let parent = self
                .admission
                .roots
                .management_staging_parent(&path)
                .map_err(protection)?
                .try_clone()
                .map_err(storage)?;
            absent(&parent, &name)?;
            self.verify_current()?;
            fault("before-management-staging-root")?;
            let component = CString::new(name.as_str()).map_err(protection)?;
            if unsafe { libc::mkdirat(parent.as_raw_fd(), component.as_ptr(), 0o700) } != 0 {
                return Err(Error::Protection);
            }
            let stage = child(
                &parent,
                &name,
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NONBLOCK,
            )
            .map_err(protection)?;
            verify_mount(&parent, &stage)?;
            stage.sync_all().map_err(storage)?;
            parent.sync_all().map_err(storage)?;
            fault("after-management-staging-root")?;
            self.admission.roots = self
                .admission
                .roots
                .bind_management_stage(&path, stage)
                .map_err(protection)?;
            self.admission.discovery = self
                .admission
                .discovery
                .bind_management_phase(&self.admission.roots, &path)
                .map_err(protection)?;
            self.verify_current()?;
            let (file, tree) = self
                .admission
                .roots
                .management_prepare_stage(&path)
                .map_err(protection)?;
            let file = file.try_clone().map_err(storage)?;
            let generations = tree.manifest().generations;
            drop(tree);
            let generation_name = CString::new("generations").map_err(protection)?;
            if unsafe { libc::mkdirat(file.as_raw_fd(), generation_name.as_ptr(), 0o700) } != 0 {
                return Err(Error::Protection);
            }
            let generation_root = child(
                &file,
                "generations",
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NONBLOCK,
            )
            .map_err(protection)?;
            verify_mount(&file, &generation_root)?;
            for generation in 1..generations {
                let name = CString::new(generation.to_string()).map_err(protection)?;
                if unsafe { libc::mkdirat(generation_root.as_raw_fd(), name.as_ptr(), 0o700) } != 0
                {
                    return Err(Error::Protection);
                }
            }
            generation_root.sync_all().map_err(storage)?;
            file.sync_all().map_err(storage)?;
            // All directory entries exist before binding the exact root links
            // used by the immutable generation writer (including macOS).
            let (_, mut tree) = self
                .admission
                .roots
                .management_prepare_stage(&path)
                .map_err(protection)?;
            self.verify_current()?;
            fault("before-management-staging-payload")?;
            tree.write_staging_mounted(
                &file,
                crate::management_effect::ManagementEffectLimitsV1::default(),
            )
            .map_err(protection)?;
            fault("after-management-staging-payload")?;
            self.admission.roots = self
                .admission
                .roots
                .finish_management_stage(&path, tree)
                .map_err(protection)?;
            self.admission.discovery = self
                .admission
                .discovery
                .bind_management_phase(&self.admission.roots, &path)
                .map_err(protection)?;
            self.verify_current()?;
            fault("after-management-staging-readback")?;
        }
        self.verify_current()?;
        Ok(self)
    }
    /// Physical project retirement remains under the same global EX gate,
    /// source lock, project leases and management history owner locks.
    pub(super) fn retire_management_projects(mut self) -> Result<Self, Error> {
        self.verify_current()?;
        let (_, migration_id, _, _) = self
            .admission
            .guard
            .verified_binding()
            .map_err(protection)?;
        let id = crate::management_effect::ManagementEffectHashV1::from_hex(migration_id)
            .map_err(protection)?;
        let migration_id = migration_id.to_owned();
        let paths = self
            .admission
            .roots
            .management_paths()
            .map_err(protection)?;
        for path in paths {
            self.verify_current()?;
            let (parent, source_anchor) = self
                .admission
                .roots
                .management_target(&path)
                .map_err(protection)?;
            let parent = parent.try_clone().map_err(storage)?;
            let retired_name = format!("management-effects-v1.retired-{migration_id}");
            absent(&parent, &retired_name)?;
            absent(&parent, "management-effects-v2")?;
            absent(
                &parent,
                &format!("management-effects-v2.staging-{migration_id}"),
            )?;
            self.verify_current()?;
            fault("before-management-rename")?;
            rename_no_replace(&parent, "management-effects-v1", &retired_name)?;
            fault("after-management-rename")?;
            parent.sync_all().map_err(storage)?;
            fault("after-management-rename-fsync")?;
            self.admission.roots = self
                .admission
                .roots
                .rebind_management_renamed(&path, id)
                .map_err(protection)?;
            self.admission.discovery = self
                .admission
                .discovery
                .bind_management_phase(&self.admission.roots, &path)
                .map_err(protection)?;
            self.verify_current()?;
            absent(&parent, "management-effects-v1")?;
            fault("before-management-tombstone")?;
            self.verify_current()?;
            let mut file = child(
                &parent,
                "management-effects-v1",
                libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
            )
            .map_err(protection)?;
            private_file(&file).map_err(protection)?;
            verify_mount(&parent, &file)?;
            fault("after-management-tombstone-create")?;
            let bytes = serde_json::to_vec(&TombstoneV2 {
                protocol_version: 2,
                migration_id: migration_id.clone(),
                retired_source_anchor: source_anchor,
            })
            .map_err(protection)?;
            file.write_all(&bytes).map_err(storage)?;
            file.sync_all().map_err(storage)?;
            fault("after-management-tombstone-file-fsync")?;
            parent.sync_all().map_err(storage)?;
            fault("after-management-tombstone-parent-fsync")?;
            self.admission.roots = self
                .admission
                .roots
                .bind_management_tombstone(&path)
                .map_err(protection)?;
            self.admission.discovery = self
                .admission
                .discovery
                .bind_management_phase(&self.admission.roots, &path)
                .map_err(protection)?;
            self.verify_current()?;
            fault("after-management-retired")?;
        }
        self.verify_current()?;
        Ok(self)
    }
    pub(super) fn prepare_source_payload(
        &self,
    ) -> Result<source_payload::SourcePayloadV2<'_>, Error> {
        self.verify_current()?;
        if !matches!(
            self.intent.record(),
            ExternalMigrationIntentV2::Retired { .. }
        ) {
            return Err(Error::Protocol);
        }
        let (home, id, _, _) = self
            .admission
            .guard
            .verified_binding()
            .map_err(protection)?;
        let payload = source_payload::SourcePayloadV2::inspect(&self.admission.source, id, home)?;
        self.verify_current()?;
        payload.verify_current()?;
        Ok(payload)
    }
    pub(super) fn verify_current(&self) -> Result<(), Error> {
        let a = &self.admission;
        self.intent.revalidate(&a.guard)?;
        self.source_lock.verify(a.source.held_root())?;
        self.tombstone.verify(&a.guard.parent)?;
        a.verify_before_retire()?;
        self.source_lock.verify(a.source.held_root())?;
        self.tombstone.verify(&a.guard.parent)?;
        self.intent.revalidate(&a.guard)
    }
    // Intent exchange already checks its own pins. Keep the actual retirement
    // and all native/root ownership checks usable while consuming that writer.
    fn verify_retirement_parts(
        admission: &MigrationAdmissionV2,
        source_lock: &SourceWriterLockV2,
        tombstone: &TombstonePin,
    ) -> Result<(), Error> {
        source_lock.verify(admission.source.held_root())?;
        tombstone.verify(&admission.guard.parent)?;
        admission.verify_before_retire()?;
        source_lock.verify(admission.source.held_root())?;
        tombstone.verify(&admission.guard.parent)
    }
}
impl SourceWriterLockV2 {
    fn acquire(root: &File) -> Result<Self, Error> {
        let file = match child(root, "owner.lock", libc::O_RDONLY | libc::O_NONBLOCK) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Self(None)),
            result => result.map_err(protection)?,
        };
        private_file(&file).map_err(protection)?;
        verify_mount(root, &file)?;
        file.try_lock().map_err(protection)?;
        let metadata = file.metadata().map_err(storage)?;
        let pin = Self(Some((file, metadata)));
        pin.verify(root)?;
        Ok(pin)
    }
    fn verify(&self, root: &File) -> Result<(), Error> {
        let Some((held, metadata)) = &self.0 else {
            return absent(root, "owner.lock");
        };
        let named =
            child(root, "owner.lock", libc::O_RDONLY | libc::O_NONBLOCK).map_err(protection)?;
        for file in [held, &named] {
            private_file(file).map_err(protection)?;
            verify_mount(root, file)?;
            if !same_metadata(metadata, &file.metadata().map_err(storage)?) {
                return Err(Error::Protection);
            }
        }
        Ok(())
    }
}
impl TombstonePin {
    fn create(parent: &File, record: TombstoneV2) -> Result<Self, Error> {
        let bytes = serde_json::to_vec(&record).map_err(|_| Error::Protocol)?;
        let mut file = child(parent, "mcp", libc::O_RDWR | libc::O_CREAT | libc::O_EXCL)
            .map_err(protection)?;
        private_file(&file).map_err(protection)?;
        verify_mount(parent, &file)?;
        fault("after-tombstone-create")?;
        file.write_all(&bytes).map_err(storage)?;
        fault("after-tombstone-write")?;
        file.sync_all().map_err(storage)?;
        fault("after-tombstone-file-fsync")?;
        parent.sync_all().map_err(storage)?;
        let pin = Self {
            metadata: file.metadata().map_err(storage)?,
            file,
            bytes,
        };
        pin.verify(parent)?;
        Ok(pin)
    }
    fn verify(&self, parent: &File) -> Result<(), Error> {
        let named = child(parent, "mcp", libc::O_RDONLY | libc::O_NONBLOCK).map_err(protection)?;
        for file in [&self.file, &named] {
            private_file(file).map_err(protection)?;
            verify_mount(parent, file)?;
            if !same_metadata(&self.metadata, &file.metadata().map_err(storage)?) {
                return Err(Error::Protection);
            }
            let mut bytes = vec![0; self.bytes.len()];
            file.read_exact_at(&mut bytes, 0).map_err(storage)?;
            verify_mount(parent, file)?;
            if bytes != self.bytes
                || !same_metadata(&self.metadata, &file.metadata().map_err(storage)?)
            {
                return Err(Error::Protection);
            }
        }
        Ok(())
    }
    fn record(&self) -> Result<TombstoneLedgerAnchorV1, Error> {
        Ok(TombstoneLedgerAnchorV1 {
            anchor: anchor(&self.file)?,
            file_hash: format!("{:x}", Sha256::digest(&self.bytes)),
        })
    }
}
fn anchor(file: &File) -> Result<EntryAnchorV2, Error> {
    Ok(EntryAnchorV2::of(&file.metadata().map_err(storage)?))
}
fn storage(_: std::io::Error) -> Error {
    Error::Storage
}
fn absent(parent: &File, name: &str) -> Result<(), Error> {
    match child(parent, name, libc::O_RDONLY | libc::O_NONBLOCK) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        _ => Err(Error::Protection),
    }
}
fn rename_no_replace(parent: &File, source: &str, target: &str) -> Result<(), Error> {
    let source = CString::new(source).map_err(protection)?;
    let target = CString::new(target).map_err(protection)?;
    #[cfg(target_os = "macos")]
    let result = unsafe {
        libc::renameatx_np(
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    let result = unsafe {
        libc::renameat2(
            parent.as_raw_fd(),
            source.as_ptr(),
            parent.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(not(any(target_os = "macos", all(target_os = "linux", target_env = "gnu"))))]
    return Err(Error::Protection);
    #[cfg(any(target_os = "macos", all(target_os = "linux", target_env = "gnu")))]
    if result != 0 {
        return Err(Error::Protection);
    }
    Ok(())
}
fn fault(_point: &'static str) -> Result<(), Error> {
    #[cfg(test)]
    if let Some(action) = FAULT.with(|slot| {
        let mut slot = slot.borrow_mut();
        if slot.as_ref().is_some_and(|(point, _)| *point == _point) {
            slot.take().map(|(_, action)| action)
        } else {
            None
        }
    }) {
        action()?;
    }
    Ok(())
}
#[cfg(test)]
type RetirementFault = (&'static str, Box<dyn FnOnce() -> Result<(), Error>>);

#[cfg(test)]
thread_local! { static FAULT: std::cell::RefCell<Option<RetirementFault>> = const { std::cell::RefCell::new(None) }; }
#[cfg(test)]
mod tests;
