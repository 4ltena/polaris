//! Per-user production entry admission for the MCP protocol cutover.
//!
//! The lock is independent of project and MCP document locks. Admission reads
//! existing files only; a legacy installation with no gate stays explicit.
//! A migration holds the exclusive lock through stop-proof and publication.

pub use polaris_tools::entry_kernel::{
    ENTRY_MANIFEST, EntryAnchorV2, EntryGateErrorV2, GATE_FILE, GATE_LOCK,
    ProductionBinaryIdentityV2, ProductionEntryGateV2, ProductionEntryIdentityV2,
    ProductionEntryKindV2, ProductionEntryManifestV2,
};
pub(crate) use polaris_tools::entry_kernel::{
    child, child_bytes, owned_directory, physical_directory, private_file, valid_digest,
};
use polaris_tools::entry_kernel::{
    decode, lock_shared, physical_search_directory, read_optional, runtime_copy_anchors,
    search_directory,
};
use std::{
    fs::File,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
};
fn storage(_: std::io::Error) -> EntryGateErrorV2 {
    EntryGateErrorV2::Storage
}
/// Kept alive by every admitted canonical entry until its children have exited.
/// Dropping a File alone releases this open-file-description's shared flock;
/// an explicit unlock must never release another inherited holder's lease.
pub struct ProductionEntryLeaseV2 {
    home: PathBuf,
    parent: Option<File>,
    lock: Option<File>,
    lock_anchor: Option<EntryAnchorV2>,
    gate: Option<ProductionEntryGateV2>,
    manifest: Option<ProductionEntryManifestV2>,
    admitted_binary: Option<ProductionBinaryIdentityV2>,
    runtime_anchors: Option<(EntryAnchorV2, EntryAnchorV2)>,
}

impl ProductionEntryLeaseV2 {
    /// Existing gate/lock/manifest files are never created or repaired here.
    pub fn enter(home: &Path, executable: &Path) -> Result<Self, EntryGateErrorV2> {
        let home_fd = physical_search_directory(home)?;
        owned_directory(&home_fd, false)?;
        let parent = match search_directory(&home_fd, ".polaris") {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::legacy(home));
            }
            result => result.map_err(storage)?,
        };
        owned_directory(&parent, false)?;
        let gate_bytes = read_optional(&parent, GATE_FILE)?;
        let manifest_bytes = read_optional(&parent, ENTRY_MANIFEST)?;
        let lock = match child(&parent, GATE_LOCK, libc::O_RDONLY) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
            result => Some(result.map_err(storage)?),
        };
        if gate_bytes.is_none() && manifest_bytes.is_none() && lock.is_none() {
            return Ok(Self::legacy(home));
        }
        let lock = lock.ok_or(EntryGateErrorV2::InvalidGate)?;
        private_file(&lock)?;
        lock_shared(&lock)?;
        // Read again only after admission lock acquisition. A writer may have
        // completed between the initial presence probe and this shared lock.
        let gate: ProductionEntryGateV2 =
            decode(&read_optional(&parent, GATE_FILE)?.ok_or(EntryGateErrorV2::InvalidGate)?)?;
        gate.validate()?;
        if matches!(gate, ProductionEntryGateV2::MigratingV2 { .. }) {
            return Err(EntryGateErrorV2::Migrating);
        }
        let manifest: ProductionEntryManifestV2 =
            decode(&read_optional(&parent, ENTRY_MANIFEST)?.ok_or(EntryGateErrorV2::InvalidGate)?)?;
        let expected = match &gate {
            ProductionEntryGateV2::ActiveV2 {
                entry_manifest_hash,
                ..
            } => entry_manifest_hash,
            _ => unreachable!(),
        };
        if manifest.canonical_hash()? != *expected {
            return Err(EntryGateErrorV2::InvalidGate);
        }
        let admitted_binary = ProductionBinaryIdentityV2::inspect(executable)?;
        manifest.admits(&admitted_binary)?;
        let runtime_anchors = if manifest
            .entries
            .iter()
            .any(|entry| entry.binary == admitted_binary)
        {
            None
        } else {
            Some(runtime_copy_anchors(&admitted_binary)?)
        };
        let lock_anchor = EntryAnchorV2::of(&lock.metadata().map_err(storage)?);
        let lease = Self {
            home: home.to_owned(),
            parent: Some(parent),
            lock: Some(lock),
            lock_anchor: Some(lock_anchor),
            gate: Some(gate),
            manifest: Some(manifest),
            admitted_binary: Some(admitted_binary),
            runtime_anchors,
        };
        lease.verify()?;
        Ok(lease)
    }

    fn legacy(home: &Path) -> Self {
        Self {
            home: home.to_owned(),
            parent: None,
            lock: None,
            lock_anchor: None,
            gate: None,
            manifest: None,
            admitted_binary: None,
            runtime_anchors: None,
        }
    }

    pub fn is_active_v2(&self) -> bool {
        self.gate.is_some()
    }

    pub fn verify(&self) -> Result<(), EntryGateErrorV2> {
        let Some(parent) = &self.parent else {
            let home = physical_search_directory(&self.home)?;
            match search_directory(&home, ".polaris") {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                result => {
                    let parent = result.map_err(storage)?;
                    for name in [GATE_FILE, GATE_LOCK, ENTRY_MANIFEST] {
                        if read_optional(&parent, name)?.is_some() {
                            return Err(EntryGateErrorV2::InvalidGate);
                        }
                    }
                    return Ok(());
                }
            }
        };
        let current_parent = search_directory(&physical_search_directory(&self.home)?, ".polaris")
            .map_err(storage)?;
        owned_directory(&current_parent, false)?;
        let old = parent.metadata().map_err(storage)?;
        let fresh = current_parent.metadata().map_err(storage)?;
        if (old.dev(), old.ino(), old.uid(), old.mode())
            != (fresh.dev(), fresh.ino(), fresh.uid(), fresh.mode())
        {
            return Err(EntryGateErrorV2::InvalidGate);
        }
        let lock = self.lock.as_ref().ok_or(EntryGateErrorV2::InvalidGate)?;
        let current_lock = child(&current_parent, GATE_LOCK, libc::O_RDONLY).map_err(storage)?;
        private_file(&current_lock)?;
        if Some(EntryAnchorV2::of(
            &current_lock.metadata().map_err(storage)?,
        )) != self.lock_anchor
            || Some(EntryAnchorV2::of(&lock.metadata().map_err(storage)?)) != self.lock_anchor
        {
            return Err(EntryGateErrorV2::InvalidGate);
        }
        let gate: ProductionEntryGateV2 = decode(
            &read_optional(&current_parent, GATE_FILE)?.ok_or(EntryGateErrorV2::InvalidGate)?,
        )?;
        if Some(&gate) != self.gate.as_ref() {
            return Err(EntryGateErrorV2::InvalidGate);
        }
        let manifest: ProductionEntryManifestV2 = decode(
            &read_optional(&current_parent, ENTRY_MANIFEST)?
                .ok_or(EntryGateErrorV2::InvalidGate)?,
        )?;
        let ProductionEntryGateV2::ActiveV2 {
            entry_manifest_hash,
            ..
        } = gate
        else {
            return Err(EntryGateErrorV2::Migrating);
        };
        if manifest.canonical_hash()? != entry_manifest_hash {
            return Err(EntryGateErrorV2::InvalidGate);
        }
        let admitted = self
            .admitted_binary
            .as_ref()
            .ok_or(EntryGateErrorV2::Identity)?;
        admitted.verify()?;
        manifest.admits(admitted)?;
        if let Some((runtime, run)) = &self.runtime_anchors {
            let (fresh_runtime, fresh_run) = runtime_copy_anchors(admitted)?;
            // A private run may create result entries while admission is held.
            // Its directory identity and permissions remain pinned.
            let directory_identity = |a: &EntryAnchorV2| (a.device, a.inode, a.uid, a.mode);
            if directory_identity(runtime) != directory_identity(&fresh_runtime)
                || directory_identity(run) != directory_identity(&fresh_run)
            {
                return Err(EntryGateErrorV2::Identity);
            }
        }
        Ok(())
    }

    /// Exact non-secret read grants for a sandboxed runtime copy. Ancestors
    /// need metadata traversal only; no MCP document or user-home subtree.
    pub fn isolated_read_files(&self) -> Vec<PathBuf> {
        let mut files: Vec<_> = [GATE_FILE, GATE_LOCK, ENTRY_MANIFEST]
            .iter()
            .map(|name| self.home.join(".polaris").join(name))
            .collect();
        if let Some(manifest) = &self.manifest {
            // All entry binaries remain fully revalidated by the helper. These
            // exact non-secret executable files grant no directory contents.
            files.extend(
                manifest
                    .entries
                    .iter()
                    .map(|entry| entry.binary.path.clone()),
            );
        }
        files
    }
}
