//! Closed, non-secret records for the approved store migration.
//!
//! These codecs describe observations, not stop or mutation capabilities. The
//! owner must establish live entry exclusion, descriptor provenance, complete
//! inventories, direct secret equality, durability and admission independently.
//! No activation/epoch/intent hash is stored inside the activation payload.

use super::paths::{
    ManagedPathFieldMappingV2, ManagedPathFieldV2, ManagedPathProjectionV2, PinnedPathsV1,
    transform_unregistered_pin_v2,
};
use crate::{
    Error,
    catalog::index::CatalogIndex,
    entry_gate::{
        EntryAnchorV2, ProductionBinaryIdentityV2, ProductionEntryKindV2,
        ProductionEntryManifestV2, valid_digest,
    },
    registration::{Destination, MAX_JSON_BYTES, Registry, canonical_hash, valid_id},
    store::{
        documents_v2::{
            CredentialDocumentV2, DocumentKindV2, DocumentMetadataV2, DocumentPresenceV2,
            DocumentReceiptV2, MigrationBaselineReceiptV1, RegistryDocumentV2,
        },
        ledger_v1::{LedgerEpochV1, RetiredLedgerAnchorV1, TombstoneLedgerAnchorV1},
    },
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{File, Metadata},
    io,
    os::unix::fs::{FileExt, MetadataExt},
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ProtocolSiblingV2 {
    SourceV1 {},
    RetiredV1 { migration_id: String },
    StagingV2 { migration_id: String },
    ActiveV2 {},
}
impl ProtocolSiblingV2 {
    pub fn component(&self) -> String {
        match self {
            Self::SourceV1 {} => "mcp".into(),
            Self::RetiredV1 { migration_id } => format!("mcp.retired-{migration_id}"),
            Self::StagingV2 { migration_id } => format!("mcp-v2.staging-{migration_id}"),
            Self::ActiveV2 {} => "mcp-v2".into(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProtocolLayoutV2 {
    pub source: ProtocolSiblingV2,
    pub retired: ProtocolSiblingV2,
    pub staging: ProtocolSiblingV2,
    pub active: ProtocolSiblingV2,
}
impl ProtocolLayoutV2 {
    pub fn new(migration_id: &str) -> Result<Self, Error> {
        if !valid_digest(migration_id) {
            return Err(Error::Protocol);
        }
        Ok(Self {
            source: ProtocolSiblingV2::SourceV1 {},
            retired: ProtocolSiblingV2::RetiredV1 {
                migration_id: migration_id.into(),
            },
            staging: ProtocolSiblingV2::StagingV2 {
                migration_id: migration_id.into(),
            },
            active: ProtocolSiblingV2::ActiveV2 {},
        })
    }
    fn validate(&self, migration_id: &str) -> Result<(), Error> {
        if *self != Self::new(migration_id)? {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}

/// The secret variant cannot carry file size, content hash or secret values.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum SourceInventoryEntryV2 {
    Directory {
        relative_path: Vec<u8>,
        anchor: EntryAnchorV2,
    },
    PublicFile {
        relative_path: Vec<u8>,
        anchor: EntryAnchorV2,
        size: u64,
        sha256: String,
    },
    SecretDocument {
        relative_path: Vec<u8>,
        anchor: EntryAnchorV2,
    },
}
impl SourceInventoryEntryV2 {
    fn path(&self) -> &[u8] {
        match self {
            Self::Directory { relative_path, .. }
            | Self::PublicFile { relative_path, .. }
            | Self::SecretDocument { relative_path, .. } => relative_path,
        }
    }
    fn anchor(&self) -> &EntryAnchorV2 {
        match self {
            Self::Directory { anchor, .. }
            | Self::PublicFile { anchor, .. }
            | Self::SecretDocument { anchor, .. } => anchor,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaselineDocumentRecordV2 {
    pub source_anchor: Option<EntryAnchorV2>,
    pub staged_anchor: EntryAnchorV2,
    pub metadata: DocumentMetadataV2,
    pub receipt: MigrationBaselineReceiptV1,
}
impl BaselineDocumentRecordV2 {
    pub fn credential(
        document: &CredentialDocumentV2,
        source_anchor: Option<EntryAnchorV2>,
        staged_anchor: EntryAnchorV2,
    ) -> Result<Self, Error> {
        Self::from_document(
            document.metadata()?,
            document.receipt(),
            source_anchor,
            staged_anchor,
        )
    }
    pub fn registry(
        document: &RegistryDocumentV2,
        source_anchor: Option<EntryAnchorV2>,
        staged_anchor: EntryAnchorV2,
    ) -> Result<Self, Error> {
        Self::from_document(
            document.metadata()?,
            document.receipt(),
            source_anchor,
            staged_anchor,
        )
    }
    fn from_document(
        metadata: DocumentMetadataV2,
        receipt: &DocumentReceiptV2,
        source_anchor: Option<EntryAnchorV2>,
        staged_anchor: EntryAnchorV2,
    ) -> Result<Self, Error> {
        let DocumentReceiptV2::MigrationBaseline { receipt } = receipt else {
            return Err(Error::Protocol);
        };
        let result = Self {
            source_anchor,
            staged_anchor,
            metadata,
            receipt: receipt.clone(),
        };
        result.validate(&result.receipt.migration_id, result.metadata.kind)?;
        Ok(result)
    }
    fn validate(&self, migration_id: &str, kind: DocumentKindV2) -> Result<(), Error> {
        let receipt = &self.receipt;
        self.metadata.validate()?;
        if self.metadata.kind != kind
            || receipt.document_kind != kind
            || receipt.migration_id != migration_id
            || !valid_digest(migration_id)
            || receipt.source_presence != self.metadata.presence
            || receipt.preserved_revision != self.metadata.revision
            || receipt.random_token != self.metadata.token
            || !valid_digest(&receipt.reference_set_hash)
            || receipt.source_schema_version
                != match self.metadata.presence {
                    DocumentPresenceV2::Absent => None,
                    DocumentPresenceV2::Present => Some(1),
                }
            || self.source_anchor.is_some()
                != (self.metadata.presence == DocumentPresenceV2::Present)
            || !document_file(&self.staged_anchor)
            || self
                .source_anchor
                .as_ref()
                .is_some_and(|a| !document_file(a))
            || (DocumentReceiptV2::MigrationBaseline {
                receipt: receipt.clone(),
            })
            .canonical_hash()?
                != self.metadata.receipt_hash
        {
            return Err(Error::Protocol);
        }
        match (kind, &receipt.registry_projection) {
            (DocumentKindV2::Credential, None) => {}
            (DocumentKindV2::Registry, Some(projection))
                if valid_digest(&projection.canonical_payload_hash)
                    && projection.source_file_hash.is_some()
                        == (self.metadata.presence == DocumentPresenceV2::Present)
                    && projection
                        .source_file_hash
                        .as_ref()
                        .is_none_or(|v| valid_digest(v)) => {}
            _ => return Err(Error::Protocol),
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageFileMappingV2 {
    pub relative_path: Vec<u8>,
    pub source_anchor: EntryAnchorV2,
    pub source_size: u64,
    pub source_sha256: String,
    pub staged_anchor: EntryAnchorV2,
    pub staged_size: u64,
    pub staged_sha256: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageDirectoryMappingV2 {
    pub relative_path: Vec<u8>,
    pub source_anchor: EntryAnchorV2,
    pub staged_anchor: EntryAnchorV2,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UnregisteredPinMappingV2 {
    pub server_id: String,
    pub fields: [ManagedPathFieldMappingV2; 2],
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RuntimeResourceRecheckV2 {
    Command {
        logical_path: PathBuf,
        anchor: EntryAnchorV2,
        sha256: String,
    },
    Script {
        logical_path: PathBuf,
        anchor: EntryAnchorV2,
        sha256: String,
    },
    ReadableRoot {
        index: usize,
        logical_path: PathBuf,
        anchor: EntryAnchorV2,
    },
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ServerRuntimeRecheckV2 {
    pub server_id: String,
    pub new_destination_hash: String,
    pub resources: Vec<RuntimeResourceRecheckV2>,
}

/// Pre-epoch root binding copied from the Staging observation. External intent
/// hashes are deliberately absent: this object is itself activation payload.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreRootBindingV2 {
    pub migration_id: String,
    pub entry_manifest_hash: String,
    pub store_identity: String,
    pub source_store_identity: Option<String>,
    pub layout: ProtocolLayoutV2,
    pub canonical_parent_anchor: EntryAnchorV2,
    pub source_root_anchor: EntryAnchorV2,
    pub retired_source: RetiredLedgerAnchorV1,
    pub tombstone: TombstoneLedgerAnchorV1,
    pub staging_anchor: EntryAnchorV2,
    pub random_epoch_id: String,
}
impl StoreRootBindingV2 {
    fn from_staging(staging: &StagingRecordV2) -> Result<Self, Error> {
        staging.validate()?;
        let retired = &staging.retired;
        let common = &retired.binding;
        Ok(Self {
            migration_id: common.migration_id.clone(),
            entry_manifest_hash: common.entry_manifest_hash.clone(),
            store_identity: common.store_identity.clone(),
            source_store_identity: common.source_store_identity.clone(),
            layout: common.layout.clone(),
            canonical_parent_anchor: staging.canonical_parent_anchor,
            source_root_anchor: common.source_root_anchor,
            retired_source: retired.retired_source.clone(),
            tombstone: retired.tombstone.clone(),
            staging_anchor: staging.staging_anchor,
            random_epoch_id: staging.random_epoch_id.clone(),
        })
    }
    fn validate(&self) -> Result<(), Error> {
        self.layout.validate(&self.migration_id)?;
        if !valid_digest(&self.entry_manifest_hash)
            || !valid_digest(&self.store_identity)
            || !valid_digest(&self.random_epoch_id)
            || self
                .source_store_identity
                .as_ref()
                .is_some_and(|v| v != &self.store_identity)
            || !parent_directory(&self.canonical_parent_anchor)
            || !root_directory(&self.source_root_anchor)
            || self.source_root_anchor != self.retired_source.anchor
            || !valid_digest(&self.retired_source.non_secret_tree_hash)
            || !document_file(&self.tombstone.anchor)
            || !valid_digest(&self.tombstone.file_hash)
            || !root_directory(&self.staging_anchor)
            || [
                &self.source_root_anchor,
                &self.tombstone.anchor,
                &self.staging_anchor,
            ]
            .into_iter()
            .any(|a| a.device != self.canonical_parent_anchor.device)
            || !distinct(&[
                &self.canonical_parent_anchor,
                &self.source_root_anchor,
                &self.tombstone.anchor,
                &self.staging_anchor,
            ])
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}

/// Non-authoritative, immutable cutover evidence. The live owner separately
/// compares every row with its retained project descriptors and exact manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagementMigrationBindingV2 {
    pub state_path: Vec<u8>,
    pub migration_id: String,
    pub project_scope: String,
    pub canonical_parent_anchor: EntryAnchorV2,
    pub retired_source_anchor: EntryAnchorV2,
    pub tombstone: TombstoneLedgerAnchorV1,
    pub planned_active_v2_root_anchor: EntryAnchorV2,
    pub migration_manifest_hash: String,
}
impl ManagementMigrationBindingV2 {
    fn validate(&self, migration_id: &str) -> Result<(), Error> {
        use std::os::unix::ffi::OsStrExt;
        require_normalized(Path::new(std::ffi::OsStr::from_bytes(&self.state_path)))?;
        let id = self
            .state_path
            .rsplit(|byte| *byte == b'/')
            .next()
            .ok_or(Error::Protocol)?;
        if id.len() != 16
            || !id
                .iter()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(b))
            || self.migration_id != migration_id
            || !valid_digest(&self.project_scope)
            || self.project_scope == "0".repeat(64)
            || !valid_digest(&self.migration_manifest_hash)
            || !valid_digest(&self.tombstone.file_hash)
            || !parent_directory(&self.canonical_parent_anchor)
            || !root_directory(&self.retired_source_anchor)
            || !root_directory(&self.planned_active_v2_root_anchor)
            || !document_file(&self.tombstone.anchor)
            || [
                &self.retired_source_anchor,
                &self.tombstone.anchor,
                &self.planned_active_v2_root_anchor,
            ]
            .iter()
            .any(|a| a.device != self.canonical_parent_anchor.device)
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}

/// This record is hashed as a payload file before epoch and external intent.
/// Deliberately no self digest, tree hash, epoch file hash or intent hash.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoreMigrationRecordV2 {
    pub schema_version: u32,
    pub binding: StoreRootBindingV2,
    pub planned_active_anchor: EntryAnchorV2,
    pub source_inventory: Vec<SourceInventoryEntryV2>,
    pub credential: BaselineDocumentRecordV2,
    pub registry: BaselineDocumentRecordV2,
    pub managed_paths: Option<ManagedPathProjectionV2>,
    pub unregistered_pins: Vec<UnregisteredPinMappingV2>,
    pub package_directories: Vec<PackageDirectoryMappingV2>,
    pub package_files: Vec<PackageFileMappingV2>,
    pub management_migrations: Vec<ManagementMigrationBindingV2>,
    pub runtime_rechecks: Vec<ServerRuntimeRecheckV2>,
}
/// Non-authoritative payload values moved into the migration record.
/// Actual descriptors are validated separately. Management bindings are supplied
/// only by the coordinator's retained project owners after construction.
pub struct StoreMigrationPayloadV2 {
    pub planned_active_anchor: EntryAnchorV2,
    pub source_inventory: Vec<SourceInventoryEntryV2>,
    pub credential: BaselineDocumentRecordV2,
    pub registry: BaselineDocumentRecordV2,
    pub managed_paths: Option<ManagedPathProjectionV2>,
    pub unregistered_pins: Vec<UnregisteredPinMappingV2>,
    pub package_directories: Vec<PackageDirectoryMappingV2>,
    pub package_files: Vec<PackageFileMappingV2>,
    pub runtime_rechecks: Vec<ServerRuntimeRecheckV2>,
}
impl StoreMigrationRecordV2 {
    /// Constructs validated payload data from actual read-only descriptors.
    /// The host separately binds its live guard before and after this call.
    pub fn new_from_descriptors(
        binding: StagingRecordV2,
        payload: StoreMigrationPayloadV2,
        source_registry: Option<&Registry>,
        actual_credential: &CredentialDocumentV2,
        actual_registry: &RegistryDocumentV2,
        namespace: &super::namespace::MigrationParentObservationV2,
        actual_pins: &BTreeMap<String, (&File, &File)>,
    ) -> Result<Self, Error> {
        if !payload
            .unregistered_pins
            .iter()
            .map(|row| row.server_id.as_str())
            .eq(actual_pins.keys().map(String::as_str))
        {
            return Err(Error::Protocol);
        }
        Self::new_from_descriptor_reader(
            binding,
            payload,
            source_registry,
            actual_credential,
            actual_registry,
            namespace,
            |id| {
                let (source, staged) = actual_pins.get(id).ok_or(Error::Protocol)?;
                Ok((
                    source.try_clone().map_err(pin_storage)?,
                    staged.try_clone().map_err(pin_storage)?,
                ))
            },
        )
    }
    pub fn new_from_descriptor_reader(
        binding: StagingRecordV2,
        payload: StoreMigrationPayloadV2,
        source_registry: Option<&Registry>,
        actual_credential: &CredentialDocumentV2,
        actual_registry: &RegistryDocumentV2,
        namespace: &super::namespace::MigrationParentObservationV2,
        actual_pins: impl FnMut(&str) -> Result<(File, File), Error>,
    ) -> Result<Self, Error> {
        let StoreMigrationPayloadV2 {
            planned_active_anchor,
            source_inventory,
            credential,
            registry,
            managed_paths,
            unregistered_pins,
            package_directories,
            package_files,
            runtime_rechecks,
        } = payload;
        let result = Self {
            schema_version: 2,
            planned_active_anchor,
            binding: StoreRootBindingV2::from_staging(&binding)?,
            source_inventory,
            credential,
            registry,
            managed_paths,
            unregistered_pins,
            package_directories,
            package_files,
            management_migrations: Vec::new(),
            runtime_rechecks,
        };
        result.validate_credential(actual_credential)?;
        result.validate_registry_source(source_registry, actual_registry)?;
        result.validate_unregistered_pin_reader(
            namespace,
            source_registry,
            actual_registry,
            actual_pins,
        )?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<(), Error> {
        self.binding.validate()?;
        let common = &self.binding;
        if self.schema_version != 2
            || !root_directory(&self.planned_active_anchor)
            || !same_directory_identity(&self.planned_active_anchor, &self.binding.staging_anchor)
        {
            return Err(Error::Protocol);
        }
        self.credential
            .validate(&common.migration_id, DocumentKindV2::Credential)?;
        self.registry
            .validate(&common.migration_id, DocumentKindV2::Registry)?;
        validate_inventory(&self.source_inventory, &common.source_root_anchor)?;
        let source_objects: BTreeSet<_> = self
            .source_inventory
            .iter()
            .map(|entry| (entry.anchor().device, entry.anchor().inode))
            .chain(std::iter::once((
                common.source_root_anchor.device,
                common.source_root_anchor.inode,
            )))
            .collect();
        let mut target_objects =
            BTreeSet::from([(common.staging_anchor.device, common.staging_anchor.inode)]);
        for anchor in [&self.credential.staged_anchor, &self.registry.staged_anchor]
            .into_iter()
            .chain(
                self.package_directories
                    .iter()
                    .map(|mapping| &mapping.staged_anchor),
            )
            .chain(
                self.package_files
                    .iter()
                    .map(|mapping| &mapping.staged_anchor),
            )
        {
            if anchor.device != common.staging_anchor.device
                || source_objects.contains(&(anchor.device, anchor.inode))
                || !target_objects.insert((anchor.device, anchor.inode))
            {
                return Err(Error::Protocol);
            }
        }
        let directory_sources: Vec<_> = self
            .source_inventory
            .iter()
            .filter_map(|entry| match entry {
                SourceInventoryEntryV2::Directory {
                    relative_path,
                    anchor,
                } => Some((relative_path, anchor)),
                _ => None,
            })
            .collect();
        if directory_sources.len() != self.package_directories.len() {
            return Err(Error::Protocol);
        }
        for ((path, anchor), mapped) in directory_sources.iter().zip(&self.package_directories) {
            if mapped.relative_path != **path
                || mapped.source_anchor != **anchor
                || !protected_directory(&mapped.staged_anchor)
                || mapped.source_anchor.mode != mapped.staged_anchor.mode
                || mapped.source_anchor.uid != mapped.staged_anchor.uid
            {
                return Err(Error::Protocol);
            }
        }
        for (name, document) in [
            (b"secrets.json".as_slice(), &self.credential),
            (b"servers.json".as_slice(), &self.registry),
        ] {
            let source = self
                .source_inventory
                .iter()
                .find(|entry| entry.path() == name);
            if source.map(SourceInventoryEntryV2::anchor) != document.source_anchor.as_ref() {
                return Err(Error::Protocol);
            }
            if name == b"servers.json" {
                let raw_hash = source.and_then(|entry| match entry {
                    SourceInventoryEntryV2::PublicFile { sha256, .. } => Some(sha256),
                    _ => None,
                });
                if raw_hash
                    != document
                        .receipt
                        .registry_projection
                        .as_ref()
                        .ok_or(Error::Protocol)?
                        .source_file_hash
                        .as_ref()
                {
                    return Err(Error::Protocol);
                }
            }
        }
        let package_sources: Vec<_> = self
            .source_inventory
            .iter()
            .filter_map(|entry| match entry {
                SourceInventoryEntryV2::PublicFile {
                    relative_path,
                    anchor,
                    size,
                    sha256,
                } if relative_path.starts_with(b"servers/") => {
                    Some((relative_path, anchor, size, sha256))
                }
                _ => None,
            })
            .collect();
        if package_sources.len() != self.package_files.len() {
            return Err(Error::Protocol);
        }
        self.validate_pin_rows()?;
        for ((path, anchor, size, sha256), mapped) in
            package_sources.iter().zip(&self.package_files)
        {
            if mapped.relative_path != **path
                || mapped.source_anchor != **anchor
                || mapped.source_size != **size
                || mapped.source_sha256 != **sha256
                || !protected_regular(&mapped.staged_anchor)
                || !valid_digest(&mapped.staged_sha256)
                || mapped.staged_anchor.device != self.binding.staging_anchor.device
                || mapped.source_anchor.mode != mapped.staged_anchor.mode
                || mapped.source_anchor.uid != mapped.staged_anchor.uid
                || (!pin_server_id(path).is_some_and(|id| {
                    self.unregistered_pins.iter().any(|row| row.server_id == id)
                        || self.managed_paths.as_ref().is_some_and(|paths| {
                            paths.servers.iter().any(|server| {
                                server.server_id == id
                                    && server.fields.len() >= 2
                                    && matches!(
                                        server.fields[server.fields.len() - 2].field,
                                        ManagedPathFieldV2::PinRuntime
                                    )
                                    && matches!(
                                        server.fields[server.fields.len() - 1].field,
                                        ManagedPathFieldV2::PinEntry
                                    )
                            })
                        })
                }) && (mapped.source_size != mapped.staged_size
                    || mapped.source_sha256 != mapped.staged_sha256))
            {
                return Err(Error::Protocol);
            }
        }
        match &self.managed_paths {
            None if self.registry.metadata.presence == DocumentPresenceV2::Absent => {}
            Some(paths) if self.registry.metadata.presence == DocumentPresenceV2::Present => {
                validate_paths(paths)?;
                if paths.new_registry_payload_hash
                    != self
                        .registry
                        .receipt
                        .registry_projection
                        .as_ref()
                        .ok_or(Error::Protocol)?
                        .canonical_payload_hash
                {
                    return Err(Error::Protocol);
                }
            }
            _ => return Err(Error::Protocol),
        }
        validate_runtime_shape(&self.runtime_rechecks)?;
        if self
            .management_migrations
            .windows(2)
            .any(|rows| rows[0].state_path >= rows[1].state_path)
        {
            return Err(Error::Protocol);
        }
        // Include every global/source/package object, not only stage objects.
        // A row is not allowed to alias either another project or this store.
        let mut objects = source_objects;
        objects.extend(target_objects);
        objects.insert((
            common.canonical_parent_anchor.device,
            common.canonical_parent_anchor.inode,
        ));
        objects.insert((
            common.tombstone.anchor.device,
            common.tombstone.anchor.inode,
        ));
        for row in &self.management_migrations {
            row.validate(&common.migration_id)?;
            for anchor in [
                &row.canonical_parent_anchor,
                &row.retired_source_anchor,
                &row.tombstone.anchor,
                &row.planned_active_v2_root_anchor,
            ] {
                if !objects.insert((anchor.device, anchor.inode)) {
                    return Err(Error::Protocol);
                }
            }
        }
        Ok(())
    }
    fn unregistered_pin_files(&self) -> BTreeMap<&str, &PackageFileMappingV2> {
        self.package_files
            .iter()
            .filter_map(|file| {
                let id = pin_server_id(&file.relative_path)?;
                if self
                    .managed_paths
                    .as_ref()
                    .is_some_and(|paths| paths.servers.iter().any(|server| server.server_id == id))
                {
                    None
                } else {
                    Some((id, file))
                }
            })
            .collect()
    }
    fn validate_pin_rows(&self) -> Result<(), Error> {
        if self
            .unregistered_pins
            .windows(2)
            .any(|rows| rows[0].server_id >= rows[1].server_id)
            || self.unregistered_pins.iter().any(|row| {
                !valid_id(&row.server_id)
                    || !matches!(row.fields[0].field, ManagedPathFieldV2::PinRuntime)
                    || !matches!(row.fields[1].field, ManagedPathFieldV2::PinEntry)
                    || row.fields.iter().any(|field| {
                        !valid_digest(&field.old_value_hash) || !valid_digest(&field.new_value_hash)
                    })
            })
            || !self.unregistered_pin_files().keys().copied().eq(self
                .unregistered_pins
                .iter()
                .map(|row| row.server_id.as_str()))
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
    /// Re-read actual source/staged pin descriptors at construction and again
    /// after decode. The owner retains all named/ancestor/mount checks; this
    /// comparison establishes no stop, mutation or publication capability.
    /// Actual descriptor comparison without stop or entry authority. The host
    /// owner must reverify its private entry guard before and after this read.
    pub fn validate_unregistered_pin_descriptors(
        &self,
        namespace: &super::namespace::MigrationParentObservationV2,
        source_registry: Option<&Registry>,
        actual_registry: &RegistryDocumentV2,
        pins: &BTreeMap<String, (&File, &File)>,
    ) -> Result<(), Error> {
        if !self
            .unregistered_pin_files()
            .keys()
            .copied()
            .eq(pins.keys().map(String::as_str))
        {
            return Err(Error::Protocol);
        }
        self.validate_unregistered_pin_reader(namespace, source_registry, actual_registry, |id| {
            let (source, staged) = pins.get(id).ok_or(Error::Protocol)?;
            Ok((
                source.try_clone().map_err(pin_storage)?,
                staged.try_clone().map_err(pin_storage)?,
            ))
        })
    }
    /// Read-only descriptor observations; this returns no activation authority.
    /// Drop each pair after inspection and close all original metadata again.
    pub fn validate_unregistered_pin_reader(
        &self,
        namespace: &super::namespace::MigrationParentObservationV2,
        source_registry: Option<&Registry>,
        actual_registry: &RegistryDocumentV2,
        mut pins: impl FnMut(&str) -> Result<(File, File), Error>,
    ) -> Result<(), Error> {
        self.validate_registry_source(source_registry, actual_registry)?;
        namespace.verify_current()?;
        let home = namespace.logical_home();
        if !same_directory_identity(
            &EntryAnchorV2::of(&namespace.parent().metadata().map_err(pin_storage)?),
            &self.binding.canonical_parent_anchor,
        ) {
            return Err(Error::Protocol);
        }
        let files = self.unregistered_pin_files();
        let mut held = Vec::with_capacity(files.len());
        for row in &self.unregistered_pins {
            let mapping = files.get(row.server_id.as_str()).ok_or(Error::Protocol)?;
            let (source, staged) = pins(&row.server_id)?;
            let (source_metadata, source_bytes) = read_pin(
                &source,
                &mapping.source_anchor,
                mapping.source_size,
                &mapping.source_sha256,
                None,
            )?;
            let (staged_metadata, staged_bytes) = read_pin(
                &staged,
                &mapping.staged_anchor,
                mapping.staged_size,
                &mapping.staged_sha256,
                None,
            )?;
            let mapped = transform_unregistered_pin_v2(
                source_registry,
                &row.server_id,
                &source_bytes,
                home,
            )?;
            let actual: PinnedPathsV1 =
                serde_json::from_slice(&staged_bytes).map_err(|_| Error::InvalidRegistration)?;
            if mapped.fields.as_slice() != row.fields.as_slice() || mapped.pin != actual {
                return Err(Error::Protocol);
            }
            held.push((
                row.server_id.clone(),
                *mapping,
                source_metadata,
                staged_metadata,
            ));
        }
        // Re-read every pair so a late writer cannot change an earlier pin
        // while a later pin is being transformed. Keep the original metadata.
        for (id, mapping, before_source, before_staged) in &held {
            let (source, staged) = pins(id)?;
            read_pin(
                &source,
                &mapping.source_anchor,
                mapping.source_size,
                &mapping.source_sha256,
                Some(before_source),
            )?;
            read_pin(
                &staged,
                &mapping.staged_anchor,
                mapping.staged_size,
                &mapping.staged_sha256,
                Some(before_staged),
            )?;
        }
        for (id, _, before_source, before_staged) in &held {
            let (source, staged) = pins(id)?;
            if !stable_pin(before_source, &source.metadata().map_err(pin_storage)?)
                || !stable_pin(before_staged, &staged.metadata().map_err(pin_storage)?)
            {
                return Err(Error::Protection);
            }
        }
        namespace.verify_current()?;
        if !same_directory_identity(
            &EntryAnchorV2::of(&namespace.parent().metadata().map_err(pin_storage)?),
            &self.binding.canonical_parent_anchor,
        ) {
            return Err(Error::Protection);
        }
        Ok(())
    }
    /// Read-back/restart must bind both actual documents again. This metadata
    /// and receipt comparison adds no secret value, size or content digest.
    pub fn validate_credential(&self, actual: &CredentialDocumentV2) -> Result<(), Error> {
        self.validate()?;
        if actual.metadata()? != self.credential.metadata
            || actual.receipt()
                != &(DocumentReceiptV2::MigrationBaseline {
                    receipt: self.credential.receipt.clone(),
                })
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
    /// Actual baseline binding is required again after reading this record.
    pub fn validate_registry(&self, actual: &RegistryDocumentV2) -> Result<(), Error> {
        self.validate()?;
        if actual.metadata()? != self.registry.metadata
            || actual.receipt()
                != &(DocumentReceiptV2::MigrationBaseline {
                    receipt: self.registry.receipt.clone(),
                })
        {
            return Err(Error::Protocol);
        }
        let servers = actual.snapshot().map_or(&[][..], |r| r.servers.as_slice());
        if let Some(paths) = &self.managed_paths {
            if paths.servers.len() != servers.len() {
                return Err(Error::Protocol);
            }
            for (mapping, server) in paths.servers.iter().zip(servers) {
                if mapping.server_id != server.server_id
                    || mapping.new_destination_hash != server.destination_hash()?
                {
                    return Err(Error::Protocol);
                }
            }
        }
        let expected: BTreeMap<_, _> = servers
            .iter()
            .filter(|s| matches!(s.transport, Destination::Stdio { .. }))
            .map(|s| (s.server_id.as_str(), s))
            .collect();
        if expected.len() != self.runtime_rechecks.len() {
            return Err(Error::Protocol);
        }
        for row in &self.runtime_rechecks {
            let server = expected
                .get(row.server_id.as_str())
                .ok_or(Error::Protocol)?;
            if row.new_destination_hash != server.destination_hash()? {
                return Err(Error::Protocol);
            }
            let Destination::Stdio {
                command,
                script,
                executable_sha256,
                script_sha256,
                readable_roots,
                ..
            } = &server.transport
            else {
                return Err(Error::Protocol);
            };
            let mut expected_resources = Vec::new();
            expected_resources.push((0, 0, command, executable_sha256.as_deref()));
            if let Some(script) = script {
                expected_resources.push((1, 0, script, script_sha256.as_deref()));
            }
            expected_resources.extend(
                readable_roots
                    .iter()
                    .enumerate()
                    .map(|(i, path)| (2, i, path, None)),
            );
            if expected_resources.len() != row.resources.len() {
                return Err(Error::Protocol);
            }
            for (resource, (role, index, path, expected_hash)) in
                row.resources.iter().zip(expected_resources)
            {
                let (actual_role, actual_index, actual_path, actual_hash) = runtime_parts(resource);
                if actual_role != role
                    || actual_index != index
                    || actual_path != path
                    || expected_hash.is_some_and(|hash| actual_hash != Some(hash))
                {
                    return Err(Error::Protocol);
                }
            }
        }
        Ok(())
    }
    pub fn validate_registry_source(
        &self,
        source: Option<&Registry>,
        actual: &RegistryDocumentV2,
    ) -> Result<(), Error> {
        self.validate_registry(actual)?;
        match (source, actual.snapshot(), &self.managed_paths) {
            (None, None, None) => Ok(()),
            (Some(source), Some(new), Some(paths)) => {
                source.validate()?;
                if source.revision != new.revision
                    || source.servers.len() != paths.servers.len()
                    || paths.source_registry_payload_hash != hash_bytes(&encode(&Some(source))?)
                {
                    return Err(Error::Protocol);
                }
                for ((server, mapped), mapping) in
                    source.servers.iter().zip(&new.servers).zip(&paths.servers)
                {
                    if server.server_id != mapping.server_id
                        || server.destination_hash()? != mapping.old_destination_hash
                    {
                        return Err(Error::Protocol);
                    }
                    let mut metadata_only = server.clone();
                    metadata_only.transport = mapped.transport.clone();
                    if metadata_only != *mapped
                        || std::mem::discriminant(&server.transport)
                            != std::mem::discriminant(&mapped.transport)
                        || (matches!(server.transport, Destination::StreamableHttp { .. })
                            && (server.transport != mapped.transport || !mapping.fields.is_empty()))
                    {
                        return Err(Error::Protocol);
                    }
                    validate_transport_projection(
                        &server.transport,
                        &mapped.transport,
                        &mapping.fields,
                    )?;
                }
                Ok(())
            }
            _ => Err(Error::Protocol),
        }
    }
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        encode(self)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let result: Self = decode(bytes)?;
        result.validate()?;
        Ok(result)
    }
    pub fn canonical_hash(&self) -> Result<String, Error> {
        Ok(hash_bytes(&self.encode()?))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CatalogRegenerationStateV2 {
    CatalogRegenerationRequired,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogRegenerationServerV2 {
    pub server_id: String,
    pub new_destination_hash: String,
    pub state: CatalogRegenerationStateV2,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CatalogRegenerationRecordV2 {
    pub schema_version: u32,
    pub migration_id: String,
    pub generation: String,
    pub registry_baseline: DocumentMetadataV2,
    pub new_registry_payload_hash: String,
    pub empty_catalog: CatalogIndex,
    pub servers: Vec<CatalogRegenerationServerV2>,
}
impl CatalogRegenerationRecordV2 {
    pub fn new(migration_id: &str, actual: &RegistryDocumentV2) -> Result<Self, Error> {
        let DocumentReceiptV2::MigrationBaseline { receipt } = actual.receipt() else {
            return Err(Error::Protocol);
        };
        if receipt.migration_id != migration_id {
            return Err(Error::Protocol);
        }
        let servers = Self::expected_servers(actual)?;
        let result = Self {
            schema_version: 2,
            migration_id: migration_id.into(),
            generation: migration_id.into(),
            registry_baseline: actual.metadata()?,
            new_registry_payload_hash: receipt
                .registry_projection
                .as_ref()
                .ok_or(Error::Protocol)?
                .canonical_payload_hash
                .clone(),
            empty_catalog: CatalogIndex {
                schema_version: 1,
                servers: BTreeMap::new(),
            },
            servers,
        };
        result.validate_registry(actual)?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<(), Error> {
        self.registry_baseline.validate()?;
        if self.schema_version != 2
            || !valid_digest(&self.migration_id)
            || self.generation != self.migration_id
            || self.registry_baseline.kind != DocumentKindV2::Registry
            || !valid_digest(&self.new_registry_payload_hash)
            || self.empty_catalog.schema_version != 1
            || !self.empty_catalog.servers.is_empty()
            || (self.registry_baseline.presence == DocumentPresenceV2::Absent
                && !self.servers.is_empty())
            || self
                .servers
                .iter()
                .any(|s| !valid_id(&s.server_id) || !valid_digest(&s.new_destination_hash))
            || self
                .servers
                .windows(2)
                .any(|s| s[0].server_id >= s[1].server_id)
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
    pub fn validate_registry(&self, actual: &RegistryDocumentV2) -> Result<(), Error> {
        self.validate()?;
        let expected = Self::expected_servers(actual)?;
        let DocumentReceiptV2::MigrationBaseline { receipt } = actual.receipt() else {
            return Err(Error::Protocol);
        };
        if receipt.migration_id != self.migration_id
            || actual.metadata()? != self.registry_baseline
            || receipt
                .registry_projection
                .as_ref()
                .ok_or(Error::Protocol)?
                .canonical_payload_hash
                != self.new_registry_payload_hash
            || expected != self.servers
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
    fn expected_servers(
        actual: &RegistryDocumentV2,
    ) -> Result<Vec<CatalogRegenerationServerV2>, Error> {
        actual.validate()?;
        let mut expected = Vec::new();
        if let Some(registry) = actual.snapshot() {
            for server in &registry.servers {
                expected.push(CatalogRegenerationServerV2 {
                    server_id: server.server_id.clone(),
                    new_destination_hash: server.destination_hash()?,
                    state: CatalogRegenerationStateV2::CatalogRegenerationRequired,
                });
            }
        }
        expected.sort_by(|a, b| a.server_id.cmp(&b.server_id));
        Ok(expected)
    }
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        encode(self)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let result: Self = decode(bytes)?;
        result.validate()?;
        Ok(result)
    }
    pub fn canonical_hash(&self) -> Result<String, Error> {
        Ok(hash_bytes(&self.encode()?))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopObservationResultV2 {
    ZeroProductionProcesses,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StopObservationV2 {
    pub result: StopObservationResultV2,
    pub coordinator: ProductionBinaryIdentityV2,
    pub inspected_processes: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationIntentBindingV2 {
    pub schema_version: u32,
    pub migration_id: String,
    pub store_identity: String,
    pub source_store_identity: Option<String>,
    pub layout: ProtocolLayoutV2,
    pub canonical_parent_anchor: EntryAnchorV2,
    pub source_root_anchor: EntryAnchorV2,
    pub source_non_secret_tree_hash: String,
    pub entry_manifest: ProductionEntryManifestV2,
    pub entry_manifest_hash: String,
    pub stop: StopObservationV2,
}
impl MigrationIntentBindingV2 {
    pub fn validate(&self) -> Result<(), Error> {
        self.layout.validate(&self.migration_id)?;
        validate_manifest(&self.entry_manifest)?;
        validate_binary(&self.stop.coordinator)?;
        if self.schema_version != 2 || !valid_digest(&self.migration_id)
            || !valid_digest(&self.store_identity) || !valid_digest(&self.source_non_secret_tree_hash)
            || self.source_store_identity.as_ref().is_some_and(|v| v != &self.store_identity)
            || !parent_directory(&self.canonical_parent_anchor) || !root_directory(&self.source_root_anchor)
            || self.canonical_parent_anchor.device != self.source_root_anchor.device
            || same_object(&self.canonical_parent_anchor, &self.source_root_anchor)
            || self.entry_manifest_hash != hash_bytes(&encode(&self.entry_manifest)?)
            || self.stop.inspected_processes == 0
            // A runtime copy is only a structural candidate here. The live
            // entry owner must re-prove its protected confined path and bytes.
            || !(self.entry_manifest.entries.iter().any(|entry| entry.binary == self.stop.coordinator)
                || self.entry_manifest.runtime_copy_source_hashes.contains(&self.stop.coordinator.sha256))
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetirePreparedRecordV2 {
    pub binding: MigrationIntentBindingV2,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RetiredRecordV2 {
    pub binding: MigrationIntentBindingV2,
    pub canonical_parent_anchor: EntryAnchorV2,
    pub previous_intent_hash: String,
    pub retired_source: RetiredLedgerAnchorV1,
    pub tombstone: TombstoneLedgerAnchorV1,
}
impl RetiredRecordV2 {
    fn validate(&self) -> Result<(), Error> {
        self.binding.validate()?;
        let initial = ExternalMigrationIntentV2::RetirePrepared {
            record: RetirePreparedRecordV2 {
                binding: self.binding.clone(),
            },
        };
        if !same_parent(
            &self.canonical_parent_anchor,
            &self.binding.canonical_parent_anchor,
        ) || self.previous_intent_hash != initial.canonical_hash()?
            || self.retired_source.anchor != self.binding.source_root_anchor
            || self.retired_source.non_secret_tree_hash != self.binding.source_non_secret_tree_hash
            || !document_file(&self.tombstone.anchor)
            || !valid_digest(&self.tombstone.file_hash)
            || self.tombstone.anchor.device != self.canonical_parent_anchor.device
            || !distinct(&[
                &self.canonical_parent_anchor,
                &self.retired_source.anchor,
                &self.tombstone.anchor,
            ])
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StagingRecordV2 {
    pub retired: RetiredRecordV2,
    pub canonical_parent_anchor: EntryAnchorV2,
    pub previous_intent_hash: String,
    pub staging_anchor: EntryAnchorV2,
    pub random_epoch_id: String,
}
impl StagingRecordV2 {
    pub fn validate(&self) -> Result<(), Error> {
        self.retired.validate()?;
        if !same_parent(
            &self.canonical_parent_anchor,
            &self.retired.canonical_parent_anchor,
        ) || self.previous_intent_hash
            != (ExternalMigrationIntentV2::Retired {
                record: self.retired.clone(),
            })
            .canonical_hash()?
            || !root_directory(&self.staging_anchor)
            || !valid_digest(&self.random_epoch_id)
            || self.staging_anchor.device != self.canonical_parent_anchor.device
            || !distinct(&[
                &self.canonical_parent_anchor,
                &self.retired.retired_source.anchor,
                &self.retired.tombstone.anchor,
                &self.staging_anchor,
            ])
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedActivationRecordV2 {
    pub staging: StagingRecordV2,
    pub canonical_parent_anchor: EntryAnchorV2,
    pub previous_intent_hash: String,
    pub staging_anchor: EntryAnchorV2,
    pub activation_tree_hash: String,
    pub epoch_file_hash: String,
}
impl PreparedActivationRecordV2 {
    pub fn validate(&self) -> Result<(), Error> {
        self.staging.validate()?;
        if !same_parent(
            &self.canonical_parent_anchor,
            &self.staging.canonical_parent_anchor,
        ) || self.previous_intent_hash
            != (ExternalMigrationIntentV2::Staging {
                record: self.staging.clone(),
            })
            .canonical_hash()?
            || !valid_digest(&self.activation_tree_hash)
            || !valid_digest(&self.epoch_file_hash)
        {
            return Err(Error::Protocol);
        }
        if !root_directory(&self.staging_anchor)
            || !same_directory_identity(&self.staging_anchor, &self.staging.staging_anchor)
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
    pub fn validate_epoch(&self, epoch: &LedgerEpochV1, epoch_bytes: &[u8]) -> Result<(), Error> {
        self.validate()?;
        epoch.validate()?;
        let retired = &self.staging.retired;
        let common = &retired.binding;
        if epoch.store_identity != common.store_identity
            || epoch.migration_id != common.migration_id
            || epoch.random_epoch_id != self.staging.random_epoch_id
            || epoch.canonical_parent_anchor != self.canonical_parent_anchor
            || epoch.tombstone_anchor != retired.tombstone
            || epoch.retired_source_anchor != retired.retired_source
            || epoch.active_v2_root_anchor != self.staging_anchor
            || epoch.activation_tree_hash != self.activation_tree_hash
            || epoch.encode()? != epoch_bytes
            || hash_bytes(epoch_bytes) != self.epoch_file_hash
        {
            return Err(Error::Protocol);
        }
        Ok(())
    }
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActiveMigrationRecordV2 {
    pub prepared: PreparedActivationRecordV2,
    pub canonical_parent_anchor: EntryAnchorV2,
    pub prepared_intent_hash: String,
    pub final_root_anchor: EntryAnchorV2,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExternalMigrationIntentV2 {
    RetirePrepared { record: RetirePreparedRecordV2 },
    Retired { record: RetiredRecordV2 },
    Staging { record: StagingRecordV2 },
    PreparedActivation { record: PreparedActivationRecordV2 },
    ActiveV2 { record: ActiveMigrationRecordV2 },
}
impl ExternalMigrationIntentV2 {
    pub fn retire_prepared(binding: MigrationIntentBindingV2) -> Result<Self, Error> {
        let result = Self::RetirePrepared {
            record: RetirePreparedRecordV2 { binding },
        };
        result.validate()?;
        Ok(result)
    }
    pub fn retired(
        &self,
        canonical_parent_anchor: EntryAnchorV2,
        retired_source: RetiredLedgerAnchorV1,
        tombstone: TombstoneLedgerAnchorV1,
    ) -> Result<Self, Error> {
        self.validate()?;
        let Self::RetirePrepared { record } = self else {
            return Err(Error::Protocol);
        };
        let result = Self::Retired {
            record: RetiredRecordV2 {
                binding: record.binding.clone(),
                canonical_parent_anchor,
                previous_intent_hash: self.canonical_hash()?,
                retired_source,
                tombstone,
            },
        };
        result.validate()?;
        Ok(result)
    }
    pub fn staging(
        &self,
        canonical_parent_anchor: EntryAnchorV2,
        staging_anchor: EntryAnchorV2,
        random_epoch_id: String,
    ) -> Result<Self, Error> {
        self.validate()?;
        let Self::Retired { record } = self else {
            return Err(Error::Protocol);
        };
        let result = Self::Staging {
            record: StagingRecordV2 {
                retired: record.clone(),
                canonical_parent_anchor,
                previous_intent_hash: self.canonical_hash()?,
                staging_anchor,
                random_epoch_id,
            },
        };
        result.validate()?;
        Ok(result)
    }
    pub fn prepared_activation(
        &self,
        canonical_parent_anchor: EntryAnchorV2,
        epoch: &LedgerEpochV1,
        epoch_bytes: &[u8],
    ) -> Result<Self, Error> {
        self.validate()?;
        let Self::Staging { record } = self else {
            return Err(Error::Protocol);
        };
        let prepared = PreparedActivationRecordV2 {
            staging: record.clone(),
            canonical_parent_anchor,
            previous_intent_hash: self.canonical_hash()?,
            staging_anchor: epoch.active_v2_root_anchor,
            activation_tree_hash: epoch.activation_tree_hash.clone(),
            epoch_file_hash: hash_bytes(epoch_bytes),
        };
        prepared.validate_epoch(epoch, epoch_bytes)?;
        Ok(Self::PreparedActivation { record: prepared })
    }
    pub fn active(
        &self,
        canonical_parent_anchor: EntryAnchorV2,
        final_root_anchor: EntryAnchorV2,
    ) -> Result<Self, Error> {
        self.validate()?;
        let Self::PreparedActivation { record } = self else {
            return Err(Error::Protocol);
        };
        let result = Self::ActiveV2 {
            record: ActiveMigrationRecordV2 {
                prepared: record.clone(),
                canonical_parent_anchor,
                prepared_intent_hash: self.canonical_hash()?,
                final_root_anchor,
            },
        };
        result.validate()?;
        Ok(result)
    }
    pub fn validate(&self) -> Result<(), Error> {
        match self {
            Self::RetirePrepared { record } => record.binding.validate(),
            Self::Retired { record } => record.validate(),
            Self::Staging { record } => record.validate(),
            Self::PreparedActivation { record } => record.validate(),
            Self::ActiveV2 { record } => {
                record.prepared.validate()?;
                if !same_parent(
                    &record.canonical_parent_anchor,
                    &record.prepared.canonical_parent_anchor,
                ) || record.final_root_anchor != record.prepared.staging_anchor
                    || record.prepared_intent_hash
                        != (Self::PreparedActivation {
                            record: record.prepared.clone(),
                        })
                        .canonical_hash()?
                {
                    return Err(Error::Protocol);
                }
                Ok(())
            }
        }
    }
    pub fn encode(&self) -> Result<Vec<u8>, Error> {
        self.validate()?;
        encode(self)
    }
    pub fn decode(bytes: &[u8]) -> Result<Self, Error> {
        let result: Self = decode(bytes)?;
        result.validate()?;
        Ok(result)
    }
    pub fn canonical_hash(&self) -> Result<String, Error> {
        Ok(hash_bytes(&self.encode()?))
    }
}

fn pin_server_id(path: &[u8]) -> Option<&str> {
    let mut parts = path.split(|byte| *byte == b'/');
    if parts.next()? != b"servers" {
        return None;
    }
    let id = std::str::from_utf8(parts.next()?).ok()?;
    if parts.next()? != b"pinned.json" || parts.next().is_some() {
        return None;
    }
    Some(id)
}
fn read_pin(
    file: &File,
    anchor: &EntryAnchorV2,
    size: u64,
    sha256: &str,
    pinned: Option<&Metadata>,
) -> Result<(Metadata, Vec<u8>), Error> {
    let before = file.metadata().map_err(pin_storage)?;
    if before.len() > MAX_JSON_BYTES as u64 {
        return Err(Error::Limit);
    }
    if !before.is_file()
        || !protected_regular(&EntryAnchorV2::of(&before))
        || EntryAnchorV2::of(&before) != *anchor
        || before.len() != size
        || pinned.is_some_and(|expected| !stable_pin(expected, &before))
    {
        return Err(Error::Protection);
    }
    let mut bytes = vec![0; before.len() as usize];
    let mut offset = 0;
    while offset < bytes.len() {
        match file.read_at(&mut bytes[offset..], offset as u64) {
            Ok(0) => return Err(Error::Protection),
            Ok(count) => offset += count,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(pin_storage(error)),
        }
    }
    #[cfg(test)]
    AFTER_PIN_READ.with(|slot| {
        if let Some(hook) = slot.borrow_mut().as_mut() {
            hook(file);
        }
    });
    if !stable_pin(&before, &file.metadata().map_err(pin_storage)?) || hash_bytes(&bytes) != sha256
    {
        return Err(Error::Protection);
    }
    Ok((before, bytes))
}
fn stable_pin(a: &Metadata, b: &Metadata) -> bool {
    EntryAnchorV2::of(a) == EntryAnchorV2::of(b)
        && a.is_file() == b.is_file()
        && a.len() == b.len()
        && (a.mtime(), a.mtime_nsec(), a.ctime(), a.ctime_nsec())
            == (b.mtime(), b.mtime_nsec(), b.ctime(), b.ctime_nsec())
}
fn pin_storage(_: io::Error) -> Error {
    Error::Storage
}
#[cfg(test)]
std::thread_local! {
    static AFTER_PIN_READ: std::cell::RefCell<Option<Box<dyn FnMut(&File)>>> =
        const { std::cell::RefCell::new(None) };
}

fn validate_inventory(
    entries: &[SourceInventoryEntryV2],
    root: &EntryAnchorV2,
) -> Result<(), Error> {
    let mut directories = BTreeSet::new();
    let mut objects = BTreeSet::from([(root.device, root.inode)]);
    let mut previous: Option<&[u8]> = None;
    for entry in entries {
        let path = entry.path();
        let parts = relative_components(path)?;
        if previous.is_some_and(|old| old >= path)
            || entry.anchor().device != root.device
            || !objects.insert((entry.anchor().device, entry.anchor().inode))
        {
            return Err(Error::Protocol);
        }
        if let Some(slash) = path.iter().rposition(|b| *b == b'/')
            && !directories.contains(&path[..slash])
        {
            return Err(Error::Protocol);
        }
        let package = parts[0] == b"servers";
        if package {
            if parts.len() > 1
                && !valid_id(std::str::from_utf8(parts[1]).map_err(|_| Error::Protocol)?)
            {
                return Err(Error::Protocol);
            }
            if parts.len() <= 2 && !matches!(entry, SourceInventoryEntryV2::Directory { .. }) {
                return Err(Error::Protocol);
            }
        } else if parts.len() != 1 {
            return Err(Error::Protocol);
        }
        match entry {
            SourceInventoryEntryV2::Directory { anchor, .. } => {
                if !package || !protected_directory(anchor) {
                    return Err(Error::Protocol);
                }
                directories.insert(path);
            }
            SourceInventoryEntryV2::PublicFile {
                anchor,
                size,
                sha256,
                ..
            } => {
                if !protected_regular(anchor)
                    || !valid_digest(sha256)
                    || (!package
                        && (!document_file(anchor)
                            || ![
                                b"servers.json".as_slice(),
                                b"catalog.json".as_slice(),
                                b"owner.lock".as_slice(),
                            ]
                            .contains(&path)))
                    || (path == b"owner.lock" && (*size != 0 || *sha256 != hash_bytes(b"")))
                {
                    return Err(Error::Protocol);
                }
            }
            SourceInventoryEntryV2::SecretDocument { anchor, .. } => {
                if path != b"secrets.json" || !document_file(anchor) {
                    return Err(Error::Protocol);
                }
            }
        }
        previous = Some(path);
    }
    Ok(())
}
fn validate_paths(paths: &ManagedPathProjectionV2) -> Result<(), Error> {
    if paths.schema_version != 1
        || !valid_digest(&paths.source_registry_payload_hash)
        || !valid_digest(&paths.new_registry_payload_hash)
    {
        return Err(Error::Protocol);
    }
    let mut servers = BTreeSet::new();
    for server in &paths.servers {
        if !valid_id(&server.server_id)
            || !servers.insert(&server.server_id)
            || !valid_digest(&server.old_destination_hash)
            || !valid_digest(&server.new_destination_hash)
        {
            return Err(Error::Protocol);
        }
        let mut previous = None;
        for field in &server.fields {
            let order = match &field.field {
                ManagedPathFieldV2::Cwd => (0, 0),
                ManagedPathFieldV2::Command => (1, 0),
                ManagedPathFieldV2::Script => (2, 0),
                ManagedPathFieldV2::ReadableRoot { index } => (3, *index),
                ManagedPathFieldV2::ScriptArgument { index } => (4, *index),
                ManagedPathFieldV2::PinRuntime => (5, 0),
                ManagedPathFieldV2::PinEntry => (6, 0),
            };
            if previous.is_some_and(|old| old >= order)
                || !valid_digest(&field.old_value_hash)
                || !valid_digest(&field.new_value_hash)
            {
                return Err(Error::Protocol);
            }
            previous = Some(order);
        }
    }
    Ok(())
}
fn mapped_field<T: Serialize>(
    field: ManagedPathFieldV2,
    old: &T,
    new: &T,
) -> Result<ManagedPathFieldMappingV2, Error> {
    Ok(ManagedPathFieldMappingV2 {
        field,
        old_value_hash: canonical_hash(&serde_json::to_value(old).map_err(|_| Error::Protocol)?)?,
        new_value_hash: canonical_hash(&serde_json::to_value(new).map_err(|_| Error::Protocol)?)?,
    })
}
fn validate_transport_projection(
    old: &Destination,
    new: &Destination,
    fields: &[ManagedPathFieldMappingV2],
) -> Result<(), Error> {
    match (old, new) {
        (Destination::StreamableHttp { .. }, Destination::StreamableHttp { .. })
            if old == new && fields.is_empty() =>
        {
            Ok(())
        }
        (
            Destination::Stdio {
                command,
                args,
                cwd,
                sandbox_profile,
                secret_env,
                readable_roots,
                executable_sha256,
                script,
                script_sha256,
            },
            Destination::Stdio {
                command: next_command,
                args: next_args,
                cwd: next_cwd,
                sandbox_profile: next_sandbox,
                secret_env: next_env,
                readable_roots: next_roots,
                executable_sha256: next_executable_hash,
                script: next_script,
                script_sha256: next_script_hash,
            },
        ) => {
            if sandbox_profile != next_sandbox
                || secret_env != next_env
                || executable_sha256 != next_executable_hash
                || script_sha256 != next_script_hash
                || args.len() != next_args.len()
                || readable_roots.len() != next_roots.len()
                || script.is_some() != next_script.is_some()
            {
                return Err(Error::Protocol);
            }
            let mut expected = vec![
                mapped_field(ManagedPathFieldV2::Cwd, cwd, next_cwd)?,
                mapped_field(ManagedPathFieldV2::Command, command, next_command)?,
                mapped_field(ManagedPathFieldV2::Script, script, next_script)?,
            ];
            for (index, (old, new)) in readable_roots.iter().zip(next_roots).enumerate() {
                expected.push(mapped_field(
                    ManagedPathFieldV2::ReadableRoot { index },
                    old,
                    new,
                )?);
            }
            let script_text = script
                .as_ref()
                .map(|path| path.to_str().ok_or(Error::Protocol))
                .transpose()?;
            let next_script_text = next_script
                .as_ref()
                .map(|path| path.to_str().ok_or(Error::Protocol))
                .transpose()?;
            for (index, (old, new)) in args.iter().zip(next_args).enumerate() {
                if Some(old.as_str()) == script_text {
                    if Some(new.as_str()) != next_script_text {
                        return Err(Error::Protocol);
                    }
                    expected.push(mapped_field(
                        ManagedPathFieldV2::ScriptArgument { index },
                        old,
                        new,
                    )?);
                } else if old != new {
                    return Err(Error::Protocol);
                }
            }
            if fields.iter().any(|field| {
                matches!(
                    field.field,
                    ManagedPathFieldV2::PinRuntime | ManagedPathFieldV2::PinEntry
                )
            }) {
                expected.push(mapped_field(
                    ManagedPathFieldV2::PinRuntime,
                    command,
                    next_command,
                )?);
                expected.push(mapped_field(
                    ManagedPathFieldV2::PinEntry,
                    script.as_ref().ok_or(Error::Protocol)?,
                    next_script.as_ref().ok_or(Error::Protocol)?,
                )?);
            }
            if expected != fields {
                return Err(Error::Protocol);
            }
            Ok(())
        }
        _ => Err(Error::Protocol),
    }
}
fn runtime_parts(resource: &RuntimeResourceRecheckV2) -> (u8, usize, &PathBuf, Option<&str>) {
    match resource {
        RuntimeResourceRecheckV2::Command {
            logical_path,
            sha256,
            ..
        } => (0, 0, logical_path, Some(sha256)),
        RuntimeResourceRecheckV2::Script {
            logical_path,
            sha256,
            ..
        } => (1, 0, logical_path, Some(sha256)),
        RuntimeResourceRecheckV2::ReadableRoot {
            index,
            logical_path,
            ..
        } => (2, *index, logical_path, None),
    }
}
fn validate_runtime_shape(rows: &[ServerRuntimeRecheckV2]) -> Result<(), Error> {
    if rows.windows(2).any(|r| r[0].server_id >= r[1].server_id) {
        return Err(Error::Protocol);
    }
    for row in rows {
        if !valid_id(&row.server_id) || !valid_digest(&row.new_destination_hash) {
            return Err(Error::Protocol);
        }
        let mut previous = None;
        for resource in &row.resources {
            let (role, index, path, hash) = runtime_parts(resource);
            require_normalized(path)?;
            let anchor = match resource {
                RuntimeResourceRecheckV2::Command { anchor, .. }
                | RuntimeResourceRecheckV2::Script { anchor, .. }
                | RuntimeResourceRecheckV2::ReadableRoot { anchor, .. } => anchor,
            };
            if previous.is_some_and(|old| old >= (role, index))
                || anchor.inode == 0
                || anchor.mode & 0o7022 != 0
                || ![0, unsafe { libc::geteuid() }].contains(&anchor.uid)
                || (hash.is_some() && (anchor.links != 1 || !valid_digest(hash.unwrap())))
                || (role == 0 && anchor.mode & 0o111 == 0)
                || (hash.is_none() && anchor.links == 0)
            {
                return Err(Error::Protocol);
            }
            previous = Some((role, index));
        }
    }
    Ok(())
}
fn validate_manifest(manifest: &ProductionEntryManifestV2) -> Result<(), Error> {
    if manifest.protocol_version != 2
        || manifest.entries.is_empty()
        || manifest.entries.len() > 8
        || manifest.runtime_copy_source_hashes.len() > 2
    {
        return Err(Error::Protocol);
    }
    let mut paths = BTreeSet::new();
    let mut kinds = BTreeSet::new();
    let mut objects = BTreeSet::new();
    for entry in &manifest.entries {
        validate_binary(&entry.binary)?;
        if !paths.insert(&entry.binary.path)
            || !kinds.insert(entry.kind)
            || !objects.insert((entry.binary.anchor.device, entry.binary.anchor.inode))
        {
            return Err(Error::Protocol);
        }
    }
    let mut hashes = BTreeSet::new();
    for hash in &manifest.runtime_copy_source_hashes {
        if !valid_digest(hash)
            || !hashes.insert(hash)
            || !manifest.entries.iter().any(|entry| {
                matches!(
                    entry.kind,
                    ProductionEntryKindV2::Cli | ProductionEntryKindV2::ExecutionHelper
                ) && entry.binary.sha256 == *hash
            })
        {
            return Err(Error::Protocol);
        }
    }
    Ok(())
}
fn validate_binary(binary: &ProductionBinaryIdentityV2) -> Result<(), Error> {
    require_normalized(&binary.path)?;
    let anchor = &binary.anchor;
    if !valid_digest(&binary.sha256)
        || anchor.inode == 0
        || anchor.links != 1
        || ![0, unsafe { libc::geteuid() }].contains(&anchor.uid)
        || anchor.mode & 0o7022 != 0
        || anchor.mode & 0o111 == 0
    {
        return Err(Error::Protocol);
    }
    Ok(())
}
fn relative_components(path: &[u8]) -> Result<Vec<&[u8]>, Error> {
    let parts: Vec<_> = path.split(|b| *b == b'/').collect();
    if parts
        .iter()
        .any(|p| p.is_empty() || *p == b"." || *p == b".." || p.contains(&0))
    {
        return Err(Error::Protocol);
    }
    Ok(parts)
}
fn require_normalized(path: &Path) -> Result<(), Error> {
    use std::os::unix::ffi::OsStrExt;
    let bytes = path.as_os_str().as_bytes();
    if !path.is_absolute()
        || bytes.contains(&0)
        || (bytes != b"/"
            && bytes
                .split(|b| *b == b'/')
                .skip(1)
                .any(|part| part.is_empty() || part == b"." || part == b".."))
        || path
            .components()
            .any(|part| matches!(part, Component::CurDir | Component::ParentDir))
    {
        return Err(Error::Protocol);
    }
    Ok(())
}
fn protected_directory(anchor: &EntryAnchorV2) -> bool {
    anchor.inode != 0
        && anchor.uid == unsafe { libc::geteuid() }
        && anchor.mode & 0o7022 == 0
        && anchor.links > 0
}
fn parent_directory(anchor: &EntryAnchorV2) -> bool {
    protected_directory(anchor)
}
fn root_directory(anchor: &EntryAnchorV2) -> bool {
    protected_directory(anchor) && anchor.mode == 0o700
}
fn protected_regular(anchor: &EntryAnchorV2) -> bool {
    protected_directory(anchor) && anchor.links == 1
}
fn document_file(anchor: &EntryAnchorV2) -> bool {
    protected_regular(anchor) && anchor.mode == 0o600
}
fn same_object(a: &EntryAnchorV2, b: &EntryAnchorV2) -> bool {
    a.device == b.device && a.inode == b.inode
}
fn same_directory_identity(a: &EntryAnchorV2, b: &EntryAnchorV2) -> bool {
    same_object(a, b) && a.uid == b.uid && a.mode == b.mode && a.links > 0 && b.links > 0
}
fn same_parent(a: &EntryAnchorV2, b: &EntryAnchorV2) -> bool {
    parent_directory(a)
        && parent_directory(b)
        && same_object(a, b)
        && a.uid == b.uid
        && a.mode == b.mode
}
fn distinct(anchors: &[&EntryAnchorV2]) -> bool {
    anchors
        .iter()
        .map(|a| (a.device, a.inode))
        .collect::<BTreeSet<_>>()
        .len()
        == anchors.len()
}
fn encode<T: Serialize>(value: &T) -> Result<Vec<u8>, Error> {
    serde_json::to_vec(value).map_err(|_| Error::Protocol)
}
fn decode<T: DeserializeOwned + Serialize>(bytes: &[u8]) -> Result<T, Error> {
    let value: T = serde_json::from_slice(bytes).map_err(|_| Error::Protocol)?;
    if encode(&value)? != bytes {
        return Err(Error::Protocol);
    }
    Ok(value)
}
fn hash_bytes(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

#[cfg(test)]
pub(super) mod tests;
