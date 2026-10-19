//! Global pre-epoch staging under the retained retirement owner. Failed writes
//! preserve the partial tree and Migrating gate; nothing here publishes V2.
use super::source_payload::{SourcePayloadV2, public_hash};
use super::*;
use crate::mcp::migration::read_admission::{
    inspect_mapped_stage, validate_unregistered_pin_reader,
};
use polaris_mcp::{
    NativeMountIdentityV2,
    admission::validate_mapped_servers_no_exec,
    registration::{Destination, Registry},
    store::documents_v2::{CredentialDocumentV2, RegistryDocumentV2},
};
use polaris_tools::entry_kernel::child_bytes;
use std::{
    collections::BTreeMap,
    fs::Permissions,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

pub(in crate::mcp::migration::coordinator) struct StagedMigrationV2 {
    retired: RetiredMigrationV2,
    payload: StagedPayloadV2,
}
struct StagedPayloadV2 {
    tree: StageTree,
    inventory: CompleteTreeProjectionV2,
    staging: StagingRecordV2,
    record: StoreMigrationRecordV2,
    runtime: Vec<RuntimePin>,
}
impl RetiredMigrationV2 {
    pub(in crate::mcp::migration::coordinator) fn stage_source(
        self,
    ) -> Result<StagedMigrationV2, Error> {
        let self_owner = self
            .retire_management_projects()?
            .stage_management_projects()?;
        self_owner.stage_global_source()
    }
    fn stage_global_source(self) -> Result<StagedMigrationV2, Error> {
        let source = self.prepare_source_payload()?;
        self.verify_current()?;
        let payload = StagedPayloadV2::write(&self, &source)?;
        drop(source);
        self.verify_current()?;
        payload.verify(&self)?;
        let RetiredMigrationV2 {
            admission,
            intent,
            source_lock,
            tombstone,
        } = self;
        let next = ExternalMigrationIntentV2::Staging {
            record: payload.staging.clone(),
        };
        let intent = intent.advance_with_check(&admission.guard, next, || {
            RetiredMigrationV2::verify_retirement_parts(&admission, &source_lock, &tombstone)?;
            payload.verify_parts(&admission)?;
            RetiredMigrationV2::verify_retirement_parts(&admission, &source_lock, &tombstone)?;
            payload.tree.verify(&admission.guard.parent)?;
            staging_fault("before-staging-exchange")
        })?;
        staging_fault("after-staging-intent")?;
        let owner = StagedMigrationV2 {
            retired: RetiredMigrationV2 {
                admission,
                intent,
                source_lock,
                tombstone,
            },
            payload,
        };
        owner.verify_current()?;
        Ok(owner)
    }
}
impl StagedMigrationV2 {
    pub(in crate::mcp::migration::coordinator) fn verify_current(&self) -> Result<(), Error> {
        self.retired.verify_current()?;
        if self.retired.intent.record()
            != &(ExternalMigrationIntentV2::Staging {
                record: self.payload.staging.clone(),
            })
        {
            return Err(Error::Protection);
        }
        self.payload.verify(&self.retired)?;
        self.retired.verify_current()
    }
}
impl StagedPayloadV2 {
    fn write(owner: &RetiredMigrationV2, source: &SourcePayloadV2<'_>) -> Result<Self, Error> {
        let guard = &owner.admission.guard;
        let (home, id, _, _) = guard.verified_binding().map_err(protection)?;
        let name = ProtocolLayoutV2::new(id)?.staging.component();
        let mut tree = StageTree::create(&guard.parent, &name)?;
        staging_fault("after-staging-root")?;
        let (_, documents) = owner.admission.source.staging_observations()?;
        let registry = documents.verified_registry()?;
        tree.bytes(
            b"secrets.json",
            &source.documents.credential.encode()?,
            0o600,
        )?;
        staging_fault("after-staging-credential")?;
        tree.bytes(b"servers.json", &source.documents.registry.encode()?, 0o600)?;
        tree.bytes(
            b"catalog.json",
            &serde_json::to_vec(&source.catalog.empty_catalog).map_err(protection)?,
            0o600,
        )?;
        tree.bytes(
            b"catalog-regeneration.v2.json",
            &source.catalog.encode()?,
            0o600,
        )?;
        // Allocate this entry before collecting any root anchor. Its body
        // contains no self hash, and will be written after resource inspection.
        tree.bytes(b"migration.v2.json", &[], 0o600)?;
        let mut epoch = [0; 32];
        getrandom::fill(&mut epoch).map_err(|_| Error::Storage)?;
        let epoch: String = epoch.iter().map(|b| format!("{b:02x}")).collect();
        tree.directory(b"ledger", 0o700)?;
        tree.bytes(b"ledger/history.jsonl", &[], 0o600)?;
        tree.bytes(b"ledger/head.json", &empty_payload_head(&epoch)?, 0o600)?;
        let (source_tree, _) = owner.admission.source.staging_observations()?;
        let copy = source_tree.verify_before_copy()?;
        let mut package_files = Vec::new();
        for entry in copy.entries() {
            let path = entry.relative_path();
            if path != b"servers" && !path.starts_with(b"servers/") {
                continue;
            }
            if entry.is_directory() {
                tree.directory(path, entry.anchor().mode)?;
            } else {
                let source_file = entry.open_readonly()?;
                if let Some(server_id) = pin_id(path) {
                    tree.bytes(
                        path,
                        &source.staged_pin_bytes(server_id)?,
                        entry.anchor().mode,
                    )?;
                } else {
                    tree.copy(path, &source_file, entry.anchor().mode)?;
                }
                let SourceInventoryEntryV2::PublicFile { size, sha256, .. } = source
                    .inventory
                    .iter()
                    .find(|row| inventory_path(row) == path)
                    .ok_or(Error::Protocol)?
                else {
                    return Err(Error::Protocol);
                };
                let staged = tree.node(path)?;
                let staged_file = tree.open(path)?;
                package_files.push(PackageFileMappingV2 {
                    relative_path: path.into(),
                    source_anchor: entry.anchor(),
                    source_size: *size,
                    source_sha256: sha256.clone(),
                    staged_anchor: anchor(&staged_file)?,
                    staged_size: staged.metadata.len(),
                    staged_sha256: public_hash(&staged_file, &staged.metadata)?,
                });
            }
        }
        copy.verify_after_copy()?;
        tree.finalize_directories()?;
        staging_fault("after-package-copy")?;
        source.verify_current()?;
        tree.verify(&guard.parent)?;
        let ExternalMigrationIntentV2::Staging { record: staging } = owner
            .intent
            .record()
            .staging(anchor(&guard.parent)?, anchor(tree.root())?, epoch)?
        else {
            return Err(Error::Protocol);
        };
        let inventory = tree.inspect(&guard.parent)?;
        let view = inspect_mapped_stage(guard, &staging, &inventory)?;
        let runtime_roots = crate::mcp::local::RUNTIME_ROOTS
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        let admissions = admit_registry(
            owner,
            &view,
            &source.documents.registry,
            &source.documents.credential,
            &runtime_roots,
        )?;
        let (runtime_rechecks, runtime) =
            observe_runtime(&view, source.documents.registry.snapshot())?;
        for admission in &admissions {
            admission.revalidate()?;
        }
        for pin in &runtime {
            pin.verify(&view)?;
        }
        drop(admissions);
        drop(view);
        // This pre-record inventory has completed admission. Do not retain its
        // directory FDs while building the fresh, post-record inventory below.
        drop(inventory);
        let directories = source
            .inventory
            .iter()
            .filter_map(|row| match row {
                SourceInventoryEntryV2::Directory {
                    relative_path,
                    anchor: source_anchor,
                } => Some(tree.node(relative_path).and_then(|n| {
                    Ok(PackageDirectoryMappingV2 {
                        relative_path: relative_path.clone(),
                        source_anchor: *source_anchor,
                        staged_anchor: anchor(n.held()?)?,
                    })
                })),
                _ => None,
            })
            .collect::<Result<Vec<_>, Error>>()?;
        let namespace = MigrationParentObservationV2::inspect(home, &guard.parent)?;
        let mut record = StoreMigrationRecordV2::new_from_descriptor_reader(
            staging.clone(),
            polaris_mcp::migration_observation::StoreMigrationPayloadV2 {
                planned_active_anchor: anchor(tree.root())?,
                source_inventory: source.inventory.clone(),
                credential: BaselineDocumentRecordV2::credential(
                    &source.documents.credential,
                    source_anchor(source, b"secrets.json"),
                    anchor(&tree.open(b"secrets.json")?)?,
                )?,
                registry: BaselineDocumentRecordV2::registry(
                    &source.documents.registry,
                    source_anchor(source, b"servers.json"),
                    anchor(&tree.open(b"servers.json")?)?,
                )?,
                managed_paths: source
                    .documents
                    .mapped_paths
                    .as_ref()
                    .map(|m| m.projection.clone()),
                unregistered_pins: source.unregistered_pins.clone(),
                package_directories: directories,
                package_files,
                runtime_rechecks,
            },
            registry,
            &source.documents.credential,
            &source.documents.registry,
            &namespace,
            |id| {
                Ok((
                    source.open_pin(id)?,
                    tree.open(format!("servers/{id}/pinned.json").as_bytes())?,
                ))
            },
        )?;
        record.management_migrations = owner
            .admission
            .roots
            .management_migrations(id)
            .map_err(protection)?;
        record.validate()?;
        owner.verify_current()?;
        source.verify_current()?;
        tree.verify(&guard.parent)?;
        tree.fill_record(&record.encode()?)?;
        staging_fault("after-migration-record")?;
        tree.sync_bottom_up()?;
        staging_fault("after-staging-fsync")?;
        let inventory = tree.inspect(&guard.parent)?;
        let result = Self {
            tree,
            inventory,
            staging,
            record,
            runtime,
        };
        result.verify(owner)?;
        staging_fault("after-mapped-validation")?;
        Ok(result)
    }
    fn verify(&self, owner: &RetiredMigrationV2) -> Result<(), Error> {
        owner.verify_current()?;
        self.verify_parts(&owner.admission)?;
        owner.verify_current()
    }
    fn verify_parts(&self, admission: &MigrationAdmissionV2) -> Result<(), Error> {
        let guard = &admission.guard;
        self.tree.verify(&guard.parent)?;
        self.inventory.verify_current()?;
        let view = inspect_mapped_stage(guard, &self.staging, &self.inventory)?;
        let secret = self.tree.read_secret()?;
        let registry = RegistryDocumentV2::decode(&self.tree.read_public(b"servers.json")?)?;
        let (_, source_documents) = admission.source.staging_observations()?;
        Self::verify_management(admission, &self.record.management_migrations)?;
        source_documents.verify_credential_baseline(&secret)?;
        self.record.validate_credential(&secret)?;
        self.record
            .validate_registry_source(source_documents.verified_registry()?, &registry)?;
        if StoreMigrationRecordV2::decode(&self.tree.read_public(b"migration.v2.json")?)?
            != self.record
        {
            return Err(Error::Protection);
        }
        let catalog = CatalogRegenerationRecordV2::decode(
            &self.tree.read_public(b"catalog-regeneration.v2.json")?,
        )?;
        catalog.validate_registry(&registry)?;
        if self.tree.read_public(b"catalog.json")?
            != serde_json::to_vec(&catalog.empty_catalog).map_err(protection)?
        {
            return Err(Error::Protection);
        }
        if self.tree.read_public(b"ledger/head.json")?
            != empty_payload_head(&self.staging.random_epoch_id)?
            || self.tree.node(b"ledger/history.jsonl")?.metadata.len() != 0
        {
            return Err(Error::Protection);
        }
        let source_copy = admission
            .source
            .staging_observations()?
            .0
            .verify_before_copy()?;
        validate_unregistered_pin_reader(
            guard,
            &self.record,
            source_documents.verified_registry()?,
            &registry,
            |id| {
                let path = format!("servers/{id}/pinned.json");
                let entry = source_copy
                    .entries()
                    .find(|e| e.relative_path() == path.as_bytes())
                    .ok_or(Error::Protection)?;
                let source = entry.open_readonly()?;
                verify_mount(admission.source.held_root(), &source)?;
                let staged = self.tree.open(path.as_bytes())?;
                Ok((source, staged))
            },
        )?;
        source_copy.verify_after_copy()?;
        let roots = crate::mcp::local::RUNTIME_ROOTS
            .iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>();
        // Resource witnesses and the mapped inventory stay alive together until
        // every server, all source roots and actual runtime pins close.
        if let Some(registry) = registry.snapshot() {
            let (home, _, _, _) = guard.verified_binding().map_err(protection)?;
            let exclusions = admission.roots.runtime_exclusions().map_err(protection)?;
            let witness = validate_mapped_servers_no_exec(
                &view,
                registry,
                &secret,
                polaris_mcp::admission::ProjectAdmissionScope::new("", &[]),
                home,
                &roots,
                crate::mcp::local::LIMITS,
            )?;
            let mut project_witnesses = Vec::new();
            for (project, paths) in &exclusions {
                project_witnesses.push(validate_mapped_servers_no_exec(
                    &view,
                    registry,
                    &secret,
                    polaris_mcp::admission::ProjectAdmissionScope::new(project, paths),
                    home,
                    &roots,
                    crate::mcp::local::LIMITS,
                )?);
            }
            for pin in &self.runtime {
                pin.verify(&view)?;
            }
            let (records, fresh) = observe_runtime(&view, Some(registry))?;
            if records != self.record.runtime_rechecks {
                return Err(Error::Protection);
            }
            for pin in &fresh {
                pin.verify(&view)?;
            }
            for witness in &project_witnesses {
                witness.revalidate()?;
            }
            witness.revalidate()?;
        } else if !self.runtime.is_empty() || !self.record.runtime_rechecks.is_empty() {
            return Err(Error::Protection);
        }
        self.inventory.verify_current()?;
        self.tree.verify(&guard.parent)?;
        admission.source.bind_guard(guard)
    }
    fn verify_management(
        admission: &MigrationAdmissionV2,
        expected: &[ManagementMigrationBindingV2],
    ) -> Result<(), Error> {
        let (_, migration_id, _, _) = admission.guard.verified_binding().map_err(protection)?;
        if admission
            .roots
            .management_migrations(migration_id)
            .map_err(protection)?
            != expected
        {
            return Err(Error::Protection);
        }
        Ok(())
    }
}
fn admit_registry<'v, 'g>(
    owner: &RetiredMigrationV2,
    view: &'v MappedStagingViewV2<'g>,
    registry: &RegistryDocumentV2,
    secret: &CredentialDocumentV2,
    roots: &[PathBuf],
) -> Result<Vec<polaris_mcp::admission::MappedRegistryAdmissionV2<'v, 'g>>, Error> {
    let Some(registry) = registry.snapshot() else {
        return Ok(Vec::new());
    };
    let (home, _, _, _) = owner
        .admission
        .guard
        .verified_binding()
        .map_err(protection)?;
    let exclusions = owner
        .admission
        .roots
        .runtime_exclusions()
        .map_err(protection)?;
    let mut admissions = vec![validate_mapped_servers_no_exec(
        view,
        registry,
        secret,
        polaris_mcp::admission::ProjectAdmissionScope::new("", &[]),
        home,
        roots,
        crate::mcp::local::LIMITS,
    )?];
    for (project, paths) in exclusions {
        admissions.push(validate_mapped_servers_no_exec(
            view,
            registry,
            secret,
            polaris_mcp::admission::ProjectAdmissionScope::new(&project, &paths),
            home,
            roots,
            crate::mcp::local::LIMITS,
        )?);
    }
    Ok(admissions)
}
fn source_anchor(source: &SourcePayloadV2<'_>, path: &[u8]) -> Option<EntryAnchorV2> {
    source
        .inventory
        .iter()
        .find(|r| inventory_path(r) == path)
        .map(|r| match r {
            SourceInventoryEntryV2::Directory { anchor, .. }
            | SourceInventoryEntryV2::PublicFile { anchor, .. }
            | SourceInventoryEntryV2::SecretDocument { anchor, .. } => *anchor,
        })
}
fn inventory_path(row: &SourceInventoryEntryV2) -> &[u8] {
    match row {
        SourceInventoryEntryV2::Directory { relative_path, .. }
        | SourceInventoryEntryV2::PublicFile { relative_path, .. }
        | SourceInventoryEntryV2::SecretDocument { relative_path, .. } => relative_path,
    }
}
fn pin_id(path: &[u8]) -> Option<&str> {
    let mut parts = path.split(|b| *b == b'/');
    if parts.next()? != b"servers" {
        return None;
    }
    let id = std::str::from_utf8(parts.next()?).ok()?;
    (parts.next()? == b"pinned.json" && parts.next().is_none()).then_some(id)
}
struct StageNode {
    // Directory FDs pin topology. Regular package files are closed after fsync
    // and reopened from these parents against their original full stamp.
    file: Option<File>,
    metadata: Metadata,
}
impl StageNode {
    fn held(&self) -> Result<&File, Error> {
        self.file.as_ref().ok_or(Error::Protocol)
    }
}
struct StageTree {
    name: String,
    nodes: BTreeMap<Vec<u8>, StageNode>,
    directory_modes: BTreeMap<Vec<u8>, u32>,
}
impl StageTree {
    fn create(parent: &File, name: &str) -> Result<Self, Error> {
        absent(parent, name)?;
        let name_c = CString::new(name).map_err(protection)?;
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name_c.as_ptr(), 0o700) } != 0 {
            return Err(Error::Storage);
        }
        let root = child(parent, name, libc::O_RDONLY | libc::O_DIRECTORY).map_err(protection)?;
        super::super::super::entry_gate::owned_directory(&root, true).map_err(protection)?;
        verify_mount(parent, &root)?;
        parent.sync_all().map_err(storage)?;
        Ok(Self {
            name: name.into(),
            directory_modes: BTreeMap::new(),
            nodes: BTreeMap::from([(
                Vec::new(),
                StageNode {
                    metadata: root.metadata().map_err(storage)?,
                    file: Some(root),
                },
            )]),
        })
    }
    fn root(&self) -> &File {
        self.nodes[&Vec::new()]
            .file
            .as_ref()
            .expect("held stage root")
    }
    fn node(&self, path: &[u8]) -> Result<&StageNode, Error> {
        self.nodes.get(path).ok_or(Error::Protocol)
    }
    fn open(&self, path: &[u8]) -> Result<File, Error> {
        let node = self.node(path)?;
        let file = if path.is_empty() {
            self.root().try_clone().map_err(storage)?
        } else {
            let (parent_path, name) = Self::parent_path(path);
            child_bytes(self.node(parent_path)?.held()?, name, libc::O_RDONLY)
                .map_err(protection)?
        };
        verify_mount(self.root(), &file)?;
        if !same_metadata(&node.metadata, &file.metadata().map_err(storage)?) {
            return Err(Error::Protection);
        }
        Ok(file)
    }
    fn parent_path(path: &[u8]) -> (&[u8], &[u8]) {
        path.iter()
            .rposition(|b| *b == b'/')
            .map_or((b"".as_slice(), path), |i| (&path[..i], &path[i + 1..]))
    }
    fn directory(&mut self, path: &[u8], mode: u32) -> Result<(), Error> {
        let (parent_path, name) = Self::parent_path(path);
        let parent = self.node(parent_path)?;
        if !same_metadata(
            &parent.metadata,
            &parent.held()?.metadata().map_err(storage)?,
        ) {
            return Err(Error::Protection);
        }
        let name_c = CString::new(name).map_err(protection)?;
        if unsafe { libc::mkdirat(parent.held()?.as_raw_fd(), name_c.as_ptr(), 0o700) } != 0 {
            return Err(Error::Storage);
        }
        let file = child_bytes(parent.held()?, name, libc::O_RDONLY | libc::O_DIRECTORY)
            .map_err(protection)?;
        verify_mount(self.root(), &file)?;
        let metadata = file.metadata().map_err(storage)?;
        let parent_metadata = parent.held()?.metadata().map_err(storage)?;
        self.nodes
            .get_mut(parent_path)
            .ok_or(Error::Protocol)?
            .metadata = parent_metadata;
        self.nodes.insert(
            path.into(),
            StageNode {
                file: Some(file),
                metadata,
            },
        );
        self.directory_modes.insert(path.into(), mode & 0o777);
        Ok(())
    }
    fn finalize_directories(&mut self) -> Result<(), Error> {
        for (path, mode) in &self.directory_modes {
            let node = self.nodes.get_mut(path).ok_or(Error::Protocol)?;
            if !same_metadata(&node.metadata, &node.held()?.metadata().map_err(storage)?) {
                return Err(Error::Protection);
            }
            node.held()?
                .set_permissions(Permissions::from_mode(*mode))
                .map_err(storage)?;
            node.metadata = node.held()?.metadata().map_err(storage)?;
        }
        self.directory_modes.clear();
        Ok(())
    }
    fn bytes(&mut self, path: &[u8], bytes: &[u8], mode: u32) -> Result<(), Error> {
        self.write_file(path, mode, |file| {
            file.write_all_at(bytes, 0).map_err(storage)
        })
    }
    fn copy(&mut self, path: &[u8], source: &File, mode: u32) -> Result<(), Error> {
        let metadata = source.metadata().map_err(storage)?;
        self.write_file(path, mode, |file| {
            let mut offset = 0;
            let mut bytes = [0; 32 * 1024];
            while offset < metadata.len() {
                let limit = (metadata.len() - offset).min(bytes.len() as u64) as usize;
                let n = source
                    .read_at(&mut bytes[..limit], offset)
                    .map_err(storage)?;
                if n == 0 {
                    return Err(Error::Protection);
                }
                file.write_all_at(&bytes[..n], offset).map_err(storage)?;
                offset += n as u64;
            }
            if source.read_at(&mut bytes[..1], offset).map_err(storage)? != 0
                || !same_metadata(&metadata, &source.metadata().map_err(storage)?)
            {
                return Err(Error::Protection);
            }
            Ok(())
        })
    }
    fn write_file(
        &mut self,
        path: &[u8],
        mode: u32,
        write: impl FnOnce(&File) -> Result<(), Error>,
    ) -> Result<(), Error> {
        let (parent_path, name) = Self::parent_path(path);
        let parent = self.node(parent_path)?;
        if !same_metadata(
            &parent.metadata,
            &parent.held()?.metadata().map_err(storage)?,
        ) {
            return Err(Error::Protection);
        }
        let file = child_bytes(
            parent.held()?,
            name,
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL,
        )
        .map_err(protection)?;
        private_file(&file).map_err(protection)?;
        verify_mount(self.root(), &file)?;
        staging_fault("before-staging-file-body")?;
        write(&file)?;
        file.set_permissions(Permissions::from_mode(mode & 0o777))
            .map_err(storage)?;
        file.sync_all().map_err(storage)?;
        verify_mount(self.root(), &file)?;
        let metadata = file.metadata().map_err(storage)?;
        let parent_metadata = parent.held()?.metadata().map_err(storage)?;
        self.nodes
            .get_mut(parent_path)
            .ok_or(Error::Protocol)?
            .metadata = parent_metadata;
        // Only the fixed migration document needs its writable descriptor later.
        let file = (path == b"migration.v2.json").then_some(file);
        self.nodes.insert(path.into(), StageNode { file, metadata });
        Ok(())
    }
    fn fill_record(&mut self, bytes: &[u8]) -> Result<(), Error> {
        let node = self
            .nodes
            .get_mut(b"migration.v2.json".as_slice())
            .ok_or(Error::Protocol)?;
        if node.metadata.len() != 0
            || !same_metadata(&node.metadata, &node.held()?.metadata().map_err(storage)?)
        {
            return Err(Error::Protection);
        }
        node.held()?.write_all_at(bytes, 0).map_err(storage)?;
        node.held()?.sync_all().map_err(storage)?;
        node.metadata = node.held()?.metadata().map_err(storage)?;
        Ok(())
    }
    fn sync_bottom_up(&self) -> Result<(), Error> {
        for node in self.nodes.values().rev() {
            if let Some(file) = &node.file {
                file.sync_all().map_err(storage)?;
            }
        }
        Ok(())
    }
    fn verify(&self, parent: &File) -> Result<(), Error> {
        let root =
            child(parent, &self.name, libc::O_RDONLY | libc::O_DIRECTORY).map_err(protection)?;
        verify_mount(parent, self.root())?;
        verify_mount(parent, &root)?;
        if !same_metadata(
            &self.node(b"")?.metadata,
            &root.metadata().map_err(storage)?,
        ) {
            return Err(Error::Protection);
        }
        for (path, node) in &self.nodes {
            let named = self.open(path)?;
            if let Some(file) = &node.file {
                verify_mount(self.root(), file)?;
                if !same_metadata(&node.metadata, &file.metadata().map_err(storage)?) {
                    return Err(Error::Protection);
                }
            }
            drop(named);
        }
        Ok(())
    }
    fn inspect(&self, parent: &File) -> Result<CompleteTreeProjectionV2, Error> {
        self.verify(parent)?;
        let tree = CompleteTreeProjectionV2::inspect(self.root(), ProjectionTreeV2::ActivationV2)?;
        let copy = tree.verify_before_copy()?;
        if !copy.entries().map(|e| e.relative_path().to_vec()).eq(self
            .nodes
            .keys()
            .filter(|p| !p.is_empty())
            .cloned())
        {
            return Err(Error::Protection);
        }
        copy.verify_after_copy()?;
        self.verify(parent)?;
        Ok(tree)
    }
    fn read_public(&self, path: &[u8]) -> Result<Vec<u8>, Error> {
        if path == b"secrets.json" {
            return Err(Error::Protection);
        }
        let node = self.node(path)?;
        if path == b"servers.json"
            && node.metadata.len() > polaris_mcp::registration::MAX_STORE_BYTES as u64
        {
            return Err(Error::Limit);
        }
        let mut bytes = vec![0; usize::try_from(node.metadata.len()).map_err(|_| Error::Limit)?];
        let file = self.open(path)?;
        verify_mount(self.root(), &file)?;
        file.read_exact_at(&mut bytes, 0).map_err(storage)?;
        if !same_metadata(&node.metadata, &file.metadata().map_err(storage)?) {
            return Err(Error::Protection);
        }
        verify_mount(self.root(), &file)?;
        Ok(bytes)
    }
    fn read_secret(&self) -> Result<CredentialDocumentV2, Error> {
        let node = self.node(b"secrets.json")?;
        if node.metadata.len() > polaris_mcp::registration::MAX_STORE_BYTES as u64 {
            return Err(Error::Limit);
        }
        let file = self.open(b"secrets.json")?;
        let document = read_baseline_credential(self.root(), &file)?;
        verify_mount(self.root(), &file)?;
        if !same_metadata(&node.metadata, &file.metadata().map_err(storage)?) {
            return Err(Error::Protection);
        }
        Ok(document)
    }
}

struct RuntimePin {
    path: PathBuf,
    file: File,
    metadata: Metadata,
    mount: NativeMountIdentityV2,
}
impl RuntimePin {
    fn open(view: &MappedStagingViewV2<'_>, path: &Path) -> Result<Self, Error> {
        let file = if view.contains(path) {
            view.open_readonly(path)?
        } else {
            if path.canonicalize().map_err(protection)? != path {
                return Err(Error::Protection);
            }
            polaris_tools::entry_kernel::physical_file(path).map_err(protection)?
        };
        let mount = NativeMountIdentityV2::inspect(&file).map_err(protection)?;
        let metadata = file.metadata().map_err(storage)?;
        let pin = Self {
            path: path.into(),
            file,
            metadata,
            mount,
        };
        pin.verify(view)?;
        Ok(pin)
    }
    fn verify(&self, view: &MappedStagingViewV2<'_>) -> Result<(), Error> {
        let named = if view.contains(&self.path) {
            view.open_readonly(&self.path)?
        } else {
            if self.path.canonicalize().map_err(protection)? != self.path {
                return Err(Error::Protection);
            }
            polaris_tools::entry_kernel::physical_file(&self.path).map_err(protection)?
        };
        for file in [&self.file, &named] {
            if NativeMountIdentityV2::inspect(file).map_err(protection)? != self.mount
                || !same_metadata(&self.metadata, &file.metadata().map_err(storage)?)
            {
                return Err(Error::Protection);
            }
        }
        Ok(())
    }
}
fn observe_runtime(
    view: &MappedStagingViewV2<'_>,
    registry: Option<&Registry>,
) -> Result<(Vec<ServerRuntimeRecheckV2>, Vec<RuntimePin>), Error> {
    let mut rows = Vec::new();
    let mut pins = Vec::new();
    if let Some(registry) = registry {
        for server in &registry.servers {
            let Destination::Stdio {
                command,
                script,
                readable_roots,
                ..
            } = &server.transport
            else {
                continue;
            };
            let mut resources = Vec::new();
            let pin = RuntimePin::open(view, command)?;
            resources.push(RuntimeResourceRecheckV2::Command {
                logical_path: command.clone(),
                anchor: anchor(&pin.file)?,
                sha256: public_hash(&pin.file, &pin.metadata)?,
            });
            pin.verify(view)?;
            pins.push(pin);
            if let Some(script) = script {
                let pin = RuntimePin::open(view, script)?;
                resources.push(RuntimeResourceRecheckV2::Script {
                    logical_path: script.clone(),
                    anchor: anchor(&pin.file)?,
                    sha256: public_hash(&pin.file, &pin.metadata)?,
                });
                pin.verify(view)?;
                pins.push(pin);
            }
            for (index, path) in readable_roots.iter().enumerate() {
                let pin = RuntimePin::open(view, path)?;
                resources.push(RuntimeResourceRecheckV2::ReadableRoot {
                    index,
                    logical_path: path.clone(),
                    anchor: anchor(&pin.file)?,
                });
                pins.push(pin);
            }
            rows.push(ServerRuntimeRecheckV2 {
                server_id: server.server_id.clone(),
                new_destination_hash: server.destination_hash()?,
                resources,
            });
        }
    }
    for pin in &pins {
        pin.verify(view)?;
    }
    rows.sort_by(|a, b| a.server_id.cmp(&b.server_id));
    Ok((rows, pins))
}
fn staging_fault(point: &'static str) -> Result<(), Error> {
    #[cfg(test)]
    if let Some(hook) = STAGING_FAULT.with(|slot| slot.borrow_mut().take()) {
        let result = hook(point);
        STAGING_FAULT.with(|slot| *slot.borrow_mut() = Some(hook));
        return result;
    }
    let _ = point;
    Ok(())
}
#[cfg(test)]
type StagingFaultHook = Box<dyn Fn(&'static str) -> Result<(), Error>>;

#[cfg(test)]
thread_local! {
    static STAGING_FAULT: std::cell::RefCell<Option<StagingFaultHook>> = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
mod tests;
