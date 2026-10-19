//! Shared private migration guard fixtures and canonical intent records.
use super::entry_gate::*;
use polaris_mcp::migration_observation::*;
use polaris_mcp::store::ledger_v1::{RetiredLedgerAnchorV1, TombstoneLedgerAnchorV1};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
pub(super) struct GuardFixture {
    pub(super) _root: tempfile::TempDir,
    pub(super) guard: EntryMigrationGuardV2,
}
impl GuardFixture {
    pub(super) fn new(migration_id: &str) -> Self {
        Self::new_inner(migration_id, false)
    }
    pub(super) fn new_for_current_coordinator(migration_id: &str) -> Self {
        Self::new_inner(migration_id, true)
    }
    fn new_inner(migration_id: &str, current_coordinator: bool) -> Self {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("home");
        std::fs::create_dir(&home).unwrap();
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o700)).unwrap();
        let binary = root.path().join("polaris");
        std::fs::write(&binary, b"synthetic executable identity\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o500)).unwrap();
        let identity_path = if current_coordinator {
            std::env::current_exe().unwrap()
        } else {
            binary
        };
        let manifest = ProductionEntryManifestV2 {
            protocol_version: 2,
            entries: vec![ProductionEntryIdentityV2 {
                kind: ProductionEntryKindV2::Cli,
                binary: ProductionBinaryIdentityV2::inspect(&identity_path).unwrap(),
            }],
            runtime_copy_source_hashes: vec![],
        };
        let guard = EntryMigrationGuardV2::begin(&home, migration_id, manifest).unwrap();
        Self { _root: root, guard }
    }
    pub(super) fn stage(&self) -> StagingRecordV2 {
        let (_, id, manifest, manifest_hash) = self.guard.verified_binding().unwrap();
        let canonical_parent = EntryAnchorV2::of(&self.guard.parent.metadata().unwrap());
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

    pub(super) fn empty_store_record(
        &self,
    ) -> (
        StoreMigrationRecordV2,
        polaris_mcp::store::documents_v2::RegistryDocumentV2,
    ) {
        use polaris_mcp::store::documents_v2::{CredentialDocumentV2, RegistryDocumentV2};
        let (home, id, _, _) = self.guard.verified_binding().unwrap();
        let credential = CredentialDocumentV2::baseline(id, None).unwrap();
        let registry = RegistryDocumentV2::baseline(id, None, None).unwrap();
        let namespace = MigrationParentObservationV2::inspect(home, &self.guard.parent).unwrap();
        let record = StoreMigrationRecordV2::new_from_descriptors(
            self.stage(),
            polaris_mcp::migration_observation::StoreMigrationPayloadV2 {
                planned_active_anchor: anchor(40, 0o700, 2),
                source_inventory: vec![],
                credential: BaselineDocumentRecordV2::credential(
                    &credential,
                    None,
                    anchor(61, 0o600, 1),
                )
                .unwrap(),
                registry: BaselineDocumentRecordV2::registry(&registry, None, anchor(60, 0o600, 1))
                    .unwrap(),
                managed_paths: None,
                unregistered_pins: vec![],
                package_directories: vec![],
                package_files: vec![],
                runtime_rechecks: vec![],
            },
            None,
            &credential,
            &registry,
            &namespace,
            &std::collections::BTreeMap::new(),
        )
        .unwrap();
        (record, registry)
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
        entry_manifest_hash: polaris_tools::entry_kernel::canonical_hash(&manifest).unwrap(),
        entry_manifest: manifest,
        stop: StopObservationV2 {
            result: StopObservationResultV2::ZeroProductionProcesses,
            coordinator: binary,
            inspected_processes: 7,
        },
    })
    .unwrap()
}
