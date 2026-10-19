//! Side-effect-free source inspection before acquiring the migrating entry gate.
//! This owns actual source/parent descriptors and closed document/inventory
//! views. Project non-terminal bindings and stop proof remain separate gates.
use super::entry_gate::{
    EntryAnchorV2, EntryMigrationGuardV2, child, owned_directory, physical_directory, private_file,
};
use polaris_mcp::migration_observation::{CompleteTreeProjectionV2, SourceDocumentsV1};
use polaris_mcp::{
    Error,
    admission::same_metadata,
    catalog::index::{CatalogIndex, MAX_INDEX_BYTES},
};
use std::{
    fs::{File, Metadata},
    io::Read,
    path::{Path, PathBuf},
};

pub(super) struct SourcePreflightV2 {
    home: PathBuf,
    home_guard: Option<std::sync::Arc<super::account_home::TrustedUserHomeV2>>,
    parent: File,
    parent_anchor: EntryAnchorV2,
    root: File,
    root_name: String,
    inventory: CompleteTreeProjectionV2,
    documents: SourceDocumentsV1,
    catalog: Option<CatalogPin>,
}
struct CatalogPin {
    file: File,
    metadata: Metadata,
    bytes: Vec<u8>,
}

impl SourcePreflightV2 {
    pub(super) fn inspect_trusted(
        home: std::sync::Arc<super::account_home::TrustedUserHomeV2>,
    ) -> Result<Self, Error> {
        let path = home.verified_path().map_err(protection)?.to_owned();
        Self::inspect_at_path(&path, Some(home))
    }
    #[cfg(test)]
    pub(super) fn inspect(home: &Path) -> Result<Self, Error> {
        Self::inspect_at_path(home, None)
    }
    fn inspect_at_path(
        home: &Path,
        home_guard: Option<std::sync::Arc<super::account_home::TrustedUserHomeV2>>,
    ) -> Result<Self, Error> {
        // No mkdir/open-create, lock mutation, intent or output tree here.
        let parent = source_parent(home, home_guard.as_deref())?;
        let parent_anchor = EntryAnchorV2::of(&parent.metadata().map_err(storage)?);
        let root = child(&parent, "mcp", libc::O_RDONLY | libc::O_DIRECTORY).map_err(protection)?;
        owned_directory(&root, true).map_err(protection)?;
        let inventory =
            CompleteTreeProjectionV2::inspect_source(&root, &home.join(".polaris/mcp"))?;
        inventory.verify_same_mount(&parent)?;
        let documents = SourceDocumentsV1::inspect(&root)?;
        let catalog = CatalogPin::inspect(&root)?;
        let value = Self {
            home: home.into(),
            home_guard,
            parent,
            parent_anchor,
            root,
            root_name: "mcp".into(),
            inventory,
            documents,
            catalog,
        };
        value.verify_current()?;
        Ok(value)
    }

    pub(super) fn verify_current(&self) -> Result<(), Error> {
        if let Some(home) = &self.home_guard {
            home.verify_current().map_err(protection)?;
        }
        self.verify_namespace()?;
        self.inventory.verify_before_retire()?;
        self.documents.verify_current()?;
        match &self.catalog {
            Some(pin) => pin.verify(&self.root)?,
            None => match child(&self.root, "catalog.json", libc::O_RDONLY) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                _ => return Err(Error::Protection),
            },
        }
        self.inventory.verify_before_retire()?;
        self.verify_namespace()?;
        if let Some(home) = &self.home_guard {
            home.verify_current().map_err(protection)?;
        }
        Ok(())
    }

    /// Consume the old observation after a host-owned rename. This validates
    /// source continuity only; the physical owner separately pins the intent,
    /// tombstone and every other storage owner and proves fresh native zero.
    pub(super) fn inspect_retired(
        self,
        guard: &EntryMigrationGuardV2,
        record: &polaris_mcp::migration_observation::RetiredRecordV2,
    ) -> Result<Self, Error> {
        polaris_mcp::migration_observation::ExternalMigrationIntentV2::Retired {
            record: record.clone(),
        }
        .validate()?;
        let (home, id, manifest, hash) = guard.verified_binding().map_err(protection)?;
        let binding = &record.binding;
        if self.root_name != "mcp"
            || home != self.home
            || id != binding.migration_id
            || manifest != &binding.entry_manifest
            || hash != binding.entry_manifest_hash
            || binding.source_non_secret_tree_hash != self.inventory.digest()
            || binding.source_root_anchor
                != EntryAnchorV2::of(&self.root.metadata().map_err(storage)?)
            || !same_parent(&binding.canonical_parent_anchor, &self.parent_anchor)
        {
            return Err(Error::Protection);
        }
        let name = binding.layout.retired.component();
        let named =
            child(&guard.parent, &name, libc::O_RDONLY | libc::O_DIRECTORY).map_err(protection)?;
        self.inventory.verify_root_mount_binding(&named)?;
        if record.retired_source.anchor != EntryAnchorV2::of(&named.metadata().map_err(storage)?)
            || record.retired_source.non_secret_tree_hash != self.inventory.digest()
        {
            return Err(Error::Protection);
        }
        self.reobserve_after_rename(guard)
    }

    pub(super) fn held_root(&self) -> &File {
        &self.root
    }

    pub(super) fn source_digest(&self) -> &str {
        self.inventory.digest()
    }

    // Read observations only; the coordinator retains this source and closes
    // its guard, source lock and native process inventory around preparation.
    pub(super) fn staging_observations(
        &self,
    ) -> Result<(&CompleteTreeProjectionV2, &SourceDocumentsV1), Error> {
        self.verify_current()?;
        Ok((&self.inventory, &self.documents))
    }

    /// The physical owner calls this after its no-replace rename and parent
    /// fsync, before creating the tombstone. It grants no publication permit.
    pub(super) fn reobserve_after_rename(
        mut self,
        guard: &EntryMigrationGuardV2,
    ) -> Result<Self, Error> {
        let (home, id, _, _) = guard.verified_binding().map_err(protection)?;
        if self.root_name != "mcp" || home != self.home {
            return Err(Error::Protection);
        }
        let name = polaris_mcp::migration_observation::ProtocolLayoutV2::new(id)?
            .retired
            .component();
        let named =
            child(&guard.parent, &name, libc::O_RDONLY | libc::O_DIRECTORY).map_err(protection)?;
        self.inventory.verify_root_mount_binding(&named)?;
        let inventory = self.inventory.inspect_retired_same_root(&named)?;
        let documents = self.documents.inspect_retired_same_root(&named)?;
        if let Some(catalog) = &self.catalog {
            catalog.verify(&named)?;
        }
        self.root_name = name;
        self.inventory = inventory;
        self.documents = documents;
        self.bind_guard(guard)?;
        Ok(self)
    }

    /// Bind a pre-gate observation to the canonical parent and source actually
    /// protected by the still-held migration guard. This is no stop proof.
    pub(super) fn bind_guard(&self, guard: &EntryMigrationGuardV2) -> Result<(), Error> {
        let (home, _, _, _) = guard.verified_binding().map_err(protection)?;
        if home != self.home {
            return Err(Error::Protection);
        }
        let parent = EntryAnchorV2::of(&guard.parent.metadata().map_err(storage)?);
        if !same_parent(&parent, &self.parent_anchor) {
            return Err(Error::Protection);
        }
        self.inventory.verify_same_mount(&guard.parent)?;
        let named = child(
            &guard.parent,
            &self.root_name,
            libc::O_RDONLY | libc::O_DIRECTORY,
        )
        .map_err(protection)?;
        self.inventory.verify_root_mount_binding(&named)?;
        self.verify_current()?;
        guard.verify_migrating().map_err(protection)
    }

    fn verify_namespace(&self) -> Result<(), Error> {
        let named_parent = source_parent(&self.home, self.home_guard.as_deref())?;
        for parent in [&self.parent, &named_parent] {
            if !same_parent(
                &self.parent_anchor,
                &EntryAnchorV2::of(&parent.metadata().map_err(storage)?),
            ) {
                return Err(Error::Protection);
            }
            self.inventory.verify_same_mount(parent)?;
            let named_root = child(parent, &self.root_name, libc::O_RDONLY | libc::O_DIRECTORY)
                .map_err(protection)?;
            self.inventory.verify_root_mount_binding(&named_root)?;
        }
        Ok(())
    }
}
impl CatalogPin {
    fn inspect(root: &File) -> Result<Option<Self>, Error> {
        let file = match child(root, "catalog.json", libc::O_RDONLY) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            result => result.map_err(protection)?,
        };
        private_file(&file).map_err(protection)?;
        let metadata = file.metadata().map_err(storage)?;
        if metadata.len() > MAX_INDEX_BYTES as u64 {
            return Err(Error::Limit);
        }
        let mut bytes = Vec::new();
        (&file)
            .take(MAX_INDEX_BYTES as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(storage)?;
        if bytes.len() > MAX_INDEX_BYTES
            || !same_metadata(&metadata, &file.metadata().map_err(storage)?)
        {
            return Err(Error::Protection);
        }
        let index: CatalogIndex = serde_json::from_slice(&bytes).map_err(|_| Error::Protocol)?;
        index.validate()?;
        let pin = Self {
            file,
            metadata,
            bytes,
        };
        pin.verify(root)?;
        Ok(Some(pin))
    }
    fn verify(&self, root: &File) -> Result<(), Error> {
        use std::os::unix::fs::FileExt;
        let named = child(root, "catalog.json", libc::O_RDONLY).map_err(protection)?;
        for file in [&self.file, &named] {
            private_file(file).map_err(protection)?;
            if !same_metadata(&self.metadata, &file.metadata().map_err(storage)?) {
                return Err(Error::Protection);
            }
            let mut bytes = vec![0; self.bytes.len()];
            file.read_exact_at(&mut bytes, 0).map_err(protection)?;
            if bytes != self.bytes
                || !same_metadata(&self.metadata, &file.metadata().map_err(storage)?)
            {
                return Err(Error::Protection);
            }
        }
        Ok(())
    }
}
fn source_parent(
    home: &Path,
    trusted: Option<&super::account_home::TrustedUserHomeV2>,
) -> Result<File, Error> {
    if let Some(home) = trusted {
        return home.verified_polaris_parent().map_err(protection);
    }
    let home = physical_directory(home).map_err(protection)?;
    owned_directory(&home, false).map_err(protection)?;
    let parent =
        child(&home, ".polaris", libc::O_RDONLY | libc::O_DIRECTORY).map_err(protection)?;
    owned_directory(&parent, false).map_err(protection)?;
    Ok(parent)
}
fn same_parent(a: &EntryAnchorV2, b: &EntryAnchorV2) -> bool {
    (a.device, a.inode, a.uid, a.mode) == (b.device, b.inode, b.uid, b.mode)
}
fn protection<T>(_: T) -> Error {
    Error::Protection
}
fn storage<T>(_: T) -> Error {
    Error::Storage
}
#[cfg(test)]
mod tests;
