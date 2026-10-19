//! External migration intent phases, anchors and canonical record validation tests.
use super::*;
use crate::{
    entry_gate::ProductionEntryIdentityV2,
    registration::{Capabilities, ProtocolProfile, Server, Timeouts},
    store::{Secrets, migration_v2::paths::transform_managed_paths_v2},
};
use std::io::Write;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

use crate::migration_observation::MigrationParentObservationV2;
pub(in crate::store::migration_v2) struct FixtureCoordinates {
    pub(in crate::store::migration_v2) parent: File,
    home: PathBuf,
    id: String,
    manifest: ProductionEntryManifestV2,
    manifest_hash: String,
}
impl FixtureCoordinates {
    pub(in crate::store::migration_v2) fn new(
        home: &Path,
        id: &str,
        manifest: ProductionEntryManifestV2,
    ) -> Self {
        let parent_path = home.join(".polaris");
        std::fs::create_dir(&parent_path).unwrap();
        std::fs::set_permissions(&parent_path, std::fs::Permissions::from_mode(0o700)).unwrap();
        let parent = File::open(&parent_path).unwrap();
        let manifest_hash = manifest.canonical_hash().unwrap();
        Self {
            parent,
            home: home.into(),
            id: id.into(),
            manifest,
            manifest_hash,
        }
    }
    pub(in crate::store::migration_v2) fn observation(
        &self,
    ) -> Result<MigrationParentObservationV2, Error> {
        MigrationParentObservationV2::inspect(&self.home, &self.parent)
    }
    pub(in crate::store::migration_v2) fn coordinates(
        &self,
    ) -> Result<(&Path, &str, &ProductionEntryManifestV2, &str), Error> {
        self.observation()?.verify_current()?;
        self.manifest.validate().map_err(|_| Error::Protection)?;
        Ok((&self.home, &self.id, &self.manifest, &self.manifest_hash))
    }
}
pub(in crate::store::migration_v2) struct ObservationFixture {
    _root: tempfile::TempDir,
    pub(in crate::store::migration_v2) binding: FixtureCoordinates,
}
impl ObservationFixture {
    pub(in crate::store::migration_v2) fn new(migration_id: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let binary = root.path().join("polaris");
        std::fs::write(&binary, b"synthetic executable identity\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o500)).unwrap();
        let manifest = ProductionEntryManifestV2 {
            protocol_version: 2,
            entries: vec![ProductionEntryIdentityV2 {
                kind: ProductionEntryKindV2::Cli,
                binary: ProductionBinaryIdentityV2::inspect(&binary).unwrap(),
            }],
            runtime_copy_source_hashes: vec![],
        };
        let binding = FixtureCoordinates::new(&home, migration_id, manifest);
        Self {
            _root: root,
            binding,
        }
    }
    pub(in crate::store::migration_v2) fn stage(&self) -> StagingRecordV2 {
        let (_, id, manifest, manifest_hash) = self.binding.coordinates().unwrap();
        let canonical_parent = EntryAnchorV2::of(&self.binding.parent.metadata().unwrap());
        let ExternalMigrationIntentV2::RetirePrepared { mut record } = initial() else {
            unreachable!()
        };
        record.binding.migration_id = id.into();
        record.binding.layout = ProtocolLayoutV2::new(id).unwrap();
        record.binding.entry_manifest = manifest.clone();
        record.binding.entry_manifest_hash = manifest_hash.into();
        record.binding.canonical_parent_anchor = canonical_parent;
        record.binding.stop.coordinator = manifest.entries[0].binary.clone();
        let retired = ExternalMigrationIntentV2::RetirePrepared { record }
            .retired(
                canonical_parent,
                RetiredLedgerAnchorV1 {
                    anchor: anchor(20, 0o700, 2),
                    non_secret_tree_hash: digest('3'),
                },
                TombstoneLedgerAnchorV1 {
                    anchor: anchor(30, 0o600, 1),
                    file_hash: digest('4'),
                },
            )
            .unwrap();
        let ExternalMigrationIntentV2::Staging { record } = retired
            .staging(canonical_parent, anchor(40, 0o700, 2), digest('5'))
            .unwrap()
        else {
            unreachable!()
        };
        record
    }
}

fn digest(byte: char) -> String {
    byte.to_string().repeat(64)
}
fn anchor(inode: u64, mode: u32, links: u64) -> EntryAnchorV2 {
    static DEVICE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    EntryAnchorV2 {
        device: *DEVICE.get_or_init(|| std::fs::metadata(std::env::temp_dir()).unwrap().dev()),
        inode,
        uid: unsafe { libc::geteuid() },
        mode,
        links,
    }
}
fn parent(links: u64) -> EntryAnchorV2 {
    anchor(10, 0o755, links)
}
fn initial() -> ExternalMigrationIntentV2 {
    let binary = ProductionBinaryIdentityV2 {
        path: "/verified/polaris".into(),
        anchor: anchor(100, 0o700, 1),
        sha256: digest('a'),
    };
    let manifest = ProductionEntryManifestV2 {
        protocol_version: 2,
        entries: vec![ProductionEntryIdentityV2 {
            kind: ProductionEntryKindV2::Cli,
            binary: binary.clone(),
        }],
        runtime_copy_source_hashes: vec![],
    };
    ExternalMigrationIntentV2::retire_prepared(MigrationIntentBindingV2 {
        schema_version: 2,
        migration_id: digest('1'),
        store_identity: digest('2'),
        source_store_identity: Some(digest('2')),
        layout: ProtocolLayoutV2::new(&digest('1')).unwrap(),
        canonical_parent_anchor: parent(2),
        source_root_anchor: anchor(20, 0o700, 2),
        source_non_secret_tree_hash: digest('3'),
        entry_manifest_hash: hash_bytes(&encode(&manifest).unwrap()),
        entry_manifest: manifest,
        stop: StopObservationV2 {
            result: StopObservationResultV2::ZeroProductionProcesses,
            coordinator: binary,
            inspected_processes: 7,
        },
    })
    .unwrap()
}
fn retired() -> ExternalMigrationIntentV2 {
    initial()
        .retired(
            parent(3),
            RetiredLedgerAnchorV1 {
                anchor: anchor(20, 0o700, 2),
                non_secret_tree_hash: digest('3'),
            },
            TombstoneLedgerAnchorV1 {
                anchor: anchor(30, 0o600, 1),
                file_hash: digest('4'),
            },
        )
        .unwrap()
}
fn staging() -> ExternalMigrationIntentV2 {
    retired()
        .staging(parent(4), anchor(40, 0o700, 2), digest('5'))
        .unwrap()
}
fn stage_record() -> StagingRecordV2 {
    let ExternalMigrationIntentV2::Staging { record } = staging() else {
        unreachable!()
    };
    record
}
fn epoch(stage: &StagingRecordV2) -> LedgerEpochV1 {
    let common = &stage.retired.binding;
    LedgerEpochV1 {
        protocol_version: 2,
        store_identity: common.store_identity.clone(),
        random_epoch_id: stage.random_epoch_id.clone(),
        migration_id: common.migration_id.clone(),
        canonical_parent_anchor: parent(4),
        tombstone_anchor: stage.retired.tombstone.clone(),
        retired_source_anchor: stage.retired.retired_source.clone(),
        active_v2_root_anchor: stage.staging_anchor,
        activation_tree_hash: digest('6'),
    }
}
fn prepared() -> ExternalMigrationIntentV2 {
    let stage = staging();
    let epoch = epoch(&stage_record());
    stage
        .prepared_activation(parent(4), &epoch, &epoch.encode().unwrap())
        .unwrap()
}
fn http(id: &str) -> Server {
    Server {
        server_id: id.into(),
        enabled_projects: BTreeSet::new(),
        transport: Destination::StreamableHttp {
            url: format!("https://example.com/{id}/mcp"),
            secret_headers: BTreeMap::from([("authorization".into(), "owned_ref".into())]),
        },
        protocol_profile: ProtocolProfile::Discover2026,
        capabilities: Capabilities::default(),
        timeouts: Timeouts::default(),
    }
}
fn registry(servers: Vec<Server>) -> Registry {
    Registry {
        schema_version: 1,
        revision: 9,
        servers,
    }
}
fn store(
    present_registry: bool,
    present_secret: bool,
) -> (StoreMigrationRecordV2, Option<Registry>, RegistryDocumentV2) {
    let (record, source, registry, _) = store_with_credential(present_registry, present_secret);
    (record, source, registry)
}
fn store_with_credential(
    present_registry: bool,
    present_secret: bool,
) -> (
    StoreMigrationRecordV2,
    Option<Registry>,
    RegistryDocumentV2,
    CredentialDocumentV2,
) {
    let fixture = ObservationFixture::new(&digest('1'));
    let source = present_registry.then(|| registry(vec![http("http")]));
    let mapped = source.as_ref().map(|source| {
        transform_managed_paths_v2(source, Path::new("/logical-home"), &BTreeMap::new()).unwrap()
    });
    let actual = RegistryDocumentV2::baseline(
        &digest('1'),
        mapped.as_ref().map(|m| m.registry.clone()),
        present_registry.then(|| digest('b')),
    )
    .unwrap();
    let credential = CredentialDocumentV2::baseline(
        &digest('1'),
        present_secret.then(|| {
            Secrets::new(
                7,
                BTreeMap::from([("owned_ref".into(), "owned-secret".into())]),
            )
            .unwrap()
        }),
    )
    .unwrap();
    let mut inventory = Vec::new();
    if present_secret {
        inventory.push(SourceInventoryEntryV2::SecretDocument {
            relative_path: b"secrets.json".to_vec(),
            anchor: anchor(51, 0o600, 1),
        });
    }
    if present_registry {
        inventory.push(SourceInventoryEntryV2::PublicFile {
            relative_path: b"servers.json".to_vec(),
            anchor: anchor(50, 0o600, 1),
            size: 10,
            sha256: digest('b'),
        });
    }
    let record = StoreMigrationRecordV2::new_from_descriptors(
        fixture.stage(),
        StoreMigrationPayloadV2 {
            planned_active_anchor: anchor(40, 0o700, 2),
            source_inventory: inventory,
            credential: BaselineDocumentRecordV2::credential(
                &credential,
                present_secret.then(|| anchor(51, 0o600, 1)),
                anchor(61, 0o600, 1),
            )
            .unwrap(),
            registry: BaselineDocumentRecordV2::registry(
                &actual,
                present_registry.then(|| anchor(50, 0o600, 1)),
                anchor(60, 0o600, 1),
            )
            .unwrap(),
            managed_paths: mapped.map(|m| m.projection),
            unregistered_pins: vec![],
            package_directories: vec![],
            package_files: vec![],
            runtime_rechecks: vec![],
        },
        source.as_ref(),
        &credential,
        &actual,
        &fixture.binding.observation().unwrap(),
        &BTreeMap::new(),
    )
    .unwrap();
    (record, source, actual, credential)
}
fn injected_field(bytes: &[u8], field: &str) -> Vec<u8> {
    let mut text = String::from_utf8(bytes.to_vec()).unwrap();
    assert_eq!(text.pop(), Some('}'));
    text.push(',');
    text.push_str(field);
    text.push('}');
    text.into_bytes()
}

fn owned_directory(path: &Path) {
    std::fs::create_dir(path).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700)).unwrap();
}
fn owned_file(path: &Path, bytes: &[u8]) -> File {
    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .unwrap();
    file.write_all(bytes).unwrap();
    file
}
fn actual_anchor(path: &Path) -> EntryAnchorV2 {
    EntryAnchorV2::of(&std::fs::symlink_metadata(path).unwrap())
}

struct PinFixture {
    source: ObservationFixture,
    stage: StagingRecordV2,
    record: StoreMigrationRecordV2,
    source_registry: Option<Registry>,
    actual_registry: RegistryDocumentV2,
    actual_credential: CredentialDocumentV2,
    files: BTreeMap<String, (File, File)>,
    paths: BTreeMap<String, (PathBuf, PathBuf)>,
}
impl PinFixture {
    fn new(count: usize, present_registry: bool, long_opaque_text: bool) -> Self {
        let source = ObservationFixture::new(&digest('1'));
        let (home, _, manifest, manifest_hash) = source.binding.coordinates().unwrap();
        let parent_path = home.join(".polaris");
        let source_root = parent_path.join(format!("mcp.retired-{}", digest('1')));
        let staged_root = parent_path.join(format!("mcp-v2.staging-{}", digest('1')));
        owned_directory(&source_root);
        owned_directory(&staged_root);
        owned_directory(&source_root.join("servers"));
        owned_directory(&staged_root.join("servers"));
        let source_registry = present_registry.then(|| registry(vec![http("http")]));
        let raw_registry = source_registry
            .as_ref()
            .map(|source| encode(source).unwrap());
        let source_registry_file = raw_registry
            .as_ref()
            .map(|bytes| owned_file(&source_root.join("servers.json"), bytes));
        let mapped_registry = source_registry
            .as_ref()
            .map(|source| transform_managed_paths_v2(source, home, &BTreeMap::new()).unwrap());
        let actual_registry = RegistryDocumentV2::baseline(
            &digest('1'),
            mapped_registry
                .as_ref()
                .map(|mapped| mapped.registry.clone()),
            raw_registry.as_ref().map(|bytes| hash_bytes(bytes)),
        )
        .unwrap();
        let actual_credential = CredentialDocumentV2::baseline(&digest('1'), None).unwrap();
        let staged_registry = owned_file(
            &staged_root.join("servers.json"),
            &actual_registry.encode().unwrap(),
        );
        let staged_credential = owned_file(
            &staged_root.join("secrets.json"),
            &actual_credential.encode().unwrap(),
        );
        let tombstone = owned_file(&parent_path.join("mcp"), b"owned fixture tombstone");
        let mut inventory = Vec::new();
        if let (Some(file), Some(bytes)) = (&source_registry_file, &raw_registry) {
            inventory.push(SourceInventoryEntryV2::PublicFile {
                relative_path: b"servers.json".to_vec(),
                anchor: EntryAnchorV2::of(&file.metadata().unwrap()),
                size: bytes.len() as u64,
                sha256: hash_bytes(bytes),
            });
        }
        let mut files = BTreeMap::new();
        let mut paths = BTreeMap::new();
        let mut pin_rows = Vec::new();
        let mut mappings = Vec::new();
        for index in 0..count {
            let id = format!("unregistered-{index:02}");
            let source_server = source_root.join("servers").join(&id);
            let staged_server = staged_root.join("servers").join(&id);
            owned_directory(&source_server);
            owned_directory(&staged_server);
            let logical_server = home.join(".polaris/mcp/servers").join(&id);
            let pin = PinnedPathsV1 {
                schema_version: 1,
                package: format!("opaque package includes {}", logical_server.display()),
                version: if long_opaque_text {
                    "opaque-version-".repeat(1024)
                } else {
                    format!("opaque version includes {}", logical_server.display())
                },
                runtime: logical_server.join("bin/runtime"),
                entry: logical_server.join("entry.js"),
            };
            let source_bytes = encode(&pin).unwrap();
            let mapped =
                transform_unregistered_pin_v2(source_registry.as_ref(), &id, &source_bytes, home)
                    .unwrap();
            let staged_bytes = encode(&mapped.pin).unwrap();
            let source_path = source_server.join("pinned.json");
            let staged_path = staged_server.join("pinned.json");
            let source_file = owned_file(&source_path, &source_bytes);
            let staged_file = owned_file(&staged_path, &staged_bytes);
            let relative_path = format!("servers/{id}/pinned.json").into_bytes();
            let source_anchor = EntryAnchorV2::of(&source_file.metadata().unwrap());
            let staged_anchor = EntryAnchorV2::of(&staged_file.metadata().unwrap());
            inventory.push(SourceInventoryEntryV2::PublicFile {
                relative_path: relative_path.clone(),
                anchor: source_anchor,
                size: source_bytes.len() as u64,
                sha256: hash_bytes(&source_bytes),
            });
            mappings.push(PackageFileMappingV2 {
                relative_path,
                source_anchor,
                source_size: source_bytes.len() as u64,
                source_sha256: hash_bytes(&source_bytes),
                staged_anchor,
                staged_size: staged_bytes.len() as u64,
                staged_sha256: hash_bytes(&staged_bytes),
            });
            pin_rows.push(UnregisteredPinMappingV2 {
                server_id: id.clone(),
                fields: mapped.fields.try_into().unwrap(),
            });
            paths.insert(id.clone(), (source_path, staged_path));
            files.insert(id, (source_file, staged_file));
        }
        let mut directories = Vec::new();
        for relative in std::iter::once("servers".to_owned())
            .chain(files.keys().map(|id| format!("servers/{id}")))
        {
            let source_anchor = actual_anchor(&source_root.join(&relative));
            let staged_anchor = actual_anchor(&staged_root.join(&relative));
            inventory.push(SourceInventoryEntryV2::Directory {
                relative_path: relative.as_bytes().to_vec(),
                anchor: source_anchor,
            });
            directories.push(PackageDirectoryMappingV2 {
                relative_path: relative.into_bytes(),
                source_anchor,
                staged_anchor,
            });
        }
        inventory.sort_by(|a, b| a.path().cmp(b.path()));
        let parent_anchor = EntryAnchorV2::of(&source.binding.parent.metadata().unwrap());
        let source_anchor = actual_anchor(&source_root);
        let tombstone_anchor = EntryAnchorV2::of(&tombstone.metadata().unwrap());
        let intent = ExternalMigrationIntentV2::retire_prepared(MigrationIntentBindingV2 {
            schema_version: 2,
            migration_id: digest('1'),
            store_identity: digest('2'),
            source_store_identity: None,
            layout: ProtocolLayoutV2::new(&digest('1')).unwrap(),
            canonical_parent_anchor: parent_anchor,
            source_root_anchor: source_anchor,
            source_non_secret_tree_hash: digest('3'),
            entry_manifest: manifest.clone(),
            entry_manifest_hash: manifest_hash.into(),
            stop: StopObservationV2 {
                result: StopObservationResultV2::ZeroProductionProcesses,
                // Synthetic codec observation, never a live stop capability.
                coordinator: manifest.entries[0].binary.clone(),
                inspected_processes: 7,
            },
        })
        .unwrap()
        .retired(
            parent_anchor,
            RetiredLedgerAnchorV1 {
                anchor: source_anchor,
                non_secret_tree_hash: digest('3'),
            },
            TombstoneLedgerAnchorV1 {
                anchor: tombstone_anchor,
                file_hash: hash_bytes(b"owned fixture tombstone"),
            },
        )
        .unwrap()
        .staging(parent_anchor, actual_anchor(&staged_root), digest('5'))
        .unwrap();
        let ExternalMigrationIntentV2::Staging { record: stage } = intent else {
            unreachable!()
        };
        let pairs = files
            .iter()
            .map(|(id, (source, staged))| (id.clone(), (source, staged)))
            .collect();
        let record = StoreMigrationRecordV2::new_from_descriptors(
            stage.clone(),
            StoreMigrationPayloadV2 {
                planned_active_anchor: stage.staging_anchor,
                source_inventory: inventory,
                credential: BaselineDocumentRecordV2::credential(
                    &actual_credential,
                    None,
                    EntryAnchorV2::of(&staged_credential.metadata().unwrap()),
                )
                .unwrap(),
                registry: BaselineDocumentRecordV2::registry(
                    &actual_registry,
                    source_registry_file
                        .as_ref()
                        .map(|file| EntryAnchorV2::of(&file.metadata().unwrap())),
                    EntryAnchorV2::of(&staged_registry.metadata().unwrap()),
                )
                .unwrap(),
                managed_paths: mapped_registry.map(|mapped| mapped.projection),
                unregistered_pins: pin_rows,
                package_directories: directories,
                package_files: mappings,
                runtime_rechecks: vec![],
            },
            source_registry.as_ref(),
            &actual_credential,
            &actual_registry,
            &source.binding.observation().unwrap(),
            &pairs,
        )
        .unwrap();
        drop(pairs);
        Self {
            source,
            stage,
            record,
            source_registry,
            actual_registry,
            actual_credential,
            files,
            paths,
        }
    }
    fn pairs(&self) -> BTreeMap<String, (&File, &File)> {
        self.files
            .iter()
            .map(|(id, (source, staged))| (id.clone(), (source, staged)))
            .collect()
    }
    fn validate(&self, record: &StoreMigrationRecordV2) -> Result<(), Error> {
        record.validate_unregistered_pin_descriptors(
            &self.source.binding.observation().unwrap(),
            self.source_registry.as_ref(),
            &self.actual_registry,
            &self.pairs(),
        )
    }
    fn construct(&self, record: &StoreMigrationRecordV2) -> Result<StoreMigrationRecordV2, Error> {
        StoreMigrationRecordV2::new_from_descriptors(
            self.stage.clone(),
            StoreMigrationPayloadV2 {
                planned_active_anchor: record.planned_active_anchor,
                source_inventory: record.source_inventory.clone(),
                credential: record.credential.clone(),
                registry: record.registry.clone(),
                managed_paths: record.managed_paths.clone(),
                unregistered_pins: record.unregistered_pins.clone(),
                package_directories: record.package_directories.clone(),
                package_files: record.package_files.clone(),
                runtime_rechecks: record.runtime_rechecks.clone(),
            },
            self.source_registry.as_ref(),
            &self.actual_credential,
            &self.actual_registry,
            &self.source.binding.observation().unwrap(),
            &self.pairs(),
        )
    }
}

#[test]
fn all_five_phases_round_trip_and_active_retains_prepared_epoch_binding() {
    let prepared = prepared();
    let active = prepared.active(parent(4), anchor(40, 0o700, 2)).unwrap();
    for intent in [
        initial(),
        retired(),
        staging(),
        prepared.clone(),
        active.clone(),
    ] {
        assert_eq!(
            ExternalMigrationIntentV2::decode(&intent.encode().unwrap()).unwrap(),
            intent
        );
    }
    let ExternalMigrationIntentV2::ActiveV2 { record } = active else {
        unreachable!()
    };
    assert_eq!(
        record.prepared_intent_hash,
        prepared.canonical_hash().unwrap()
    );
    let epoch = epoch(&stage_record());
    record
        .prepared
        .validate_epoch(&epoch, &epoch.encode().unwrap())
        .unwrap();
    assert_eq!(
        record.final_root_anchor,
        record.prepared.staging.staging_anchor
    );
}

#[test]
fn phase_constructors_reject_skips_and_terminal_advancement() {
    assert!(
        initial()
            .staging(parent(4), anchor(40, 0o700, 2), digest('5'))
            .is_err()
    );
    assert!(retired().active(parent(4), anchor(40, 0o700, 2)).is_err());
    let epoch = epoch(&stage_record());
    assert!(
        initial()
            .prepared_activation(parent(4), &epoch, &epoch.encode().unwrap())
            .is_err()
    );
    let active = prepared().active(parent(4), anchor(40, 0o700, 2)).unwrap();
    assert!(active.active(parent(4), anchor(40, 0o700, 2)).is_err());
}

#[test]
fn each_previous_hash_is_bound_to_the_closed_predecessor() {
    let mut intent = retired();
    let ExternalMigrationIntentV2::Retired { record } = &mut intent else {
        unreachable!()
    };
    record.previous_intent_hash = digest('f');
    assert!(ExternalMigrationIntentV2::decode(&encode(&intent).unwrap()).is_err());
    let mut intent = staging();
    let ExternalMigrationIntentV2::Staging { record } = &mut intent else {
        unreachable!()
    };
    record.previous_intent_hash = digest('f');
    assert!(ExternalMigrationIntentV2::decode(&encode(&intent).unwrap()).is_err());
    let mut intent = prepared();
    let ExternalMigrationIntentV2::PreparedActivation { record } = &mut intent else {
        unreachable!()
    };
    record.previous_intent_hash = digest('f');
    assert!(ExternalMigrationIntentV2::decode(&encode(&intent).unwrap()).is_err());
}

#[test]
fn active_rejects_wrong_snapshot_hash_and_final_inode() {
    let active = prepared().active(parent(4), anchor(40, 0o700, 2)).unwrap();
    let mut changed = active.clone();
    let ExternalMigrationIntentV2::ActiveV2 { record } = &mut changed else {
        unreachable!()
    };
    record.prepared.epoch_file_hash = digest('f');
    assert!(ExternalMigrationIntentV2::decode(&encode(&changed).unwrap()).is_err());
    let mut changed = active;
    let ExternalMigrationIntentV2::ActiveV2 { record } = &mut changed else {
        unreachable!()
    };
    record.final_root_anchor.inode += 1;
    assert!(changed.validate().is_err());
}

#[test]
fn planned_namespace_changes_allow_parent_links_but_never_parent_replacement() {
    let stage = retired()
        .staging(parent(99), anchor(40, 0o700, 2), digest('5'))
        .unwrap();
    assert!(stage.validate().is_ok());
    let mut changed = stage;
    let ExternalMigrationIntentV2::Staging { record } = &mut changed else {
        unreachable!()
    };
    record.canonical_parent_anchor.inode += 1;
    assert!(changed.validate().is_err());
    let mut wrong_mode = parent(4);
    wrong_mode.mode = 0o777;
    assert!(
        retired()
            .staging(wrong_mode, anchor(40, 0o700, 2), digest('5'))
            .is_err()
    );
}

#[test]
fn prepared_captures_final_staging_links_after_directory_assembly() {
    let mut epoch = epoch(&stage_record());
    epoch.active_v2_root_anchor.links = 9;
    let prepared = staging()
        .prepared_activation(parent(4), &epoch, &epoch.encode().unwrap())
        .unwrap();
    let active = prepared.active(parent(4), anchor(40, 0o700, 9)).unwrap();
    let ExternalMigrationIntentV2::ActiveV2 { record } = active else {
        unreachable!()
    };
    assert_eq!(record.prepared.staging.staging_anchor.links, 2);
    assert_eq!(record.prepared.staging_anchor.links, 9);
    assert_eq!(record.final_root_anchor.links, 9);
    assert!(prepared.active(parent(4), anchor(40, 0o700, 2)).is_err());
    let (mut store, _, _) = store(false, false);
    store.planned_active_anchor.links = 9;
    store.validate().unwrap();
    store.planned_active_anchor.inode += 1;
    assert!(store.validate().is_err());
}

#[test]
fn retirement_requires_same_source_inode_projection_and_distinct_tombstone() {
    assert!(
        initial()
            .retired(
                parent(3),
                RetiredLedgerAnchorV1 {
                    anchor: anchor(21, 0o700, 2),
                    non_secret_tree_hash: digest('3')
                },
                TombstoneLedgerAnchorV1 {
                    anchor: anchor(30, 0o600, 1),
                    file_hash: digest('4')
                }
            )
            .is_err()
    );
    assert!(
        initial()
            .retired(
                parent(3),
                RetiredLedgerAnchorV1 {
                    anchor: anchor(20, 0o700, 2),
                    non_secret_tree_hash: digest('f')
                },
                TombstoneLedgerAnchorV1 {
                    anchor: anchor(30, 0o600, 1),
                    file_hash: digest('4')
                }
            )
            .is_err()
    );
    assert!(
        retired()
            .staging(parent(4), anchor(20, 0o700, 2), digest('5'))
            .is_err()
    );
}

#[test]
fn logical_store_identity_is_preserved_without_using_physical_hashes() {
    let mut intent = initial();
    let ExternalMigrationIntentV2::RetirePrepared { record } = &mut intent else {
        unreachable!()
    };
    record.binding.source_store_identity = Some(digest('3'));
    assert!(intent.validate().is_err());
    record_without_source_identity_is_valid();
    fn record_without_source_identity_is_valid() {
        let mut intent = initial();
        let ExternalMigrationIntentV2::RetirePrepared { record } = &mut intent else {
            unreachable!()
        };
        record.binding.source_store_identity = None;
        assert!(intent.validate().is_ok());
    }
}

#[test]
fn entry_manifest_hash_and_coordinator_observation_cannot_be_substituted() {
    let mut intent = initial();
    let ExternalMigrationIntentV2::RetirePrepared { record } = &mut intent else {
        unreachable!()
    };
    record.binding.entry_manifest.entries[0].binary.anchor.inode += 1;
    assert!(intent.validate().is_err());
    let mut intent = initial();
    let ExternalMigrationIntentV2::RetirePrepared { record } = &mut intent else {
        unreachable!()
    };
    record.binding.stop.coordinator.sha256 = digest('f');
    assert!(intent.validate().is_err());
}

#[test]
fn runtime_copy_observation_is_data_and_bundled_physical_aliases_are_refused() {
    let mut intent = initial();
    let ExternalMigrationIntentV2::RetirePrepared { record } = &mut intent else {
        unreachable!()
    };
    record.binding.entry_manifest.runtime_copy_source_hashes = vec![digest('a')];
    record.binding.entry_manifest_hash =
        hash_bytes(&encode(&record.binding.entry_manifest).unwrap());
    record.binding.stop.coordinator.path = "/private/runtime/polaris".into();
    record.binding.stop.coordinator.anchor.inode = 101;
    intent.validate().unwrap();
    assert_eq!(
        ExternalMigrationIntentV2::decode(&intent.encode().unwrap()).unwrap(),
        intent
    );
    let ExternalMigrationIntentV2::RetirePrepared { record } = &mut intent else {
        unreachable!()
    };
    let mut alias = record.binding.entry_manifest.entries[0].clone();
    alias.kind = ProductionEntryKindV2::ExecutionHelper;
    alias.binary.path = "/verified/helper".into();
    record.binding.entry_manifest.entries.push(alias);
    record.binding.entry_manifest_hash =
        hash_bytes(&encode(&record.binding.entry_manifest).unwrap());
    assert!(intent.validate().is_err());
}

#[test]
fn unknown_fields_versions_states_and_persisted_grants_are_refused() {
    let bytes = initial().encode().unwrap();
    assert!(
        ExternalMigrationIntentV2::decode(&injected_field(&bytes, "\"write_granted\":true"))
            .is_err()
    );
    let mut intent = initial();
    let ExternalMigrationIntentV2::RetirePrepared { record } = &mut intent else {
        unreachable!()
    };
    record.binding.schema_version = 3;
    assert!(ExternalMigrationIntentV2::decode(&encode(&intent).unwrap()).is_err());
    let unknown = String::from_utf8(bytes)
        .unwrap()
        .replace("retire_prepared", "resumable_anything");
    assert!(ExternalMigrationIntentV2::decode(unknown.as_bytes()).is_err());
    assert!(
        serde_json::from_str::<StopObservationV2>(
            "{\"result\":true,\"coordinator\":{},\"inspected_processes\":7}"
        )
        .is_err()
    );
}

#[test]
fn typed_layout_rejects_cross_migration_or_role_aliases() {
    let mut intent = initial();
    let ExternalMigrationIntentV2::RetirePrepared { record } = &mut intent else {
        unreachable!()
    };
    record.binding.layout.staging = ProtocolSiblingV2::StagingV2 {
        migration_id: digest('f'),
    };
    assert!(intent.validate().is_err());
    let layout = ProtocolLayoutV2::new(&digest('1')).unwrap();
    assert_eq!(layout.source.component(), "mcp");
    assert_eq!(layout.active.component(), "mcp-v2");
    assert_eq!(
        layout.staging.component(),
        format!("mcp-v2.staging-{}", digest('1'))
    );
    assert!(
        serde_json::from_str::<ProtocolSiblingV2>(
            "{\"kind\":\"source_v1\",\"path\":\"../outside\"}"
        )
        .is_err()
    );
}

#[test]
fn canonical_external_decode_rejects_whitespace_and_duplicate_fields() {
    let bytes = initial().encode().unwrap();
    let mut spaced = bytes.clone();
    spaced.push(b'\n');
    assert!(ExternalMigrationIntentV2::decode(&spaced).is_err());
    assert!(
        ExternalMigrationIntentV2::decode(&injected_field(&bytes, "\"phase\":\"retire_prepared\""))
            .is_err()
    );
}

#[test]
fn epoch_binding_rejects_wrong_bytes_identity_or_payload_hash() {
    let stage = staging();
    let good = epoch(&stage_record());
    let mut wrong = good.clone();
    wrong.store_identity = digest('f');
    assert!(
        stage
            .prepared_activation(parent(4), &wrong, &wrong.encode().unwrap())
            .is_err()
    );
    let mut bytes = good.encode().unwrap();
    bytes.push(b'\n');
    assert!(stage.prepared_activation(parent(4), &good, &bytes).is_err());
    let ExternalMigrationIntentV2::PreparedActivation { record } = prepared() else {
        unreachable!()
    };
    let mut wrong = good;
    wrong.activation_tree_hash = digest('f');
    assert!(
        record
            .validate_epoch(&wrong, &wrong.encode().unwrap())
            .is_err()
    );
}

#[test]
fn store_record_round_trip_has_no_self_reference_hash_fields() {
    let (record, source, actual) = store(true, true);
    let bytes = record.encode().unwrap();
    let decoded = StoreMigrationRecordV2::decode(&bytes).unwrap();
    decoded
        .validate_registry_source(source.as_ref(), &actual)
        .unwrap();
    assert_eq!(decoded, record);
    let text = String::from_utf8(bytes.clone()).unwrap();
    for field in [
        "activation_tree_hash",
        "epoch_file_hash",
        "previous_intent_hash",
        "prepared_intent_hash",
        "self_digest",
    ] {
        assert!(!text.contains(field));
        assert!(
            StoreMigrationRecordV2::decode(&injected_field(
                &bytes,
                &format!("\"{field}\":\"{}\"", digest('f'))
            ))
            .is_err()
        );
    }
    assert_eq!(record.canonical_hash().unwrap(), hash_bytes(&bytes));
}

#[test]
fn management_bindings_are_mandatory_ordered_and_disjoint_from_all_store_objects() {
    let (mut record, _, _) = store(false, false);
    let row = ManagementMigrationBindingV2 {
        state_path: b"/custom/state/0123456789abcdef".to_vec(),
        migration_id: record.binding.migration_id.clone(),
        project_scope: digest('a'),
        canonical_parent_anchor: anchor(100, 0o700, 4),
        retired_source_anchor: anchor(101, 0o700, 2),
        tombstone: TombstoneLedgerAnchorV1 {
            anchor: anchor(102, 0o600, 1),
            file_hash: digest('b'),
        },
        planned_active_v2_root_anchor: anchor(103, 0o700, 2),
        migration_manifest_hash: digest('c'),
    };
    record.management_migrations = vec![row.clone()];
    assert_eq!(
        StoreMigrationRecordV2::decode(&record.encode().unwrap()).unwrap(),
        record
    );
    let mut non_utf8 = record.clone();
    non_utf8.management_migrations[0]
        .state_path
        .splice(1..1, [0xff]);
    assert_eq!(
        StoreMigrationRecordV2::decode(&non_utf8.encode().unwrap()).unwrap(),
        non_utf8
    );
    let mut wire = serde_json::to_value(&record).unwrap();
    wire.as_object_mut()
        .unwrap()
        .remove("management_migrations");
    assert!(StoreMigrationRecordV2::decode(&serde_json::to_vec(&wire).unwrap()).is_err());
    for kind in 0..9 {
        let mut changed = record.clone();
        let row = &mut changed.management_migrations[0];
        match kind {
            0 => row.migration_id = digest('e'),
            1 => row.project_scope = "0".repeat(64),
            2 => row.state_path = b"/custom/../state/0123456789abcdef".to_vec(),
            3 => row.tombstone.anchor.links = 2,
            4 => row.planned_active_v2_root_anchor.device += 1,
            5 => row.migration_manifest_hash = "invalid".into(),
            6 => row.retired_source_anchor = row.planned_active_v2_root_anchor,
            7 => row.canonical_parent_anchor = changed.binding.canonical_parent_anchor,
            _ => {
                let duplicate = row.clone();
                changed.management_migrations.push(duplicate);
            }
        }
        assert!(changed.encode().is_err(), "{kind}");
    }
    let mut other = row.clone();
    other.state_path = b"/custom/state/1123456789abcdef".to_vec();
    for anchor in [
        &mut other.canonical_parent_anchor,
        &mut other.retired_source_anchor,
        &mut other.tombstone.anchor,
        &mut other.planned_active_v2_root_anchor,
    ] {
        anchor.inode += 100;
    }
    record.management_migrations.push(other);
    record.encode().unwrap();
    record.management_migrations.reverse();
    assert!(record.encode().is_err());
}

#[test]
fn secret_metadata_is_identical_for_different_values_and_has_no_content_fields() {
    let credential = CredentialDocumentV2::baseline(
        &digest('1'),
        Some(Secrets::new(7, BTreeMap::from([("owned_ref".into(), "one".into())])).unwrap()),
    )
    .unwrap();
    let mut wire: serde_json::Value =
        serde_json::from_slice(&credential.encode().unwrap()).unwrap();
    wire["payload"]["values"]["owned_ref"] = "a much longer different value".into();
    let different = CredentialDocumentV2::decode(&serde_json::to_vec(&wire).unwrap()).unwrap();
    let first = BaselineDocumentRecordV2::credential(
        &credential,
        Some(anchor(51, 0o600, 1)),
        anchor(61, 0o600, 1),
    )
    .unwrap();
    let second = BaselineDocumentRecordV2::credential(
        &different,
        Some(anchor(51, 0o600, 1)),
        anchor(61, 0o600, 1),
    )
    .unwrap();
    assert_eq!(encode(&first).unwrap(), encode(&second).unwrap());
    let secret = SourceInventoryEntryV2::SecretDocument {
        relative_path: b"secrets.json".to_vec(),
        anchor: anchor(51, 0o600, 1),
    };
    let bytes = encode(&secret).unwrap();
    let text = String::from_utf8(bytes.clone()).unwrap();
    assert!(!text.contains("sha256"));
    assert!(!text.contains("size"));
    assert!(!text.contains("one"));
    for field in ["\"sha256\":\"bad\"", "\"size\":3", "\"values\":{}"] {
        assert!(
            serde_json::from_slice::<SourceInventoryEntryV2>(&injected_field(&bytes, field))
                .is_err()
        );
    }
}

#[test]
fn secret_path_cannot_be_public_hashed_or_relocated_into_a_package() {
    let (mut record, _, _) = store(true, true);
    record.source_inventory[0] = SourceInventoryEntryV2::PublicFile {
        relative_path: b"secrets.json".to_vec(),
        anchor: anchor(51, 0o600, 1),
        size: 5,
        sha256: digest('c'),
    };
    assert!(record.validate().is_err());
    let entries = vec![
        SourceInventoryEntryV2::Directory {
            relative_path: b"servers".to_vec(),
            anchor: anchor(70, 0o700, 2),
        },
        SourceInventoryEntryV2::Directory {
            relative_path: b"servers/alpha".to_vec(),
            anchor: anchor(71, 0o700, 2),
        },
        SourceInventoryEntryV2::SecretDocument {
            relative_path: b"servers/alpha/secrets.json".to_vec(),
            anchor: anchor(72, 0o600, 1),
        },
    ];
    assert!(validate_inventory(&entries, &anchor(20, 0o700, 2)).is_err());
}

#[test]
fn inventory_requires_raw_global_order_unique_paths_and_directory_ancestry() {
    let directory = |path: &[u8], inode| SourceInventoryEntryV2::Directory {
        relative_path: path.to_vec(),
        anchor: anchor(inode, 0o700, 2),
    };
    let file = |path: &[u8], inode| SourceInventoryEntryV2::PublicFile {
        relative_path: path.to_vec(),
        anchor: anchor(inode, 0o600, 1),
        size: 1,
        sha256: digest('c'),
    };
    let root = anchor(20, 0o700, 2);
    let good = vec![
        directory(b"servers", 70),
        directory(b"servers/alpha", 71),
        file(b"servers/alpha/foo-bar", 72),
        directory(b"servers/alpha/foo", 73),
    ];
    assert!(validate_inventory(&good, &root).is_err());
    let good = vec![
        directory(b"servers", 70),
        directory(b"servers/alpha", 71),
        directory(b"servers/alpha/foo", 73),
        file(b"servers/alpha/foo-bar", 72),
        file(b"servers/alpha/foo/child", 74),
        file(b"servers/alpha/\xff", 75),
    ];
    validate_inventory(&good, &root).unwrap();
    let mut duplicate = good.clone();
    duplicate.insert(2, good[1].clone());
    assert!(validate_inventory(&duplicate, &root).is_err());
    assert!(validate_inventory(&[file(b"servers/alpha/child", 74)], &root).is_err());
    for bad in [
        b"/servers".as_slice(),
        b"servers/../alpha",
        b"servers/./alpha",
        b"servers//alpha",
        b"servers/alpha/zero\0",
    ] {
        assert!(validate_inventory(&[directory(bad, 70)], &root).is_err());
    }
}

#[test]
fn full_materialized_inventory_is_not_capped_by_registry_count_or_json_limit() {
    let (mut record, _, _) = store(false, false);
    record
        .source_inventory
        .push(SourceInventoryEntryV2::Directory {
            relative_path: b"servers".to_vec(),
            anchor: anchor(70, 0o700, 2),
        });
    record.package_directories.push(PackageDirectoryMappingV2 {
        relative_path: b"servers".to_vec(),
        source_anchor: anchor(70, 0o700, 2),
        staged_anchor: anchor(700, 0o700, 2),
    });
    for index in 0..40_u64 {
        let prefix = format!("servers/unregistered-{index:02}");
        record
            .source_inventory
            .push(SourceInventoryEntryV2::Directory {
                relative_path: prefix.as_bytes().to_vec(),
                anchor: anchor(1000 + index, 0o700, 2),
            });
        record.package_directories.push(PackageDirectoryMappingV2 {
            relative_path: prefix.as_bytes().to_vec(),
            source_anchor: anchor(1000 + index, 0o700, 2),
            staged_anchor: anchor(7000 + index, 0o700, 2),
        });
        for child in 0..100_u64 {
            let relative_path = format!("{prefix}/file-{child:03}").into_bytes();
            let source_anchor = anchor(2000 + index * 100 + child, 0o600, 1);
            let staged_anchor = anchor(10000 + index * 100 + child, 0o600, 1);
            record
                .source_inventory
                .push(SourceInventoryEntryV2::PublicFile {
                    relative_path: relative_path.clone(),
                    anchor: source_anchor,
                    size: 1,
                    sha256: digest('c'),
                });
            record.package_files.push(PackageFileMappingV2 {
                relative_path,
                source_anchor,
                source_size: 1,
                source_sha256: digest('c'),
                staged_anchor,
                staged_size: 1,
                staged_sha256: digest('c'),
            });
        }
    }
    let bytes = record.encode().unwrap();
    assert!(bytes.len() > crate::registration::MAX_JSON_BYTES);
    assert_eq!(
        StoreMigrationRecordV2::decode(&bytes)
            .unwrap()
            .package_files
            .len(),
        4000
    );
}

#[test]
fn package_mapping_cannot_omit_a_file_change_opaque_bytes_or_alias_source() {
    let (mut record, _, _) = store(false, false);
    record.source_inventory = vec![
        SourceInventoryEntryV2::Directory {
            relative_path: b"servers".to_vec(),
            anchor: anchor(70, 0o700, 2),
        },
        SourceInventoryEntryV2::Directory {
            relative_path: b"servers/alpha".to_vec(),
            anchor: anchor(71, 0o700, 2),
        },
        SourceInventoryEntryV2::PublicFile {
            relative_path: b"servers/alpha/opaque".to_vec(),
            anchor: anchor(72, 0o600, 1),
            size: 1,
            sha256: digest('c'),
        },
    ];
    assert!(record.validate().is_err());
    record.package_directories = vec![
        PackageDirectoryMappingV2 {
            relative_path: b"servers".to_vec(),
            source_anchor: anchor(70, 0o700, 2),
            staged_anchor: anchor(170, 0o700, 2),
        },
        PackageDirectoryMappingV2 {
            relative_path: b"servers/alpha".to_vec(),
            source_anchor: anchor(71, 0o700, 2),
            staged_anchor: anchor(171, 0o700, 2),
        },
    ];
    record.package_files.push(PackageFileMappingV2 {
        relative_path: b"servers/alpha/opaque".to_vec(),
        source_anchor: anchor(72, 0o600, 1),
        source_size: 1,
        source_sha256: digest('c'),
        staged_anchor: anchor(73, 0o600, 1),
        staged_size: 1,
        staged_sha256: digest('c'),
    });
    record.validate().unwrap();
    record.package_files[0].staged_sha256 = digest('f');
    assert!(record.validate().is_err());
    record.package_files[0].staged_sha256 = digest('c');
    record.package_files[0].staged_anchor = anchor(72, 0o600, 1);
    assert!(record.validate().is_err());
    record.package_files[0].staged_anchor = anchor(73, 0o600, 1);
    record.package_directories[1].staged_anchor = anchor(170, 0o700, 2);
    assert!(record.validate().is_err());
}

#[test]
fn baseline_presence_receipt_token_and_source_raw_registry_hash_are_bound() {
    let (record, source, actual) = store(true, false);
    let mut changed = record.clone();
    changed.registry.metadata.token = digest('f');
    assert!(changed.validate().is_err());
    let mut changed = record.clone();
    changed.credential.source_anchor = Some(anchor(51, 0o600, 1));
    assert!(changed.validate().is_err());
    let mut changed = record.clone();
    let SourceInventoryEntryV2::PublicFile { sha256, .. } = &mut changed.source_inventory[0] else {
        unreachable!()
    };
    *sha256 = digest('f');
    assert!(changed.validate().is_err());
    let mut changed = record;
    changed
        .managed_paths
        .as_mut()
        .unwrap()
        .source_registry_payload_hash = digest('f');
    assert!(
        changed
            .validate_registry_source(source.as_ref(), &actual)
            .is_err()
    );
}

#[test]
fn actual_credential_metadata_and_baseline_receipt_bind_present_and_absent_documents() {
    for present in [false, true] {
        let (record, source, registry, credential) = store_with_credential(false, present);
        record.validate_credential(&credential).unwrap();
        let decoded = StoreMigrationRecordV2::decode(&record.encode().unwrap()).unwrap();
        decoded.validate_credential(&credential).unwrap();
        decoded
            .validate_registry_source(source.as_ref(), &registry)
            .unwrap();
    }
}

#[test]
fn self_consistent_credential_substitutions_are_rejected_by_actual_binding_and_constructor() {
    let fixture = ObservationFixture::new(&digest('1'));
    let (record, source, registry, credential) = store_with_credential(true, true);
    let mut token = record.clone();
    token.credential.metadata.token = digest('f');
    token.credential.receipt.random_token = digest('f');
    token.credential.metadata.receipt_hash = (DocumentReceiptV2::MigrationBaseline {
        receipt: token.credential.receipt.clone(),
    })
    .canonical_hash()
    .unwrap();

    let mut references = record.clone();
    references.credential.receipt.reference_set_hash = digest('f');
    references.credential.metadata.receipt_hash = (DocumentReceiptV2::MigrationBaseline {
        receipt: references.credential.receipt.clone(),
    })
    .canonical_hash()
    .unwrap();

    let alternate = CredentialDocumentV2::baseline(
        &digest('1'),
        Some(
            Secrets::new(
                8,
                BTreeMap::from([("different_ref".into(), "owned-different".into())]),
            )
            .unwrap(),
        ),
    )
    .unwrap();
    let mut receipt = record;
    receipt.credential = BaselineDocumentRecordV2::credential(
        &alternate,
        Some(anchor(51, 0o600, 1)),
        anchor(61, 0o600, 1),
    )
    .unwrap();

    for changed in [token, references, receipt] {
        // Each substituted record is internally coherent; structural decoding
        // alone must never establish binding to the actual credential envelope.
        changed.validate().unwrap();
        let decoded = StoreMigrationRecordV2::decode(&changed.encode().unwrap()).unwrap();
        assert!(decoded.validate_credential(&credential).is_err());
        assert!(
            StoreMigrationRecordV2::new_from_descriptors(
                fixture.stage(),
                StoreMigrationPayloadV2 {
                    planned_active_anchor: changed.planned_active_anchor,
                    source_inventory: changed.source_inventory.clone(),
                    credential: changed.credential.clone(),
                    registry: changed.registry.clone(),
                    managed_paths: changed.managed_paths.clone(),
                    unregistered_pins: changed.unregistered_pins.clone(),
                    package_directories: changed.package_directories.clone(),
                    package_files: changed.package_files.clone(),
                    runtime_rechecks: changed.runtime_rechecks.clone(),
                },
                source.as_ref(),
                &credential,
                &registry,
                &fixture.binding.observation().unwrap(),
                &BTreeMap::new(),
            )
            .is_err()
        );
    }
}

#[test]
fn catalog_marks_all_http_rows_using_their_exact_new_destination_hashes() {
    let source = registry(vec![http("z-http"), http("a-http")]);
    let actual =
        RegistryDocumentV2::baseline(&digest('1'), Some(source.clone()), Some(digest('b')))
            .unwrap();
    let record = CatalogRegenerationRecordV2::new(&digest('1'), &actual).unwrap();
    assert_eq!(
        record
            .servers
            .iter()
            .map(|s| s.server_id.as_str())
            .collect::<Vec<_>>(),
        vec!["a-http", "z-http"]
    );
    assert_eq!(
        record.servers[0].new_destination_hash,
        source.servers[1].destination_hash().unwrap()
    );
    assert_eq!(record.empty_catalog.schema_version, 1);
    assert!(record.empty_catalog.servers.is_empty());
    let decoded = CatalogRegenerationRecordV2::decode(&record.encode().unwrap()).unwrap();
    decoded.validate_registry(&actual).unwrap();
    assert_eq!(decoded, record);
}

#[test]
fn catalog_absent_and_present_empty_baselines_remain_distinct() {
    let absent = RegistryDocumentV2::baseline(&digest('1'), None, None).unwrap();
    let present =
        RegistryDocumentV2::baseline(&digest('1'), Some(registry(vec![])), Some(digest('b')))
            .unwrap();
    let absent = CatalogRegenerationRecordV2::new(&digest('1'), &absent).unwrap();
    let present = CatalogRegenerationRecordV2::new(&digest('1'), &present).unwrap();
    assert!(absent.servers.is_empty() && present.servers.is_empty());
    assert_ne!(
        absent.registry_baseline.presence,
        present.registry_baseline.presence
    );
    assert_ne!(
        absent.new_registry_payload_hash,
        present.new_registry_payload_hash
    );
}

#[test]
fn catalog_rejects_omitted_http_old_hash_duplicate_rows_and_nonempty_payload() {
    let actual = RegistryDocumentV2::baseline(
        &digest('1'),
        Some(registry(vec![http("http")])),
        Some(digest('b')),
    )
    .unwrap();
    let good = CatalogRegenerationRecordV2::new(&digest('1'), &actual).unwrap();
    let mut changed = good.clone();
    changed.servers.clear();
    assert!(changed.validate_registry(&actual).is_err());
    let mut changed = good.clone();
    changed.servers[0].new_destination_hash = digest('f');
    assert!(changed.validate_registry(&actual).is_err());
    let mut changed = good.clone();
    changed.servers.push(changed.servers[0].clone());
    assert!(changed.validate().is_err());
    let mut changed = good.clone();
    changed.empty_catalog.schema_version = 0;
    assert!(changed.validate().is_err());
    let mut changed = good.clone();
    changed.empty_catalog.servers.insert(
        "http".into(),
        crate::catalog::index::ServerIndex {
            destination_hash: digest('f'),
            partial: false,
            tools: vec![],
        },
    );
    assert!(changed.validate().is_err());
    assert!(
        CatalogRegenerationRecordV2::decode(&injected_field(
            &good.encode().unwrap(),
            "\"old_catalog_payload\":{}"
        ))
        .is_err()
    );
}

#[test]
fn catalog_binds_generation_token_revision_receipt_and_closed_required_state() {
    let actual = RegistryDocumentV2::baseline(
        &digest('1'),
        Some(registry(vec![http("http")])),
        Some(digest('b')),
    )
    .unwrap();
    let good = CatalogRegenerationRecordV2::new(&digest('1'), &actual).unwrap();
    let mut changed = good.clone();
    changed.generation = digest('f');
    assert!(changed.validate().is_err());
    let mut changed = good.clone();
    changed.registry_baseline.token = digest('f');
    assert!(changed.validate_registry(&actual).is_err());
    let mut changed = good.clone();
    changed.registry_baseline.revision += 1;
    assert!(changed.validate_registry(&actual).is_err());
    let mut changed = good.clone();
    changed.registry_baseline.receipt_hash = digest('f');
    assert!(changed.validate_registry(&actual).is_err());
    let text = String::from_utf8(good.encode().unwrap())
        .unwrap()
        .replace("catalog_regeneration_required", "already_trusted");
    assert!(CatalogRegenerationRecordV2::decode(text.as_bytes()).is_err());
    assert_eq!(
        good.canonical_hash().unwrap(),
        hash_bytes(&good.encode().unwrap())
    );
}

#[test]
fn runtime_rows_bind_actual_stdio_paths_digest_and_resource_order() {
    let mut server = http("stdio");
    server.transport = Destination::Stdio {
        command: "/usr/bin/owned-command".into(),
        args: vec![],
        cwd: "/logical-home".into(),
        sandbox_profile: "read-only".into(),
        secret_env: BTreeMap::new(),
        readable_roots: vec!["/usr/bin".into()],
        executable_sha256: Some(digest('a')),
        script: None,
        script_sha256: None,
    };
    let source = registry(vec![server]);
    let mapped =
        transform_managed_paths_v2(&source, Path::new("/logical-home"), &BTreeMap::new()).unwrap();
    let actual =
        RegistryDocumentV2::baseline(&digest('1'), Some(mapped.registry), Some(digest('b')))
            .unwrap();
    let catalog = CatalogRegenerationRecordV2::new(&digest('1'), &actual).unwrap();
    assert_eq!(catalog.servers[0].server_id, "stdio");
    let (mut record, _, _) = store(true, false);
    record.registry = BaselineDocumentRecordV2::registry(
        &actual,
        Some(anchor(50, 0o600, 1)),
        anchor(60, 0o600, 1),
    )
    .unwrap();
    record.managed_paths = Some(mapped.projection);
    record.runtime_rechecks = vec![ServerRuntimeRecheckV2 {
        server_id: "stdio".into(),
        new_destination_hash: actual.snapshot().unwrap().servers[0]
            .destination_hash()
            .unwrap(),
        resources: vec![
            RuntimeResourceRecheckV2::Command {
                logical_path: "/usr/bin/owned-command".into(),
                anchor: anchor(80, 0o755, 1),
                sha256: digest('a'),
            },
            RuntimeResourceRecheckV2::ReadableRoot {
                index: 0,
                logical_path: "/usr/bin".into(),
                anchor: anchor(81, 0o755, 2),
            },
        ],
    }];
    record
        .validate_registry_source(Some(&source), &actual)
        .unwrap();
    let mut changed = record.clone();
    changed.managed_paths.as_mut().unwrap().servers[0].fields[0].old_value_hash = digest('f');
    assert!(
        changed
            .validate_registry_source(Some(&source), &actual)
            .is_err()
    );
    let mut changed = record.clone();
    let RuntimeResourceRecheckV2::Command { sha256, .. } =
        &mut changed.runtime_rechecks[0].resources[0]
    else {
        unreachable!()
    };
    *sha256 = digest('f');
    assert!(changed.validate_registry(&actual).is_err());
    let mut changed = record.clone();
    changed.runtime_rechecks[0].resources.swap(0, 1);
    assert!(changed.validate().is_err());
    let mut changed = record;
    let RuntimeResourceRecheckV2::Command { logical_path, .. } =
        &mut changed.runtime_rechecks[0].resources[0]
    else {
        unreachable!()
    };
    *logical_path = "/usr/bin/../other".into();
    assert!(changed.validate().is_err());
}

#[test]
fn registry_source_comparison_rejects_nonpath_metadata_changes() {
    let (mut record, source, actual) = store(true, false);
    let mut altered = actual.snapshot().unwrap().clone();
    altered.servers[0].capabilities.tool_call = true;
    let altered =
        RegistryDocumentV2::baseline(&digest('1'), Some(altered), Some(digest('b'))).unwrap();
    record.registry = BaselineDocumentRecordV2::registry(
        &altered,
        Some(anchor(50, 0o600, 1)),
        anchor(60, 0o600, 1),
    )
    .unwrap();
    let DocumentReceiptV2::MigrationBaseline { receipt } = altered.receipt() else {
        unreachable!()
    };
    record
        .managed_paths
        .as_mut()
        .unwrap()
        .new_registry_payload_hash = receipt
        .registry_projection
        .as_ref()
        .unwrap()
        .canonical_payload_hash
        .clone();
    record.validate_registry(&altered).unwrap();
    assert!(
        record
            .validate_registry_source(source.as_ref(), &altered)
            .is_err()
    );
}

#[test]
fn unregistered_pins_bind_actual_files_and_preserve_complete_opaque_package_version() {
    for present_registry in [false, true] {
        let fixture = PinFixture::new(2, present_registry, false);
        fixture.validate(&fixture.record).unwrap();
        let decoded = StoreMigrationRecordV2::decode(&fixture.record.encode().unwrap()).unwrap();
        fixture.validate(&decoded).unwrap();
        assert_eq!(decoded.unregistered_pins.len(), 2);
        let (home, _, _, _) = fixture.source.binding.coordinates().unwrap();
        for (id, (source_path, staged_path)) in &fixture.paths {
            let source: PinnedPathsV1 =
                serde_json::from_slice(&std::fs::read(source_path).unwrap()).unwrap();
            let staged: PinnedPathsV1 =
                serde_json::from_slice(&std::fs::read(staged_path).unwrap()).unwrap();
            assert_eq!(source.package, staged.package);
            assert_eq!(source.version, staged.version);
            assert!(
                staged.package.contains(
                    &home
                        .join(".polaris/mcp/servers")
                        .join(id)
                        .display()
                        .to_string()
                )
            );
            assert_eq!(
                staged.runtime,
                home.join(".polaris/mcp-v2/servers")
                    .join(id)
                    .join("bin/runtime")
            );
            assert_eq!(
                staged.entry,
                home.join(".polaris/mcp-v2/servers")
                    .join(id)
                    .join("entry.js")
            );
        }
    }
}

#[test]
fn unregistered_pin_rows_are_closed_ordered_complete_and_disjoint_from_registered_ids() {
    let fixture = PinFixture::new(2, true, false);
    let good = &fixture.record;
    let mut changed = good.clone();
    changed.unregistered_pins.pop();
    assert!(changed.validate().is_err());
    let mut changed = good.clone();
    changed
        .unregistered_pins
        .push(changed.unregistered_pins[1].clone());
    assert!(changed.validate().is_err());
    let mut changed = good.clone();
    changed.unregistered_pins.swap(0, 1);
    assert!(changed.validate().is_err());
    let mut changed = good.clone();
    changed.unregistered_pins[0].fields.swap(0, 1);
    assert!(changed.validate().is_err());
    let mut changed = good.clone();
    changed.unregistered_pins[0].fields[0].old_value_hash = digest('G');
    assert!(changed.validate().is_err());
    let mut changed = good.clone();
    changed.unregistered_pins[1].server_id = "unregistered-extra".into();
    assert!(changed.validate().is_err());
    let mut value = serde_json::to_value(&good.unregistered_pins[0]).unwrap();
    value["grant"] = serde_json::json!(true);
    assert!(serde_json::from_value::<UnregisteredPinMappingV2>(value).is_err());
    for length in [1, 3] {
        let mut value = serde_json::to_value(&good.unregistered_pins[0]).unwrap();
        value["fields"] =
            serde_json::Value::Array((0..length).map(|_| value["fields"][0].clone()).collect());
        assert!(serde_json::from_value::<UnregisteredPinMappingV2>(value).is_err());
    }
    let mut missing = fixture.pairs();
    missing.pop_last();
    assert!(
        good.validate_unregistered_pin_descriptors(
            &fixture.source.binding.observation().unwrap(),
            fixture.source_registry.as_ref(),
            &fixture.actual_registry,
            &missing,
        )
        .is_err()
    );
    let mut extra = fixture.pairs();
    let pair = *extra.values().next().unwrap();
    extra.insert("unregistered-extra".into(), pair);
    assert!(
        good.validate_unregistered_pin_descriptors(
            &fixture.source.binding.observation().unwrap(),
            fixture.source_registry.as_ref(),
            &fixture.actual_registry,
            &extra,
        )
        .is_err()
    );

    for stdio in [false, true] {
        let id = good.unregistered_pins[0].server_id.clone();
        let mut server = http(&id);
        if stdio {
            server.transport = Destination::Stdio {
                command: "/usr/bin/owned-command".into(),
                args: vec![],
                cwd: "/logical-home".into(),
                sandbox_profile: "read-only".into(),
                secret_env: BTreeMap::new(),
                readable_roots: vec![],
                executable_sha256: Some(digest('a')),
                script: None,
                script_sha256: None,
            };
        }
        let source = registry(vec![server]);
        let mapped =
            transform_managed_paths_v2(&source, Path::new("/logical-home"), &BTreeMap::new())
                .unwrap();
        let raw_hash = good
            .registry
            .receipt
            .registry_projection
            .as_ref()
            .unwrap()
            .source_file_hash
            .clone();
        let actual =
            RegistryDocumentV2::baseline(&digest('1'), Some(mapped.registry), raw_hash).unwrap();
        let mut changed = good.clone();
        changed.registry = BaselineDocumentRecordV2::registry(
            &actual,
            good.registry.source_anchor,
            good.registry.staged_anchor,
        )
        .unwrap();
        changed.managed_paths = Some(mapped.projection);
        if stdio {
            changed.runtime_rechecks = vec![ServerRuntimeRecheckV2 {
                server_id: id,
                new_destination_hash: actual.snapshot().unwrap().servers[0]
                    .destination_hash()
                    .unwrap(),
                resources: vec![RuntimeResourceRecheckV2::Command {
                    logical_path: "/usr/bin/owned-command".into(),
                    anchor: anchor(80, 0o755, 1),
                    sha256: digest('a'),
                }],
            }];
        }
        assert!(
            changed
                .validate_unregistered_pin_descriptors(
                    &fixture.source.binding.observation().unwrap(),
                    Some(&source),
                    &actual,
                    &fixture.pairs(),
                )
                .is_err()
        );
        // Removing the now-registered row makes structural validation succeed;
        // the remaining unregistered row is still required.
        changed.unregistered_pins.remove(0);
        changed.package_files[0].staged_size = changed.package_files[0].source_size;
        changed.package_files[0].staged_sha256 = changed.package_files[0].source_sha256.clone();
        changed
            .validate_registry_source(Some(&source), &actual)
            .unwrap();
    }
}

#[test]
fn unregistered_pin_self_consistent_staged_substitutions_fail_actual_binding_and_constructor() {
    let fixture = PinFixture::new(1, true, false);
    let staged_path = &fixture.paths["unregistered-00"].1;
    let original = std::fs::read(staged_path).unwrap();
    for field in 0..4 {
        let mut pin: PinnedPathsV1 = serde_json::from_slice(&original).unwrap();
        match field {
            0 => pin.package.push_str(" substituted"),
            1 => pin.version.push_str(" substituted"),
            2 => pin.runtime = "/outside/substituted-runtime".into(),
            3 => pin.entry = "/outside/substituted-entry".into(),
            _ => unreachable!(),
        }
        let bytes = encode(&pin).unwrap();
        std::fs::write(staged_path, &bytes).unwrap();
        let mut changed = fixture.record.clone();
        changed.package_files[0].staged_size = bytes.len() as u64;
        changed.package_files[0].staged_sha256 = hash_bytes(&bytes);
        if field == 2 {
            changed.unregistered_pins[0].fields[0].new_value_hash =
                canonical_hash(&serde_json::to_value(&pin.runtime).unwrap()).unwrap();
        }
        if field == 3 {
            changed.unregistered_pins[0].fields[1].new_value_hash =
                canonical_hash(&serde_json::to_value(&pin.entry).unwrap()).unwrap();
        }
        changed.validate().unwrap();
        assert!(fixture.validate(&changed).is_err());
        assert!(fixture.construct(&changed).is_err());
        assert_eq!(std::fs::read(staged_path).unwrap(), bytes);
    }
    std::fs::write(staged_path, &original).unwrap();
    fixture.validate(&fixture.record).unwrap();
}

#[test]
fn unregistered_pin_fd_anchor_size_raw_hash_and_individual_read_limit_are_bound() {
    let fixture = PinFixture::new(2, false, false);
    fixture.validate(&fixture.record).unwrap();
    let pairs = fixture.pairs();
    for swap_source in [false, true] {
        let mut wrong = pairs.clone();
        let &(source, staged) = pairs.get("unregistered-00").unwrap();
        let &(other_source, other_staged) = pairs.get("unregistered-01").unwrap();
        wrong.insert(
            "unregistered-00".into(),
            if swap_source {
                (other_source, staged)
            } else {
                (source, other_staged)
            },
        );
        assert!(
            fixture
                .record
                .validate_unregistered_pin_descriptors(
                    &fixture.source.binding.observation().unwrap(),
                    None,
                    &fixture.actual_registry,
                    &wrong,
                )
                .is_err()
        );
    }
    for change_size in [false, true] {
        let mut changed = fixture.record.clone();
        let relative = changed.package_files[0].relative_path.clone();
        let source = changed
            .source_inventory
            .iter_mut()
            .find(|entry| entry.path() == relative)
            .unwrap();
        let SourceInventoryEntryV2::PublicFile { size, sha256, .. } = source else {
            unreachable!()
        };
        if change_size {
            *size += 1;
            changed.package_files[0].source_size = *size;
        } else {
            *sha256 = digest('f');
            changed.package_files[0].source_sha256 = sha256.clone();
        }
        changed.validate().unwrap();
        assert!(fixture.validate(&changed).is_err());
    }
    let source_path = &fixture.paths["unregistered-00"].0;
    let oversized = vec![b'x'; MAX_JSON_BYTES + 1];
    std::fs::write(source_path, &oversized).unwrap();
    let mut changed = fixture.record.clone();
    let relative = changed.package_files[0].relative_path.clone();
    let SourceInventoryEntryV2::PublicFile { size, sha256, .. } = changed
        .source_inventory
        .iter_mut()
        .find(|entry| entry.path() == relative)
        .unwrap()
    else {
        unreachable!()
    };
    *size = oversized.len() as u64;
    *sha256 = hash_bytes(&oversized);
    changed.package_files[0].source_size = *size;
    changed.package_files[0].source_sha256 = sha256.clone();
    changed.validate().unwrap();
    assert!(matches!(fixture.validate(&changed), Err(Error::Limit)));
    assert_eq!(std::fs::read(source_path).unwrap(), oversized);
}

#[test]
fn unregistered_pin_reads_reject_during_read_and_late_earlier_pair_changes() {
    struct HookReset;
    impl Drop for HookReset {
        fn drop(&mut self) {
            AFTER_PIN_READ.with(|slot| *slot.borrow_mut() = None);
        }
    }
    // 1: per-read metadata, 4: final pair reread, 8: final metadata closure.
    for target_read in [1, 4, 8] {
        let fixture = PinFixture::new(2, false, false);
        fixture.validate(&fixture.record).unwrap();
        let source = fixture.files["unregistered-00"].0.try_clone().unwrap();
        let old_size = source.metadata().unwrap().len();
        let reached = std::rc::Rc::new(std::cell::Cell::new(false));
        let observed = reached.clone();
        let mut calls = 0;
        let _reset = HookReset;
        AFTER_PIN_READ.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move |_| {
                calls += 1;
                if calls == target_read {
                    source.set_len(old_size + 1).unwrap();
                    observed.set(true);
                }
            }));
        });
        assert!(fixture.validate(&fixture.record).is_err());
        assert!(reached.get());
        assert_eq!(
            fixture.files["unregistered-00"].0.metadata().unwrap().len(),
            old_size + 1
        );
    }
}

#[test]
fn decoded_unregistered_pins_require_actual_namespace_and_actual_file_revalidation() {
    let fixture = PinFixture::new(1, false, false);
    let decoded = StoreMigrationRecordV2::decode(&fixture.record.encode().unwrap()).unwrap();
    fixture.validate(&decoded).unwrap();
    let same_id_other_manifest = ObservationFixture::new(&digest('1'));
    assert!(
        decoded
            .validate_unregistered_pin_descriptors(
                &same_id_other_manifest.binding.observation().unwrap(),
                None,
                &fixture.actual_registry,
                &fixture.pairs(),
            )
            .is_err()
    );
    let other_id = ObservationFixture::new(&digest('9'));
    let mut changed = decoded.clone();
    changed.binding.entry_manifest_hash = other_id.binding.coordinates().unwrap().3.into();
    changed.validate().unwrap();
    assert!(
        changed
            .validate_unregistered_pin_descriptors(
                &other_id.binding.observation().unwrap(),
                None,
                &fixture.actual_registry,
                &fixture.pairs(),
            )
            .is_err()
    );
    std::fs::write(
        &fixture.paths["unregistered-00"].1,
        b"external replacement bytes",
    )
    .unwrap();
    assert!(fixture.validate(&decoded).is_err());
    assert_eq!(
        std::fs::read(&fixture.paths["unregistered-00"].1).unwrap(),
        b"external replacement bytes"
    );
}

#[test]
fn forty_unregistered_pin_files_have_no_registry_or_aggregate_json_limit() {
    let fixture = PinFixture::new(40, true, true);
    assert_eq!(fixture.record.unregistered_pins.len(), 40);
    assert!(
        fixture
            .record
            .package_files
            .iter()
            .map(|file| file.source_size)
            .sum::<u64>()
            > MAX_JSON_BYTES as u64
    );
    fixture.validate(&fixture.record).unwrap();
    let decoded = StoreMigrationRecordV2::decode(&fixture.record.encode().unwrap()).unwrap();
    fixture.validate(&decoded).unwrap();
}

#[test]
fn matching_id_and_manifest_cannot_substitute_a_different_home_parent_before_pin_reads() {
    let fixture = PinFixture::new(1, false, false);
    fixture.validate(&fixture.record).unwrap();
    let (_, migration_id, manifest, manifest_hash) = fixture.source.binding.coordinates().unwrap();
    let home_b = fixture.source._root.path().join("other-home");
    owned_directory(&home_b);
    let guard_b = FixtureCoordinates::new(&home_b, migration_id, manifest.clone());
    let (_, id_b, _, hash_b) = guard_b.coordinates().unwrap();
    assert_eq!(id_b, migration_id);
    assert_eq!(hash_b, manifest_hash);
    let parent_b = EntryAnchorV2::of(&guard_b.parent.metadata().unwrap());
    assert!(!same_directory_identity(
        &parent_b,
        &fixture.record.binding.canonical_parent_anchor
    ));

    let id = "unregistered-00";
    let (source_path, staged_path) = &fixture.paths[id];
    let mut source: PinnedPathsV1 =
        serde_json::from_slice(&std::fs::read(source_path).unwrap()).unwrap();
    let old_server_b = home_b.join(".polaris/mcp/servers").join(id);
    source.runtime = old_server_b.join("bin/runtime");
    source.entry = old_server_b.join("entry.js");
    let source_bytes = encode(&source).unwrap();
    let mapped_b = transform_unregistered_pin_v2(None, id, &source_bytes, &home_b).unwrap();
    let staged_bytes = encode(&mapped_b.pin).unwrap();
    std::fs::write(source_path, &source_bytes).unwrap();
    std::fs::write(staged_path, &staged_bytes).unwrap();
    let mut changed = fixture.record.clone();
    changed.unregistered_pins[0].fields = mapped_b.fields.try_into().unwrap();
    changed.package_files[0].source_size = source_bytes.len() as u64;
    changed.package_files[0].source_sha256 = hash_bytes(&source_bytes);
    changed.package_files[0].staged_size = staged_bytes.len() as u64;
    changed.package_files[0].staged_sha256 = hash_bytes(&staged_bytes);
    let relative_path = changed.package_files[0].relative_path.clone();
    let SourceInventoryEntryV2::PublicFile { size, sha256, .. } = changed
        .source_inventory
        .iter_mut()
        .find(|entry| entry.path() == relative_path)
        .unwrap()
    else {
        unreachable!()
    };
    *size = source_bytes.len() as u64;
    *sha256 = hash_bytes(&source_bytes);
    changed
        .validate_credential(&fixture.actual_credential)
        .unwrap();
    changed
        .validate_registry_source(None, &fixture.actual_registry)
        .unwrap();

    // A codec with B's parent is a positive control for the exact same B
    // logical pin bytes/rows/FDs; named tree provenance belongs to the owner.
    let mut bound_to_b = changed.clone();
    bound_to_b.binding.canonical_parent_anchor = parent_b;
    bound_to_b
        .validate_unregistered_pin_descriptors(
            &guard_b.observation().unwrap(),
            None,
            &fixture.actual_registry,
            &fixture.pairs(),
        )
        .unwrap();
    let read_reached = std::rc::Rc::new(std::cell::Cell::new(false));
    let observed = read_reached.clone();
    AFTER_PIN_READ.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(move |_| observed.set(true)));
    });
    let result = changed.validate_unregistered_pin_descriptors(
        &guard_b.observation().unwrap(),
        None,
        &fixture.actual_registry,
        &fixture.pairs(),
    );
    AFTER_PIN_READ.with(|slot| *slot.borrow_mut() = None);
    assert!(matches!(result, Err(Error::Protocol)));
    assert!(!read_reached.get());
    assert_eq!(std::fs::read(source_path).unwrap(), source_bytes);
    assert_eq!(std::fs::read(staged_path).unwrap(), staged_bytes);
}

#[test]
fn streaming_pin_reader_rechecks_earlier_source_stage_and_reopened_identity() {
    for staged in [false, true] {
        // Two rows; calls 2/4 change the earlier row after it was closed.
        for change_at in [2, 4] {
            let fixture = PinFixture::new(2, false, false);
            let mut calls = 0;
            let mut reached = false;
            let result = fixture.record.validate_unregistered_pin_reader(
                &fixture.source.binding.observation().unwrap(),
                fixture.source_registry.as_ref(),
                &fixture.actual_registry,
                |id| {
                    calls += 1;
                    if calls == change_at {
                        let paths = &fixture.paths["unregistered-00"];
                        let path = if staged { &paths.1 } else { &paths.0 };
                        let bytes = std::fs::read(path).unwrap();
                        std::fs::write(path, bytes).unwrap();
                        reached = true;
                    }
                    let paths = fixture.paths.get(id).ok_or(Error::Protocol)?;
                    Ok((
                        File::open(&paths.0).map_err(pin_storage)?,
                        File::open(&paths.1).map_err(pin_storage)?,
                    ))
                },
            );
            assert!(result.is_err());
            assert!(reached);
        }
    }
    let fixture = PinFixture::new(1, false, false);
    let alternate = fixture.source._root.path().join("alternate-pin");
    std::fs::write(
        &alternate,
        std::fs::read(&fixture.paths["unregistered-00"].0).unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&alternate, std::fs::Permissions::from_mode(0o600)).unwrap();
    let mut calls = 0;
    assert!(
        fixture
            .record
            .validate_unregistered_pin_reader(
                &fixture.source.binding.observation().unwrap(),
                None,
                &fixture.actual_registry,
                |id| {
                    calls += 1;
                    let paths = fixture.paths.get(id).ok_or(Error::Protocol)?;
                    let source = if calls == 2 { &alternate } else { &paths.0 };
                    Ok((
                        File::open(source).map_err(pin_storage)?,
                        File::open(&paths.1).map_err(pin_storage)?,
                    ))
                },
            )
            .is_err()
    );
    assert_eq!(calls, 2);
}
