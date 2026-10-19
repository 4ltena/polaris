//! Immutable, whole-generation v1 -> v2 transformation.
//! This prepares a staging tree; entry exclusion, retirement and activation
//! remain owned by the store protocol gate.
use super::*;
use crate::management_effect::v2::{
    CredentialMutationAuthorizationV1, ManagementEffectSnapshotV2, ManagementUnknownReasonV2,
    validate_recovery_chains,
};
use std::collections::BTreeSet;

const V2_MANIFEST: &str = "manifest.v2.json";
const V2_ARCHIVE: &str = "archive.v2.json";
const V2_CHECKPOINT: &str = "checkpoint.v2.json";
const V2_JOURNAL: &str = "journal.v2.jsonl";
const MIGRATION: &str = "migration.v2.json";
const MIGRATION_TEMP: &str = ".migration.v2.tmp";
const CURRENT_TEMP: &str = ".CURRENT.v2.tmp";

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagementMigrationAnchorV2 {
    pub device: u64,
    pub inode: u64,
    pub uid: u32,
    pub mode: u32,
    pub links: u64,
}
impl ManagementMigrationAnchorV2 {
    fn validate(&self, directory: bool) -> Result<(), ManagementEffectStoreError> {
        if self.inode == 0
            || self.uid != unsafe { libc::geteuid() }
            || self.mode != if directory { 0o700 } else { 0o600 }
            || self.links == 0
            || (!directory && self.links != 1)
        {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagementMigrationAnchorsV2 {
    pub canonical_parent: ManagementMigrationAnchorV2,
    pub retired_source: ManagementMigrationAnchorV2,
    pub tombstone: ManagementMigrationAnchorV2,
    pub active_v2_root: ManagementMigrationAnchorV2,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagementMigrationSourceFileV2 {
    pub relative_name: String,
    pub hash: ManagementEffectHashV1,
    pub bytes: u64,
    pub referenced: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagementMigrationFileMappingV2 {
    pub source_name: String,
    pub source_hash: ManagementEffectHashV1,
    pub destination_name: String,
    pub destination_hash: ManagementEffectHashV1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagementMigrationEventMappingV2 {
    pub source_generation: u64,
    pub source_sequence: u64,
    pub source_hash: ManagementEffectHashV1,
    pub destination_sequence: u64,
    pub destination_hash: ManagementEffectHashV1,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MigrationManifestV2 {
    pub version: u8,
    pub migration_id: ManagementEffectHashV1,
    pub project_scope: ManagementEffectHashV1,
    pub anchors: ManagementMigrationAnchorsV2,
    pub source_current_hash: Option<ManagementEffectHashV1>,
    pub source_files: Vec<ManagementMigrationSourceFileV2>,
    pub generations: u64,
    pub effects: u64,
    pub records: u64,
    pub file_mappings: Vec<ManagementMigrationFileMappingV2>,
    pub event_mappings: Vec<ManagementMigrationEventMappingV2>,
    pub source_effect_hashes: BTreeMap<ManagementOperationIdV1, ManagementEffectHashV1>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CurrentV2 {
    pub version: u8,
    pub generation: u64,
    pub manifest_hash: ManagementEffectHashV1,
    pub migration_manifest_hash: ManagementEffectHashV1,
    pub source_current_hash: Option<ManagementEffectHashV1>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestV2 {
    pub version: u8,
    pub generation: u64,
    pub source_manifest_hash: Option<ManagementEffectHashV1>,
    pub previous_generation: Option<u64>,
    pub previous_manifest_hash: ManagementEffectHashV1,
    pub previous_journal_hash: ManagementEffectHashV1,
    pub previous_sequence: u64,
    pub archive_hash: ManagementEffectHashV1,
    pub checkpoint_hash: ManagementEffectHashV1,
    pub cumulative_effects: u64,
    pub cumulative_records: u64,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ArchiveV2 {
    version: u8,
    terminal: BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV2>,
}
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointV2 {
    version: u8,
    pending: BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV2>,
}

/// Migration-only history. Live replay cannot construct LegacyV1 state.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MigratedEventV2 {
    source_generation: u64,
    source_sequence: u64,
    source_previous_hash: ManagementEffectHashV1,
    source_event_hash: ManagementEffectHashV1,
    source_effect_hash: ManagementEffectHashV1,
    event: ManagementEffectEventV1,
}
#[derive(Serialize)]
struct UnsignedMigrationEnvelopeV2<'a> {
    version: u32,
    generation: u64,
    sequence: u64,
    previous_hash: ManagementEffectHashV1,
    migrated_v1: &'a MigratedEventV2,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct MigrationEnvelopeV2 {
    version: u32,
    generation: u64,
    sequence: u64,
    previous_hash: ManagementEffectHashV1,
    migrated_v1: MigratedEventV2,
    event_hash: ManagementEffectHashV1,
}

/// Keeps immutable source inspection alive until preparation is verified.
/// CURRENT is produced only after the full closure and limits validate.
/// Its hash is not stored in the migration manifest, avoiding the cycle
/// CURRENT -> migration manifest -> CURRENT. Source CURRENT is bound by both.
pub struct ManagementMigrationTreeV2 {
    manifest: MigrationManifestV2,
    files: BTreeMap<String, Vec<u8>>,
}

impl ManagementMigrationTreeV2 {
    pub fn manifest(&self) -> &MigrationManifestV2 {
        &self.manifest
    }
    pub(crate) fn manifest_file_hash(
        &self,
    ) -> Result<ManagementEffectHashV1, ManagementEffectStoreError> {
        self.files
            .get(MIGRATION)
            .map(|bytes| ManagementEffectHashV1::digest(bytes))
            .ok_or(ManagementEffectStoreError::InvalidStore)
    }
    pub(crate) fn verify_staging(&self, root: &File) -> Result<(), ManagementEffectStoreError> {
        let mount = MigrationMountV2::inspect(root, true)?;
        let read = Self::read_staging_inner(root, ManagementEffectLimitsV1::default(), &mount)?;
        if read.manifest != self.manifest || read.files != self.files {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        Ok(())
    }
}

impl ManagementEffectInspectV1 {
    pub fn prepare_migration_v2(
        &self,
        migration_id: ManagementEffectHashV1,
        anchors: ManagementMigrationAnchorsV2,
    ) -> Result<ManagementMigrationTreeV2, ManagementEffectStoreError> {
        if !self.migration_inspection
            || self.tail != ManagementEffectTailV1::Complete
            || migration_id == ManagementEffectHashV1::zero()
        {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        self.verify_pins()?;
        anchors.canonical_parent.validate(true)?;
        anchors.retired_source.validate(true)?;
        anchors.active_v2_root.validate(true)?;
        anchors.tombstone.validate(false)?;
        let source = self.root_directory.metadata()?;
        let parent =
            crate::project_mutation::open_directory_nofollow(&self.store.state_dir)?.metadata()?;
        if anchors.retired_source != migration_anchor(&source)
            || anchors.canonical_parent != migration_anchor(&parent)
            || (anchors.active_v2_root.device, anchors.active_v2_root.inode)
                == (source.dev(), source.ino())
        {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        let chain = self.store.verified_generations_reading(|relative, max| {
            match self.files.get(relative) {
                Some(file) if file.bytes.len() as u64 <= max => Ok(Some(file.bytes.clone())),
                Some(_) => Err(ManagementEffectStoreError::StoreFull),
                None => Ok(None),
            }
        })?;
        let source_effect_hashes = self
            .snapshots
            .iter()
            .map(|(id, source)| {
                Ok((
                    *id,
                    ManagementEffectHashV1::digest(&serde_json::to_vec(source)?),
                ))
            })
            .collect::<Result<BTreeMap<_, _>, ManagementEffectStoreError>>()?;
        let mut manifest = MigrationManifestV2 {
            version: 2,
            migration_id,
            project_scope: self.store.project_scope,
            anchors,
            source_current_hash: self.files.get(CURRENT).map(|file| file.evidence.hash),
            source_files: Vec::new(),
            generations: chain.len() as u64,
            effects: self.snapshots.len() as u64,
            records: self.events.len() as u64,
            file_mappings: Vec::new(),
            event_mappings: Vec::new(),
            source_effect_hashes,
        };
        if manifest.effects > self.store.limits.max_effects as u64
            || manifest.records > self.store.limits.max_records as u64
            || manifest.generations > self.store.limits.max_records as u64
        {
            return Err(ManagementEffectStoreError::StoreFull);
        }
        let mut files = BTreeMap::new();
        let mut referenced = BTreeSet::from([OWNER_LOCK_NAME.to_owned()]);
        if manifest.source_current_hash.is_some() {
            referenced.insert(CURRENT.to_owned());
        }
        let mut previous_manifest = ManagementEffectHashV1::zero();
        let mut previous_journal = ManagementEffectHashV1::zero();
        let mut sequence = 0u64;
        let mut latest_sequence = BTreeMap::new();
        let mut unknown_reasons = BTreeMap::new();
        let mut prior_v2: BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV2> =
            BTreeMap::new();
        let mut cumulative_effects = 0u64;
        for active in &chain {
            let source_name = generation_name(active.number, JOURNAL_NAME);
            referenced.insert(source_name.clone());
            let source_file = self
                .files
                .get(&source_name)
                .ok_or(ManagementEffectStoreError::CorruptJournal)?;
            let state = self.store.parse_journal_bytes(active, &source_file.bytes)?;
            let mut archive = BTreeMap::new();
            let mut pending = BTreeMap::new();
            for (id, effect) in &prior_v2 {
                if effect.state.is_terminal() {
                    archive.insert(*id, effect.clone());
                } else {
                    pending.insert(*id, effect.clone());
                }
            }
            cumulative_effects = cumulative_effects
                .checked_add(archive.len() as u64)
                .ok_or(ManagementEffectStoreError::StoreFull)?;
            let archive_bytes = encode(&ArchiveV2 {
                version: 2,
                terminal: archive,
            })?;
            let checkpoint_bytes = encode(&CheckpointV2 {
                version: 2,
                pending: pending.clone(),
            })?;
            if archive_bytes.len() as u64 > MAX_ARCHIVE_BYTES
                || checkpoint_bytes.len() as u64 > MAX_CHECKPOINT_BYTES
            {
                return Err(ManagementEffectStoreError::StoreFull);
            }
            let before_sequence = sequence;
            let before_journal = previous_journal;
            let mut journal = Vec::new();
            let mut source_previous_hash = ManagementEffectHashV1::zero();
            for verified in state.events.values() {
                sequence = sequence
                    .checked_add(1)
                    .ok_or(ManagementEffectStoreError::StoreFull)?;
                let id = verified.event.operation_id();
                let event = MigratedEventV2 {
                    source_generation: active.number,
                    source_sequence: verified.evidence.sequence,
                    source_previous_hash,
                    source_event_hash: verified.evidence.event_hash,
                    source_effect_hash: *manifest
                        .source_effect_hashes
                        .get(&id)
                        .ok_or(ManagementEffectStoreError::CorruptJournal)?,
                    event: verified.event.clone(),
                };
                let unsigned = UnsignedMigrationEnvelopeV2 {
                    version: 2,
                    generation: active.number,
                    sequence,
                    previous_hash: previous_journal,
                    migrated_v1: &event,
                };
                let event_hash = ManagementEffectHashV1::digest(&serde_json::to_vec(&unsigned)?);
                let line = encode(&MigrationEnvelopeV2 {
                    version: 2,
                    generation: active.number,
                    sequence,
                    previous_hash: previous_journal,
                    migrated_v1: event,
                    event_hash,
                })?;
                if line.len() > self.store.limits.max_record_bytes {
                    return Err(ManagementEffectStoreError::StoreFull);
                }
                journal.extend_from_slice(&line);
                manifest
                    .event_mappings
                    .push(ManagementMigrationEventMappingV2 {
                        source_generation: active.number,
                        source_sequence: verified.evidence.sequence,
                        source_hash: verified.evidence.event_hash,
                        destination_sequence: sequence,
                        destination_hash: event_hash,
                    });
                if let ManagementEffectEventV1::OutcomeUnknown { reason, .. } = verified.event {
                    unknown_reasons.insert(id, ManagementUnknownReasonV2::from(reason));
                }
                latest_sequence.insert(id, sequence);
                source_previous_hash = verified.evidence.event_hash;
                previous_journal = event_hash;
            }
            if journal.len() as u64 > self.store.limits.max_journal_bytes {
                return Err(ManagementEffectStoreError::StoreFull);
            }
            let converted = convert_legacy_snapshots(
                &state.effects,
                &manifest.source_effect_hashes,
                &latest_sequence,
                &unknown_reasons,
            )?;
            validate_recovery_chains(&converted)?;
            for (source, target, bytes) in [
                (ARCHIVE, V2_ARCHIVE, archive_bytes),
                (CHECKPOINT, V2_CHECKPOINT, checkpoint_bytes),
                (JOURNAL_NAME, V2_JOURNAL, journal),
            ] {
                let target_name = generation_name(active.number, target);
                if active.number != 0 || source == JOURNAL_NAME {
                    let source_name = generation_name(active.number, source);
                    referenced.insert(source_name.clone());
                    let source_file = self
                        .files
                        .get(&source_name)
                        .ok_or(ManagementEffectStoreError::CorruptJournal)?;
                    manifest
                        .file_mappings
                        .push(ManagementMigrationFileMappingV2 {
                            source_name,
                            source_hash: source_file.evidence.hash,
                            destination_name: target_name.clone(),
                            destination_hash: ManagementEffectHashV1::digest(&bytes),
                        });
                }
                files.insert(target_name, bytes);
            }
            let source_manifest = if active.number == 0 {
                None
            } else {
                let name = generation_name(active.number, MANIFEST);
                referenced.insert(name.clone());
                Some(
                    self.files
                        .get(&name)
                        .ok_or(ManagementEffectStoreError::CorruptJournal)?
                        .evidence
                        .hash,
                )
            };
            let new_manifest = ManifestV2 {
                version: 2,
                generation: active.number,
                source_manifest_hash: source_manifest,
                previous_generation: active.number.checked_sub(1),
                previous_manifest_hash: previous_manifest,
                previous_journal_hash: before_journal,
                previous_sequence: before_sequence,
                archive_hash: ManagementEffectHashV1::digest(
                    &files[&generation_name(active.number, V2_ARCHIVE)],
                ),
                checkpoint_hash: ManagementEffectHashV1::digest(
                    &files[&generation_name(active.number, V2_CHECKPOINT)],
                ),
                cumulative_effects: cumulative_effects + pending.len() as u64,
                cumulative_records: before_sequence,
            };
            let bytes = encode(&new_manifest)?;
            if bytes.len() as u64 > MAX_MANIFEST_BYTES {
                return Err(ManagementEffectStoreError::StoreFull);
            }
            previous_manifest = ManagementEffectHashV1::digest(&bytes);
            if let Some(source_hash) = source_manifest {
                manifest
                    .file_mappings
                    .push(ManagementMigrationFileMappingV2 {
                        source_name: generation_name(active.number, MANIFEST),
                        source_hash,
                        destination_name: generation_name(active.number, V2_MANIFEST),
                        destination_hash: previous_manifest,
                    });
            }
            files.insert(generation_name(active.number, V2_MANIFEST), bytes);
            prior_v2 = converted;
        }
        for (name, file) in &self.files {
            manifest.source_files.push(ManagementMigrationSourceFileV2 {
                relative_name: name.clone(),
                hash: file.evidence.hash,
                bytes: file.evidence.length,
                referenced: referenced.contains(name),
            });
        }
        let migration = encode(&manifest)?;
        files.insert(MIGRATION.to_owned(), migration.clone());
        files.insert(OWNER_LOCK_NAME.to_owned(), Vec::new());
        let pointer = CurrentV2 {
            version: 2,
            generation: chain
                .last()
                .ok_or(ManagementEffectStoreError::CorruptJournal)?
                .number,
            manifest_hash: previous_manifest,
            migration_manifest_hash: ManagementEffectHashV1::digest(&migration),
            source_current_hash: manifest.source_current_hash,
        };
        files.insert(CURRENT.to_owned(), encode(&pointer)?);
        let total = files
            .values()
            .try_fold(0u64, |sum, bytes| sum.checked_add(bytes.len() as u64))
            .ok_or(ManagementEffectStoreError::StoreFull)?;
        if total > self.store.limits.max_journal_bytes {
            return Err(ManagementEffectStoreError::StoreFull);
        }
        self.verify_pins()?;
        let tree = ManagementMigrationTreeV2 { manifest, files };
        tree.verify(self.store.limits)?;
        Ok(tree)
    }
}

fn generation_name(number: u64, name: &str) -> String {
    if number == 0 {
        name.to_owned()
    } else {
        format!("{GENERATIONS}/{number}/{name}")
    }
}
fn encode(value: &impl Serialize) -> Result<Vec<u8>, ManagementEffectStoreError> {
    let mut bytes = serde_json::to_vec(value)?;
    bytes.push(b'\n');
    Ok(bytes)
}

impl ManagementMigrationTreeV2 {
    /// Replays every retained generation. It never chooses an orphan, repairs
    /// a tail, or treats a migrated credential as a live authorization.
    pub fn verify(
        &self,
        limits: ManagementEffectLimitsV1,
    ) -> Result<
        (
            BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV2>,
            u64,
        ),
        ManagementEffectStoreError,
    > {
        let corrupt = || ManagementEffectStoreError::CorruptJournal;
        let migration_bytes = self.files.get(MIGRATION).ok_or_else(corrupt)?;
        let manifest: MigrationManifestV2 = decode(migration_bytes)?;
        if manifest != self.manifest
            || manifest.version != 2
            || manifest.migration_id == ManagementEffectHashV1::zero()
            || manifest.project_scope == ManagementEffectHashV1::zero()
            || manifest.generations == 0
            || manifest.generations > limits.max_records as u64
            || manifest.records > limits.max_records as u64
            || manifest.effects > limits.max_effects as u64
            || manifest.source_effect_hashes.len() as u64 != manifest.effects
        {
            return Err(corrupt());
        }
        manifest.anchors.canonical_parent.validate(true)?;
        manifest.anchors.retired_source.validate(true)?;
        manifest.anchors.active_v2_root.validate(true)?;
        manifest.anchors.tombstone.validate(false)?;
        let parent_device = manifest.anchors.canonical_parent.device;
        if manifest.anchors.retired_source.device != parent_device
            || manifest.anchors.active_v2_root.device != parent_device
            || manifest.anchors.tombstone.device != parent_device
            || manifest.anchors.retired_source.inode == manifest.anchors.active_v2_root.inode
        {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        let total = self
            .files
            .values()
            .try_fold(0u64, |sum, bytes| sum.checked_add(bytes.len() as u64))
            .ok_or(ManagementEffectStoreError::StoreFull)?;
        if total > limits.max_journal_bytes {
            return Err(ManagementEffectStoreError::StoreFull);
        }
        let pointer_bytes = self.files.get(CURRENT).ok_or_else(corrupt)?;
        if pointer_bytes.len() as u64 > MAX_POINTER_BYTES {
            return Err(ManagementEffectStoreError::StoreFull);
        }
        let pointer: CurrentV2 = decode(pointer_bytes)?;
        if pointer.version != 2
            || pointer.generation.checked_add(1) != Some(manifest.generations)
            || pointer.migration_manifest_hash != ManagementEffectHashV1::digest(migration_bytes)
            || pointer.source_current_hash != manifest.source_current_hash
            || manifest.source_current_hash.is_some() != (pointer.generation != 0)
            || self
                .files
                .get(OWNER_LOCK_NAME)
                .is_none_or(|bytes| !bytes.is_empty())
        {
            return Err(corrupt());
        }
        let mut source_files = BTreeMap::new();
        let mut last_name: Option<&str> = None;
        for source in &manifest.source_files {
            if !valid_source_name(&source.relative_name)
                || source.hash == ManagementEffectHashV1::zero()
                || source.bytes > source_file_limit(&source.relative_name, limits)
                || last_name.is_some_and(|name| name >= source.relative_name.as_str())
                || source_files
                    .insert(source.relative_name.clone(), source)
                    .is_some()
            {
                return Err(corrupt());
            }
            last_name = Some(&source.relative_name);
        }
        if source_files.get(CURRENT).map(|file| file.hash) != manifest.source_current_hash
            || !source_files.contains_key(OWNER_LOCK_NAME)
        {
            return Err(corrupt());
        }
        let mut expected_files = BTreeSet::from([
            CURRENT.to_owned(),
            MIGRATION.to_owned(),
            OWNER_LOCK_NAME.to_owned(),
        ]);
        let mut referenced = BTreeSet::from([OWNER_LOCK_NAME.to_owned()]);
        if pointer.generation != 0 {
            referenced.insert(CURRENT.to_owned());
        }
        let mut expected_mappings = Vec::new();
        let mut event_mappings = Vec::new();
        let mut previous_manifest = ManagementEffectHashV1::zero();
        let mut previous_journal = ManagementEffectHashV1::zero();
        let mut sequence = 0u64;
        let mut archived_effects = 0u64;
        let mut all_v1 = BTreeMap::new();
        let mut all_v2 = BTreeMap::new();
        let mut previous_v1 =
            BTreeMap::<ManagementOperationIdV1, ManagementEffectSnapshotV1>::new();
        let mut previous_v2 =
            BTreeMap::<ManagementOperationIdV1, ManagementEffectSnapshotV2>::new();
        let mut prepared_ids = BTreeSet::new();
        let mut positions = BTreeMap::new();
        let mut reasons = BTreeMap::new();
        for number in 0..manifest.generations {
            let read = |name: &str, max: u64| -> Result<&Vec<u8>, ManagementEffectStoreError> {
                let bytes = self
                    .files
                    .get(&generation_name(number, name))
                    .ok_or_else(corrupt)?;
                if bytes.len() as u64 > max {
                    return Err(ManagementEffectStoreError::StoreFull);
                }
                Ok(bytes)
            };
            let manifest_bytes = read(V2_MANIFEST, MAX_MANIFEST_BYTES)?;
            let generation: ManifestV2 = decode(manifest_bytes)?;
            let archive_bytes = read(V2_ARCHIVE, MAX_ARCHIVE_BYTES)?;
            let checkpoint_bytes = read(V2_CHECKPOINT, MAX_CHECKPOINT_BYTES)?;
            let journal = read(V2_JOURNAL, limits.max_journal_bytes)?;
            let archive: ArchiveV2 = decode(archive_bytes)?;
            let checkpoint: CheckpointV2 = decode(checkpoint_bytes)?;
            let expected_archive = previous_v2
                .iter()
                .filter(|(_, effect)| effect.state.is_terminal())
                .map(|(id, effect)| (*id, effect.clone()))
                .collect::<BTreeMap<_, _>>();
            let expected_pending = previous_v2
                .iter()
                .filter(|(_, effect)| !effect.state.is_terminal())
                .map(|(id, effect)| (*id, effect.clone()))
                .collect::<BTreeMap<_, _>>();
            archived_effects = archived_effects
                .checked_add(expected_archive.len() as u64)
                .ok_or_else(corrupt)?;
            let source_manifest_name = generation_name(number, MANIFEST);
            let source_manifest_hash = if number == 0 {
                None
            } else {
                Some(
                    source_files
                        .get(&source_manifest_name)
                        .ok_or_else(corrupt)?
                        .hash,
                )
            };
            if generation.version != 2
                || generation.generation != number
                || generation.previous_generation != number.checked_sub(1)
                || generation.previous_manifest_hash != previous_manifest
                || generation.previous_journal_hash != previous_journal
                || generation.previous_sequence != sequence
                || generation.source_manifest_hash != source_manifest_hash
                || generation.archive_hash != ManagementEffectHashV1::digest(archive_bytes)
                || generation.checkpoint_hash != ManagementEffectHashV1::digest(checkpoint_bytes)
                || generation.cumulative_records != sequence
                || generation.cumulative_effects != archived_effects + expected_pending.len() as u64
                || archive.version != 2
                || checkpoint.version != 2
                || archive.terminal != expected_archive
                || checkpoint.pending != expected_pending
            {
                return Err(corrupt());
            }
            let mut old = JournalStateV1 {
                generation: number,
                effects: previous_v1
                    .iter()
                    .filter(|(_, effect)| !effect.state.is_terminal())
                    .map(|(id, effect)| (*id, effect.clone()))
                    .collect(),
                ..Default::default()
            };
            if complete_prefix_len(journal) != journal.len() {
                return Err(ManagementEffectStoreError::TornTail);
            }
            for line in journal
                .strip_suffix(b"\n")
                .unwrap_or(journal)
                .split(|byte| *byte == b'\n')
                .filter(|_| !journal.is_empty())
            {
                if line.is_empty() || line.len() + 1 > limits.max_record_bytes {
                    return Err(corrupt());
                }
                let envelope: MigrationEnvelopeV2 =
                    serde_json::from_slice(line).map_err(|_| corrupt())?;
                sequence = sequence.checked_add(1).ok_or_else(corrupt)?;
                if sequence > limits.max_records as u64
                    || envelope.version != 2
                    || envelope.generation != number
                    || envelope.sequence != sequence
                    || envelope.previous_hash != previous_journal
                    || envelope.migrated_v1.source_generation != number
                {
                    return Err(corrupt());
                }
                let expected = ManagementEffectHashV1::digest(&serde_json::to_vec(
                    &UnsignedMigrationEnvelopeV2 {
                        version: 2,
                        generation: number,
                        sequence,
                        previous_hash: previous_journal,
                        migrated_v1: &envelope.migrated_v1,
                    },
                )?);
                if expected != envelope.event_hash {
                    return Err(corrupt());
                }
                let source = &envelope.migrated_v1;
                let id = source.event.operation_id();
                if manifest.source_effect_hashes.get(&id) != Some(&source.source_effect_hash) {
                    return Err(corrupt());
                }
                if let ManagementEffectEventV1::Prepared { request } = &source.event {
                    if !prepared_ids.insert(id) {
                        return Err(corrupt());
                    }
                    validate_request_shape(manifest.project_scope, request)?;
                }
                old.apply(JournalEnvelopeV1 {
                    version: JOURNAL_VERSION,
                    sequence: source.source_sequence,
                    previous_hash: source.source_previous_hash,
                    event: source.event.clone(),
                    event_hash: source.source_event_hash,
                })?;
                if let ManagementEffectEventV1::OutcomeUnknown { reason, .. } = source.event {
                    reasons.insert(id, ManagementUnknownReasonV2::from(reason));
                }
                positions.insert(id, sequence);
                event_mappings.push(ManagementMigrationEventMappingV2 {
                    source_generation: number,
                    source_sequence: source.source_sequence,
                    source_hash: source.source_event_hash,
                    destination_sequence: sequence,
                    destination_hash: envelope.event_hash,
                });
                previous_journal = envelope.event_hash;
            }
            let converted = convert_legacy_snapshots(
                &old.effects,
                &manifest.source_effect_hashes,
                &positions,
                &reasons,
            )?;
            validate_recovery_chains(&converted)?;
            all_v1.extend(old.effects.clone());
            all_v2.extend(converted.clone());
            previous_v1 = old.effects;
            previous_v2 = converted;
            for (source, destination) in [
                (ARCHIVE, V2_ARCHIVE),
                (CHECKPOINT, V2_CHECKPOINT),
                (JOURNAL_NAME, V2_JOURNAL),
                (MANIFEST, V2_MANIFEST),
            ] {
                let destination_name = generation_name(number, destination);
                expected_files.insert(destination_name.clone());
                if number == 0 && source != JOURNAL_NAME {
                    continue;
                }
                let source_name = generation_name(number, source);
                referenced.insert(source_name.clone());
                expected_mappings.push(ManagementMigrationFileMappingV2 {
                    source_hash: source_files.get(&source_name).ok_or_else(corrupt)?.hash,
                    source_name,
                    destination_hash: ManagementEffectHashV1::digest(
                        self.files.get(&destination_name).ok_or_else(corrupt)?,
                    ),
                    destination_name,
                });
            }
            previous_manifest = ManagementEffectHashV1::digest(manifest_bytes);
        }
        let expected_effect_hashes = all_v1
            .iter()
            .map(|(id, effect)| {
                Ok((
                    *id,
                    ManagementEffectHashV1::digest(&serde_json::to_vec(effect)?),
                ))
            })
            .collect::<Result<BTreeMap<_, _>, ManagementEffectStoreError>>()?;
        if pointer.manifest_hash != previous_manifest
            || sequence != manifest.records
            || all_v1.len() as u64 != manifest.effects
            || manifest.source_effect_hashes != expected_effect_hashes
            || manifest.file_mappings != expected_mappings
            || manifest.event_mappings != event_mappings
            || self.files.keys().cloned().collect::<BTreeSet<_>>() != expected_files
            || source_files
                .values()
                .any(|file| file.referenced != referenced.contains(&file.relative_name))
        {
            return Err(corrupt());
        }
        validate_recovery_chains(&all_v2)?;
        Ok((all_v2, sequence))
    }
}
fn convert_legacy_snapshots(
    source: &BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV1>,
    hashes: &BTreeMap<ManagementOperationIdV1, ManagementEffectHashV1>,
    positions: &BTreeMap<ManagementOperationIdV1, u64>,
    reasons: &BTreeMap<ManagementOperationIdV1, ManagementUnknownReasonV2>,
) -> Result<BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV2>, ManagementEffectStoreError>
{
    source
        .iter()
        .map(|(id, effect)| {
            let mut snapshot = ManagementEffectSnapshotV2::from_legacy(effect)?;
            if let Some(CredentialMutationAuthorizationV1::LegacyV1 {
                source_effect_hash, ..
            }) = &mut snapshot.credential_authorization
            {
                *source_effect_hash = *hashes
                    .get(id)
                    .ok_or(ManagementEffectStoreError::CorruptJournal)?;
            }
            snapshot.last_sequence = *positions
                .get(id)
                .ok_or(ManagementEffectStoreError::CorruptJournal)?;
            if snapshot.unknown_reason.is_none() {
                snapshot.unknown_reason = reasons.get(id).copied();
            }
            Ok((*id, snapshot))
        })
        .collect()
}
fn valid_source_name(name: &str) -> bool {
    if matches!(name, CURRENT | JOURNAL_NAME | OWNER_LOCK_NAME) {
        return true;
    }
    if let Some(suffix) = name
        .strip_prefix(".CURRENT-")
        .and_then(|n| n.strip_suffix(".v1.tmp"))
    {
        return suffix.parse::<u64>().is_ok_and(|n| n.to_string() == suffix);
    }
    let parts = name.split('/').collect::<Vec<_>>();
    if parts.len() != 3
        || parts[0] != GENERATIONS
        || !matches!(parts[2], MANIFEST | ARCHIVE | CHECKPOINT | JOURNAL_NAME)
    {
        return false;
    }
    let number = if let Some((number, suffix)) = parts[1]
        .strip_prefix(".orphan-")
        .and_then(|n| n.split_once('-'))
    {
        if !suffix.parse::<u64>().is_ok_and(|n| n.to_string() == suffix) {
            return false;
        }
        number
    } else {
        parts[1]
            .strip_prefix(".seal-")
            .and_then(|n| n.strip_suffix(".v1.tmp"))
            .unwrap_or(parts[1])
    };
    number
        .parse::<u64>()
        .is_ok_and(|n| n != 0 && n.to_string() == number)
}
fn source_file_limit(name: &str, limits: ManagementEffectLimitsV1) -> u64 {
    match name.rsplit('/').next().unwrap_or(name) {
        JOURNAL_NAME => limits.max_journal_bytes,
        ARCHIVE => MAX_ARCHIVE_BYTES,
        CHECKPOINT => MAX_CHECKPOINT_BYTES,
        _ => MAX_POINTER_BYTES,
    }
}

#[cfg(test)]
thread_local! {
    static STAGING_FAILURE: std::cell::Cell<Option<u64>> = const { std::cell::Cell::new(None) };
}
#[cfg(test)]
fn fail_staging_once_at(point: u64) {
    STAGING_FAILURE.with(|slot| slot.set(Some(point)));
}
fn staging_fault(counter: &mut u64) -> Result<(), ManagementEffectStoreError> {
    *counter += 1;
    #[cfg(test)]
    if STAGING_FAILURE.with(|slot| slot.get() == Some(*counter)) {
        STAGING_FAILURE.with(|slot| slot.set(None));
        return Err(std::io::Error::other(format!("injected staging failure {counter}")).into());
    }
    Ok(())
}

impl ManagementMigrationTreeV2 {
    /// The caller pins the final directory layout before preparation. The
    /// final anchor is read after all payload names exist, including hidden
    /// metadata temps, because macOS counts regular entries in directory links.
    /// This finalizes only an unactivated tree; CURRENT is renamed last.
    #[cfg(test)]
    pub(crate) fn write_staging(
        &mut self,
        root: &File,
        limits: ManagementEffectLimitsV1,
    ) -> Result<(), ManagementEffectStoreError> {
        self.write_staging_inner(root, limits, &MigrationMountV2::default())
    }

    pub(crate) fn write_staging_mounted(
        &mut self,
        root: &File,
        limits: ManagementEffectLimitsV1,
    ) -> Result<(), ManagementEffectStoreError> {
        let mount = MigrationMountV2::inspect(root, true)?;
        self.write_staging_inner(root, limits, &mount)
    }

    fn write_staging_inner(
        &mut self,
        root: &File,
        limits: ManagementEffectLimitsV1,
        mount: &MigrationMountV2,
    ) -> Result<(), ManagementEffectStoreError> {
        mount.verify(root)?;
        self.verify(limits)?;
        if migration_anchor(&root.metadata()?) != self.manifest.anchors.active_v2_root {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        let directories = staging_directories(root, self.manifest.generations, false, None, mount)?;
        let mut fault = 0;
        for (relative, bytes) in self
            .files
            .iter()
            .filter(|(name, _)| !matches!(name.as_str(), CURRENT | MIGRATION))
        {
            let (parent, name) = staging_parent(&directories, relative)?;
            write_staging_file(parent, name, relative, bytes, mount)?;
            staging_fault(&mut fault)?;
        }
        let mut migration_file = create_staging_file(root, MIGRATION_TEMP, mount)?;
        staging_fault(&mut fault)?;
        let mut current_file = create_staging_file(root, CURRENT_TEMP, mount)?;
        staging_fault(&mut fault)?;
        let final_anchor = migration_anchor(&root.metadata()?);
        let initial = &self.manifest.anchors.active_v2_root;
        if (
            final_anchor.device,
            final_anchor.inode,
            final_anchor.uid,
            final_anchor.mode,
        ) != (initial.device, initial.inode, initial.uid, initial.mode)
        {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        self.manifest.anchors.active_v2_root = final_anchor;
        let migration = encode(&self.manifest)?;
        let mut pointer: CurrentV2 = decode(&self.files[CURRENT])?;
        pointer.migration_manifest_hash = ManagementEffectHashV1::digest(&migration);
        self.files.insert(MIGRATION.to_owned(), migration);
        self.files.insert(CURRENT.to_owned(), encode(&pointer)?);
        self.verify(limits)?;
        finish_staging_file(
            &mut migration_file,
            root,
            MIGRATION_TEMP,
            MIGRATION_TEMP,
            &self.files[MIGRATION],
            mount,
        )?;
        joined_rename_noreplace(root, MIGRATION_TEMP, root, MIGRATION)?;
        staging_fault(&mut fault)?;
        finish_staging_file(
            &mut current_file,
            root,
            CURRENT_TEMP,
            CURRENT_TEMP,
            &self.files[CURRENT],
            mount,
        )?;
        staging_fault(&mut fault)?;
        // Payload and directory entries are durable before CURRENT becomes visible.
        for directory in directories.values().rev() {
            mount.verify(directory)?;
            directory.sync_all()?;
            mount.verify(directory)?;
            staging_fault(&mut fault)?;
        }
        let partial =
            read_staging_files(root, self.manifest.generations, limits, CURRENT_TEMP, mount)?;
        if partial != self.files
            || migration_anchor(&root.metadata()?) != self.manifest.anchors.active_v2_root
        {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        staging_fault(&mut fault)?;
        joined_rename_noreplace(root, CURRENT_TEMP, root, CURRENT)?;
        staging_fault(&mut fault)?;
        root.sync_all()?;
        staging_fault(&mut fault)?;
        let reopened = Self::read_staging_inner(root, limits, mount)?;
        if reopened.files != self.files || reopened.manifest != self.manifest {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        Ok(())
    }

    /// Read-only descriptor-relative validation of the entire new closure.
    /// Missing CURRENT and partial trees are blockers, never repairs.
    #[cfg(test)]
    pub(crate) fn read_staging(
        root: &File,
        limits: ManagementEffectLimitsV1,
    ) -> Result<Self, ManagementEffectStoreError> {
        Self::read_staging_inner(root, limits, &MigrationMountV2::default())
    }

    fn read_staging_inner(
        root: &File,
        limits: ManagementEffectLimitsV1,
        mount: &MigrationMountV2,
    ) -> Result<Self, ManagementEffectStoreError> {
        mount.verify(root)?;
        let metadata = root.metadata()?;
        migration_anchor(&metadata).validate(true)?;
        let migration = pin_management_file_mounted(
            root,
            MIGRATION,
            MIGRATION,
            limits.max_journal_bytes,
            mount,
        )?;
        let manifest: MigrationManifestV2 = decode(&migration.bytes)?;
        if manifest.version != 2
            || manifest.generations == 0
            || manifest.generations > limits.max_records as u64
            || migration_anchor(&metadata) != manifest.anchors.active_v2_root
        {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        let files = read_staging_files(root, manifest.generations, limits, CURRENT, mount)?;
        verify_management_file(&migration)?;
        let tree = Self { manifest, files };
        tree.verify(limits)?;
        if migration_anchor(&root.metadata()?) != tree.manifest.anchors.active_v2_root {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        mount.verify(root)?;
        Ok(tree)
    }
}

fn migration_anchor(meta: &fs::Metadata) -> ManagementMigrationAnchorV2 {
    ManagementMigrationAnchorV2 {
        device: meta.dev(),
        inode: meta.ino(),
        uid: meta.uid(),
        mode: meta.mode() & 0o7777,
        links: meta.nlink(),
    }
}
fn staging_directories(
    root: &File,
    generations: u64,
    populated: bool,
    current_name: Option<&str>,
    mount: &MigrationMountV2,
) -> Result<BTreeMap<String, File>, ManagementEffectStoreError> {
    use crate::project_mutation::directory_names_bounded;
    mount.verify(root)?;
    let mut directories = BTreeMap::from([(String::new(), root.try_clone()?)]);
    let names = directory_names_bounded(root, 8)?
        .into_iter()
        .collect::<BTreeSet<_>>();
    let mut expected = BTreeSet::new();
    expected.insert(std::ffi::OsString::from(GENERATIONS));
    if populated {
        for name in [
            MIGRATION,
            OWNER_LOCK_NAME,
            V2_MANIFEST,
            V2_ARCHIVE,
            V2_CHECKPOINT,
            V2_JOURNAL,
        ] {
            expected.insert(std::ffi::OsString::from(name));
        }
        if let Some(name) = current_name {
            expected.insert(std::ffi::OsString::from(name));
        }
    }
    if names != expected {
        return Err(ManagementEffectStoreError::InvalidStore);
    }
    {
        let parent = management_child_directory(root, GENERATIONS)?;
        mount.verify(&parent)?;
        let expected = (1..generations)
            .map(|n| std::ffi::OsString::from(n.to_string()))
            .collect::<BTreeSet<_>>();
        let actual = directory_names_bounded(&parent, generations as usize)?
            .into_iter()
            .collect::<BTreeSet<_>>();
        if actual != expected {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        for number in 1..generations {
            let directory = management_child_directory(&parent, &number.to_string())?;
            mount.verify(&directory)?;
            let expected = if populated {
                [V2_MANIFEST, V2_ARCHIVE, V2_CHECKPOINT, V2_JOURNAL]
                    .into_iter()
                    .map(std::ffi::OsString::from)
                    .collect()
            } else {
                BTreeSet::new()
            };
            let actual = directory_names_bounded(&directory, 4)?
                .into_iter()
                .collect::<BTreeSet<_>>();
            if actual != expected {
                return Err(ManagementEffectStoreError::InvalidStore);
            }
            directories.insert(format!("{GENERATIONS}/{number}"), directory);
        }
        directories.insert(GENERATIONS.to_owned(), parent);
    }
    for directory in directories.values() {
        mount.verify(directory)?;
    }
    Ok(directories)
}
fn staging_parent<'a>(
    directories: &'a BTreeMap<String, File>,
    relative: &'a str,
) -> Result<(&'a File, &'a str), ManagementEffectStoreError> {
    let (parent, name) = relative.rsplit_once('/').unwrap_or(("", relative));
    Ok((
        directories
            .get(parent)
            .ok_or(ManagementEffectStoreError::InvalidStore)?,
        name,
    ))
}
fn create_staging_file(
    parent: &File,
    name: &str,
    mount: &MigrationMountV2,
) -> Result<File, ManagementEffectStoreError> {
    mount.verify(parent)?;
    let c_name = CString::new(name).map_err(|_| ManagementEffectStoreError::InvalidStore)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            c_name.as_ptr(),
            libc::O_WRONLY
                | libc::O_CREAT
                | libc::O_EXCL
                | libc::O_NOFOLLOW
                | libc::O_CLOEXEC
                | libc::O_NONBLOCK,
            0o600,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    mount.verify(&file)?;
    if unsafe { libc::fchmod(fd, 0o600) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    verify_management_entry(parent, &c_name, &file)?;
    mount.verify(parent)?;
    mount.verify(&file)?;
    Ok(file)
}
fn finish_staging_file(
    file: &mut File,
    parent: &File,
    name: &str,
    relative: &str,
    bytes: &[u8],
    mount: &MigrationMountV2,
) -> Result<(), ManagementEffectStoreError> {
    mount.verify(parent)?;
    mount.verify(file)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    let c_name = CString::new(name).map_err(|_| ManagementEffectStoreError::InvalidStore)?;
    verify_management_entry(parent, &c_name, file)?;
    mount.verify(parent)?;
    mount.verify(file)?;
    let pinned = pin_management_file_mounted(parent, name, relative, bytes.len() as u64, mount)?;
    if pinned.bytes != bytes {
        return Err(ManagementEffectStoreError::InvalidStore);
    }
    Ok(())
}
fn write_staging_file(
    parent: &File,
    name: &str,
    relative: &str,
    bytes: &[u8],
    mount: &MigrationMountV2,
) -> Result<(), ManagementEffectStoreError> {
    let mut file = create_staging_file(parent, name, mount)?;
    finish_staging_file(&mut file, parent, name, relative, bytes, mount)
}
fn read_staging_files(
    root: &File,
    generations: u64,
    limits: ManagementEffectLimitsV1,
    current_name: &str,
    mount: &MigrationMountV2,
) -> Result<BTreeMap<String, Vec<u8>>, ManagementEffectStoreError> {
    let directories = staging_directories(root, generations, true, Some(current_name), mount)?;
    let mut expected = BTreeMap::from([
        (MIGRATION.to_owned(), limits.max_journal_bytes),
        (OWNER_LOCK_NAME.to_owned(), 4096),
    ]);
    expected.insert(current_name.to_owned(), MAX_POINTER_BYTES);
    for number in 0..generations {
        for (name, max) in [
            (V2_MANIFEST, MAX_MANIFEST_BYTES),
            (V2_ARCHIVE, MAX_ARCHIVE_BYTES),
            (V2_CHECKPOINT, MAX_CHECKPOINT_BYTES),
            (V2_JOURNAL, limits.max_journal_bytes),
        ] {
            expected.insert(generation_name(number, name), max);
        }
    }
    let mut files = BTreeMap::new();
    let mut pins = Vec::new();
    let mut total = 0u64;
    for (relative, max) in expected {
        let (parent, name) = staging_parent(&directories, &relative)?;
        let pinned = pin_management_file_mounted(parent, name, &relative, max, mount)?;
        total = total
            .checked_add(pinned.bytes.len() as u64)
            .ok_or(ManagementEffectStoreError::StoreFull)?;
        if total > limits.max_journal_bytes {
            return Err(ManagementEffectStoreError::StoreFull);
        }
        let logical = if relative == CURRENT_TEMP {
            CURRENT.to_owned()
        } else {
            relative
        };
        files.insert(logical, pinned.bytes.clone());
        pins.push(pinned);
    }
    for file in &pins {
        verify_management_file(file)?;
    }
    let reopened = staging_directories(root, generations, true, Some(current_name), mount)?;
    for (name, directory) in &directories {
        if migration_anchor(&directory.metadata()?) != migration_anchor(&reopened[name].metadata()?)
        {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
    }
    Ok(files)
}

#[cfg(test)]
#[path = "migration_v2/tests.rs"]
mod tests;
