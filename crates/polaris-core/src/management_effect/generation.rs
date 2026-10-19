//! Durable management-effect generations, checkpoints and sealed archives.

use super::*;
#[cfg(unix)]
#[path = "generation/migration_v2.rs"]
mod migration_v2;
#[cfg(unix)]
pub use migration_v2::{
    CurrentV2, ManagementMigrationAnchorV2, ManagementMigrationAnchorsV2,
    ManagementMigrationTreeV2, ManifestV2, MigrationManifestV2,
};

use std::sync::atomic::{AtomicU64, Ordering};

const GENERATIONS: &str = "generations";
const CURRENT: &str = "CURRENT";
const MANIFEST: &str = "manifest.v1.json";
const ARCHIVE: &str = "archive.v1.json";
const CHECKPOINT: &str = "checkpoint.v1.json";
const FORMAT_VERSION: u8 = 1;
const MAX_POINTER_BYTES: u64 = 4096;
const MAX_MANIFEST_BYTES: u64 = 4096;
const MAX_CHECKPOINT_BYTES: u64 = 64 * 1024;
const MAX_ARCHIVE_BYTES: u64 = 64 * 1024 * 1024;
static FILE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

#[cfg(test)]
thread_local! {
    static SEAL_FAILURE: std::cell::Cell<Option<u8>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) fn fail_seal_once_at(point: u8) {
    SEAL_FAILURE.with(|slot| slot.set(Some(point)));
}

#[cfg(test)]
pub(super) fn reset_file_sequence_for_restart() {
    FILE_SEQUENCE.store(0, Ordering::SeqCst);
}

#[cfg(test)]
fn seal_fault(point: u8) -> Result<(), ManagementEffectStoreError> {
    let fail = SEAL_FAILURE.with(|slot| slot.get() == Some(point));
    if fail {
        SEAL_FAILURE.with(|slot| slot.set(None));
        return Err(std::io::Error::other(format!("injected seal failure {point}")).into());
    }
    Ok(())
}

#[derive(Clone)]
pub(super) struct ActiveGenerationV1 {
    pub number: u64,
    pub chain_hash: ManagementEffectHashV1,
    pub journal_path: PathBuf,
    pub pending: BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV1>,
    pub archived_effects: u64,
    pub prior_records: u64,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CurrentV1 {
    version: u8,
    generation: u64,
    manifest_hash: ManagementEffectHashV1,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ManifestV1 {
    version: u8,
    generation: u64,
    previous_generation: u64,
    previous_manifest_hash: ManagementEffectHashV1,
    previous_journal_hash: ManagementEffectHashV1,
    previous_sequence: u64,
    cumulative_effects: u64,
    cumulative_records: u64,
    archive_hash: ManagementEffectHashV1,
    checkpoint_hash: ManagementEffectHashV1,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CheckpointV1 {
    version: u8,
    rescue: bool,
    pending: BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV1>,
}

impl ManagementEffectStoreV1 {
    pub(super) fn journal_for_generation(&self, number: u64) -> PathBuf {
        if number == 0 {
            self.journal_path.clone()
        } else {
            self.root
                .join(GENERATIONS)
                .join(number.to_string())
                .join(JOURNAL_NAME)
        }
    }

    fn legacy_generation(&self) -> ActiveGenerationV1 {
        ActiveGenerationV1 {
            number: 0,
            chain_hash: ManagementEffectHashV1::zero(),
            journal_path: self.journal_path.clone(),
            pending: BTreeMap::new(),
            archived_effects: 0,
            prior_records: 0,
        }
    }

    pub(super) fn active_generation(
        &self,
    ) -> Result<ActiveGenerationV1, ManagementEffectStoreError> {
        self.verified_generations()?
            .pop()
            .ok_or(ManagementEffectStoreError::CorruptJournal)
    }

    /// Only follows the chain bound by CURRENT, never directory discovery.
    pub(super) fn verified_generations(
        &self,
    ) -> Result<Vec<ActiveGenerationV1>, ManagementEffectStoreError> {
        self.verified_generations_reading(|relative, max| {
            let path = self.root.join(relative);
            self.m_validate_directory(
                path.parent()
                    .ok_or(ManagementEffectStoreError::InvalidStore)?,
            )?;
            match self.m_bounded(&path, max) {
                Err(ManagementEffectStoreError::Io(error))
                    if error.kind() == std::io::ErrorKind::NotFound =>
                {
                    Ok(None)
                }
                other => other.map(Some),
            }
        })
    }

    /// Inspect supplies bytes from pinned descriptors, using the same chain
    /// and transition validation as the ordinary reader.
    pub(super) fn verified_generations_reading(
        &self,
        read: impl FnMut(&str, u64) -> Result<Option<Vec<u8>>, ManagementEffectStoreError>,
    ) -> Result<Vec<ActiveGenerationV1>, ManagementEffectStoreError> {
        self.verified_generations_reading_scoped(read, Some(self.project_scope))
    }

    // None is used only while observing opaque historical migration scope;
    // every request still validates, and the owner must close a unique scope.
    pub(super) fn verified_generations_reading_scoped(
        &self,
        mut read: impl FnMut(&str, u64) -> Result<Option<Vec<u8>>, ManagementEffectStoreError>,
        scope: Option<ManagementEffectHashV1>,
    ) -> Result<Vec<ActiveGenerationV1>, ManagementEffectStoreError> {
        let pointer: CurrentV1 = match read(CURRENT, MAX_POINTER_BYTES)? {
            None => return Ok(vec![self.legacy_generation()]),
            Some(bytes) => decode(&bytes)?,
        };
        if pointer.version != FORMAT_VERSION || pointer.generation == 0 {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }

        let mut previous = self.legacy_generation();
        let mut generations = vec![previous.clone()];
        let bytes = read(JOURNAL_NAME, self.limits.max_journal_bytes)?
            .ok_or(ManagementEffectStoreError::CorruptJournal)?;
        if complete_prefix_len(&bytes) != bytes.len() {
            return Err(ManagementEffectStoreError::TornTail);
        }
        let mut previous_state = self.parse_journal_bytes_scoped(&previous, &bytes, scope)?;
        for number in 1..=pointer.generation {
            let relative = |name: &str| format!("{GENERATIONS}/{number}/{name}");
            let manifest_bytes = read(&relative(MANIFEST), MAX_MANIFEST_BYTES)?
                .ok_or(ManagementEffectStoreError::CorruptJournal)?;
            let manifest_hash = ManagementEffectHashV1::digest(&manifest_bytes);
            let manifest: ManifestV1 = decode(&manifest_bytes)?;
            if manifest.version != FORMAT_VERSION
                || manifest.generation != number
                || manifest.previous_generation != previous.number
                || manifest.previous_manifest_hash != previous.chain_hash
                || manifest.previous_journal_hash != previous_state.journal_hash
                || manifest.previous_sequence != previous_state.sequence
            {
                return Err(ManagementEffectStoreError::CorruptJournal);
            }
            let archive_bytes = read(&relative(ARCHIVE), MAX_ARCHIVE_BYTES)?
                .ok_or(ManagementEffectStoreError::CorruptJournal)?;
            if ManagementEffectHashV1::digest(&archive_bytes) != manifest.archive_hash {
                return Err(ManagementEffectStoreError::CorruptJournal);
            }
            let archived: BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV1> =
                decode(&archive_bytes)?;
            let checkpoint_bytes = read(&relative(CHECKPOINT), MAX_CHECKPOINT_BYTES)?
                .ok_or(ManagementEffectStoreError::CorruptJournal)?;
            if ManagementEffectHashV1::digest(&checkpoint_bytes) != manifest.checkpoint_hash {
                return Err(ManagementEffectStoreError::CorruptJournal);
            }
            let checkpoint: CheckpointV1 = decode(&checkpoint_bytes)?;
            if checkpoint.version != FORMAT_VERSION
                || checkpoint.rescue == checkpoint.pending.is_empty()
            {
                return Err(ManagementEffectStoreError::CorruptJournal);
            }
            let expected_archive = previous_state
                .effects
                .iter()
                .filter(|(_, effect)| effect.state.is_terminal())
                .map(|(id, effect)| (*id, effect.clone()))
                .collect::<BTreeMap<_, _>>();
            let expected_pending = previous_state
                .effects
                .iter()
                .filter(|(_, effect)| !effect.state.is_terminal())
                .map(|(id, effect)| (*id, effect.clone()))
                .collect::<BTreeMap<_, _>>();
            if archived != expected_archive || checkpoint.pending != expected_pending {
                return Err(ManagementEffectStoreError::CorruptJournal);
            }
            let archived_effects = previous_state
                .archived_effects
                .checked_add(archived.len() as u64)
                .ok_or(ManagementEffectStoreError::CorruptJournal)?;
            let cumulative_effects = archived_effects
                .checked_add(checkpoint.pending.len() as u64)
                .ok_or(ManagementEffectStoreError::CorruptJournal)?;
            let prior_records = previous_state
                .prior_records
                .checked_add(previous_state.records as u64)
                .ok_or(ManagementEffectStoreError::CorruptJournal)?;
            if manifest.cumulative_effects != cumulative_effects
                || manifest.cumulative_records != prior_records
            {
                return Err(ManagementEffectStoreError::CorruptJournal);
            }
            previous = ActiveGenerationV1 {
                number,
                chain_hash: manifest_hash,
                journal_path: self.journal_for_generation(number),
                pending: checkpoint.pending,
                archived_effects,
                prior_records,
            };
            generations.push(previous.clone());
            if number < pointer.generation {
                let bytes = read(&relative(JOURNAL_NAME), self.limits.max_journal_bytes)?
                    .ok_or(ManagementEffectStoreError::CorruptJournal)?;
                if complete_prefix_len(&bytes) != bytes.len() {
                    return Err(ManagementEffectStoreError::TornTail);
                }
                previous_state = self.parse_journal_bytes_scoped(&previous, &bytes, scope)?;
            }
        }
        if previous.chain_hash != pointer.manifest_hash {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        Ok(generations)
    }

    fn generation_directory(&self, number: u64) -> PathBuf {
        self.root.join(GENERATIONS).join(number.to_string())
    }

    fn read_archive(
        &self,
        number: u64,
        expected_hash: ManagementEffectHashV1,
    ) -> Result<
        BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV1>,
        ManagementEffectStoreError,
    > {
        let bytes = self.m_bounded(
            &self.generation_directory(number).join(ARCHIVE),
            MAX_ARCHIVE_BYTES,
        )?;
        if ManagementEffectHashV1::digest(&bytes) != expected_hash {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        let archive: BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV1> =
            decode(&bytes)?;
        if archive.iter().any(|(id, effect)| {
            *id != effect.request.operation_id
                || !effect.state.is_terminal()
                || validate_request_shape(self.project_scope, &effect.request).is_err()
        }) {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        Ok(archive)
    }

    pub(super) fn archived_effect(
        &self,
        operation_id: ManagementOperationIdV1,
    ) -> Result<Option<ManagementEffectSnapshotV1>, ManagementEffectStoreError> {
        let active = self.active_generation()?;
        let mut found = None;
        for number in 1..=active.number {
            let manifest: ManifestV1 = decode(&self.m_bounded(
                &self.generation_directory(number).join(MANIFEST),
                MAX_MANIFEST_BYTES,
            )?)?;
            if let Some(effect) = self
                .read_archive(number, manifest.archive_hash)?
                .remove(&operation_id)
                && found.replace(effect).is_some()
            {
                return Err(ManagementEffectStoreError::CorruptJournal);
            }
        }
        Ok(found)
    }

    pub(super) fn archive_for_page(
        &self,
        number: u64,
    ) -> Result<
        BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV1>,
        ManagementEffectStoreError,
    > {
        let manifest: ManifestV1 = decode(&self.m_bounded(
            &self.generation_directory(number).join(MANIFEST),
            MAX_MANIFEST_BYTES,
        )?)?;
        self.read_archive(number, manifest.archive_hash)
    }

    pub(super) fn seal_generation(
        &self,
        lease: &ProjectMutationLeaseV1,
        state: &JournalStateV1,
        rescue: bool,
    ) -> Result<(), ManagementEffectStoreError> {
        self.check_lease(lease)?;
        if self.load()?.cursor() != state.cursor() {
            return Err(ManagementEffectStoreError::StaleCursor);
        }
        let pending = state
            .effects
            .iter()
            .filter(|(_, effect)| !effect.state.is_terminal())
            .map(|(id, effect)| (*id, effect.clone()))
            .collect::<BTreeMap<_, _>>();
        if rescue == pending.is_empty() {
            return Err(ManagementEffectStoreError::UnresolvedEffect);
        }
        preflight_seal_capacity(state)?;
        let archive = state
            .effects
            .iter()
            .filter(|(_, effect)| effect.state.is_terminal())
            .map(|(id, effect)| (*id, effect.clone()))
            .collect::<BTreeMap<_, _>>();
        let next = state
            .generation
            .checked_add(1)
            .ok_or(ManagementEffectStoreError::StoreFull)?;
        let generations = self.root.join(GENERATIONS);
        self.m_ensure_directory(&generations)?;
        let directory = self.generation_directory(next);
        match self.m_directory_present(&directory) {
            Ok(true) => {
                self.m_validate_directory(&directory)?;
                let mut suffix = FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
                loop {
                    let orphan = generations.join(format!(".orphan-{next}-{suffix}"));
                    match self.m_directory_present(&orphan) {
                        Ok(true) => {
                            suffix = suffix
                                .checked_add(1)
                                .ok_or(ManagementEffectStoreError::StoreFull)?;
                        }
                        Ok(false) => {
                            self.m_rename_directory(&directory, &orphan)?;
                            break;
                        }
                        Err(error) => return Err(error.into()),
                    }
                }
                self.m_sync(&generations)?;
            }
            Ok(false) => {}
            Err(error) => return Err(error.into()),
        }
        self.m_ensure_directory(&directory)?;
        let archive_bytes = serde_json::to_vec(&archive)?;
        let checkpoint_bytes = serde_json::to_vec(&CheckpointV1 {
            version: FORMAT_VERSION,
            rescue,
            pending,
        })?;
        if archive_bytes.len() as u64 > MAX_ARCHIVE_BYTES
            || checkpoint_bytes.len() as u64 > MAX_CHECKPOINT_BYTES
        {
            return Err(ManagementEffectStoreError::StoreFull);
        }
        self.m_write_new(&directory.join(ARCHIVE), &archive_bytes)?;
        #[cfg(test)]
        seal_fault(1)?;
        self.m_write_new(&directory.join(CHECKPOINT), &checkpoint_bytes)?;
        #[cfg(test)]
        seal_fault(2)?;
        let manifest = ManifestV1 {
            version: FORMAT_VERSION,
            generation: next,
            previous_generation: state.generation,
            previous_manifest_hash: state.chain_hash,
            previous_journal_hash: state.journal_hash,
            previous_sequence: state.sequence,
            cumulative_effects: state
                .archived_effects
                .checked_add(state.effects.len() as u64)
                .ok_or(ManagementEffectStoreError::StoreFull)?,
            cumulative_records: state
                .prior_records
                .checked_add(state.records as u64)
                .ok_or(ManagementEffectStoreError::StoreFull)?,
            archive_hash: ManagementEffectHashV1::digest(&archive_bytes),
            checkpoint_hash: ManagementEffectHashV1::digest(&checkpoint_bytes),
        };
        let manifest_bytes = serde_json::to_vec(&manifest)?;
        if manifest_bytes.len() as u64 > MAX_MANIFEST_BYTES {
            return Err(ManagementEffectStoreError::StoreFull);
        }
        self.m_write_new(&directory.join(MANIFEST), &manifest_bytes)?;
        #[cfg(test)]
        seal_fault(3)?;
        self.m_write_new(&directory.join(JOURNAL_NAME), b"")?;
        self.m_sync(&directory)?;
        #[cfg(test)]
        seal_fault(4)?;
        let pointer = CurrentV1 {
            version: FORMAT_VERSION,
            generation: next,
            manifest_hash: ManagementEffectHashV1::digest(&manifest_bytes),
        };
        let pointer_bytes = serde_json::to_vec(&pointer)?;
        let mut suffix = FILE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let temporary = loop {
            let candidate = self
                .root
                .join(format!(".CURRENT-{}-{suffix}", std::process::id()));
            match self.m_write_new(&candidate, &pointer_bytes) {
                Ok(()) => break candidate,
                Err(ManagementEffectStoreError::Io(error))
                    if error.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    suffix = suffix
                        .checked_add(1)
                        .ok_or(ManagementEffectStoreError::StoreFull)?;
                }
                Err(error) => return Err(error),
            }
        };
        #[cfg(test)]
        seal_fault(5)?;
        self.m_replace(&temporary, &self.root.join(CURRENT))?;
        #[cfg(test)]
        seal_fault(6)?;
        self.m_sync(&self.root)?;
        Ok(())
    }

    /// Joined M2-A recovery uses one staging name and one pointer name. The
    /// ordinary compatibility opener retains its existing seal behavior.
    #[cfg(unix)]
    pub(super) fn seal_generation_joined(
        &self,
        lease: &ProjectMutationLeaseV1,
        root: &File,
        state: &JournalStateV1,
    ) -> Result<(), ManagementEffectStoreError> {
        use crate::project_mutation::directory_names_bounded;
        self.check_lease(lease)?;
        let artifacts = joined_seal_payloads(state)?;
        let next = artifacts.next;
        let payloads = artifacts.payloads();
        let pointer_bytes = &artifacts.pointer_bytes;
        let stage_name = format!(".seal-{next}.v1.tmp");
        let pointer_name = format!(".CURRENT-{next}.v1.tmp");
        let final_name = next.to_string();
        let root_names = directory_names_bounded(root, self.limits.max_records)?;
        for name in &root_names {
            if name != OWNER_LOCK_NAME
                && name != JOURNAL_NAME
                && name != CURRENT
                && name != GENERATIONS
                && name != pointer_name.as_str()
            {
                return Err(ManagementEffectStoreError::ForeignPrefix);
            }
        }
        let pointer_current = optional_management_bytes(root, CURRENT, MAX_POINTER_BYTES)?;
        let already_published = pointer_current.as_deref() == Some(pointer_bytes.as_slice());
        if !already_published && self.load()?.cursor() != state.cursor() {
            return Err(ManagementEffectStoreError::StaleCursor);
        }
        if let Some(bytes) = optional_management_bytes(root, &pointer_name, MAX_POINTER_BYTES)? {
            if already_published || !pointer_bytes.starts_with(&bytes) {
                return Err(ManagementEffectStoreError::ForeignPrefix);
            }
        }
        let generations = optional_management_directory(root, GENERATIONS)?;
        let mut stage = None;
        let mut final_directory = None;
        if let Some(generations) = &generations {
            for name in directory_names_bounded(generations, self.limits.max_records)? {
                let valid_published = name
                    .to_str()
                    .and_then(|name| name.parse::<u64>().ok())
                    .is_some_and(|number| {
                        number > 0
                            && number <= state.generation
                            && number.to_string() == name.to_string_lossy()
                    });
                if !valid_published && name != stage_name.as_str() && name != final_name.as_str() {
                    return Err(ManagementEffectStoreError::ForeignPrefix);
                }
            }
            stage = optional_management_directory(generations, &stage_name)?;
            final_directory = optional_management_directory(generations, &final_name)?;
            if stage.is_some() && final_directory.is_some() {
                return Err(ManagementEffectStoreError::ForeignPrefix);
            }
            if let Some(stage) = &stage {
                validate_joined_payload_prefixes(stage, &payloads, false)?;
            }
            if let Some(final_directory) = &final_directory {
                validate_joined_payload_prefixes(final_directory, &payloads, true)?;
            }
        }
        if already_published {
            if stage.is_some() || final_directory.is_none() {
                return Err(ManagementEffectStoreError::ForeignPrefix);
            }
            return Ok(());
        }
        let generations = match generations {
            Some(directory) => directory,
            None => create_management_directory(root, GENERATIONS)?,
        };
        if final_directory.is_none() {
            let stage = match stage {
                Some(directory) => directory,
                None => create_management_directory(&generations, &stage_name)?,
            };
            #[cfg(test)]
            seal_fault(1)?;
            for (_index, (name, bytes, max)) in payloads.into_iter().enumerate() {
                resume_joined_exact_prefix(&stage, name, bytes, max)?;
                #[cfg(test)]
                seal_fault(_index as u8 + 2)?;
            }
            stage.sync_all()?;
            verify_management_entry(
                &generations,
                &CString::new(stage_name.as_str()).expect("fixed name"),
                &stage,
            )?;
            joined_rename_noreplace(&generations, &stage_name, &generations, &final_name)?;
            generations.sync_all()?;
        }
        let final_directory = management_child_directory(&generations, &final_name)?;
        validate_joined_payload_prefixes(&final_directory, &payloads, true)?;
        #[cfg(test)]
        seal_fault(6)?;
        resume_joined_prefix(
            root,
            &pointer_name,
            CURRENT,
            &pointer_bytes,
            MAX_POINTER_BYTES,
        )?;
        root.sync_all()?;
        #[cfg(test)]
        seal_fault(7)?;
        let temporary = pin_management_file(root, &pointer_name, &pointer_name, MAX_POINTER_BYTES)?;
        verify_management_file(&temporary)?;
        if temporary.bytes.as_slice() != pointer_bytes.as_slice() {
            return Err(ManagementEffectStoreError::ForeignPrefix);
        }
        let source = CString::new(pointer_name).expect("fixed name");
        if unsafe {
            libc::renameat(
                root.as_raw_fd(),
                source.as_ptr(),
                root.as_raw_fd(),
                c"CURRENT".as_ptr(),
            )
        } != 0
        {
            return Err(std::io::Error::last_os_error().into());
        }
        #[cfg(test)]
        seal_fault(8)?;
        root.sync_all()?;
        #[cfg(test)]
        seal_fault(9)?;
        if optional_management_bytes(root, CURRENT, MAX_POINTER_BYTES)?.as_deref()
            != Some(pointer_bytes.as_slice())
            || self.active_generation()?.number != next
        {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        Ok(())
    }

    pub(super) fn validate_inspected_seal_layout(
        &self,
        inspected: &ManagementEffectInspectV1,
    ) -> Result<(), ManagementEffectStoreError> {
        let next = inspected
            .state
            .generation
            .checked_add(1)
            .ok_or(ManagementEffectStoreError::StoreFull)?;
        let stage = format!("generations/.seal-{next}.v1.tmp");
        let target = format!("generations/{next}");
        let temporary = format!(".CURRENT-{next}.v1.tmp");
        for directory in inspected
            .directories
            .keys()
            .filter(|name| name.as_str() != GENERATIONS)
        {
            let published = directory
                .strip_prefix("generations/")
                .and_then(|name| name.parse::<u64>().ok())
                .is_some_and(|number| {
                    number > 0
                        && number <= inspected.state.generation
                        && directory == &format!("generations/{number}")
                });
            if !published && directory != &stage && directory != &target {
                return Err(ManagementEffectStoreError::ForeignPrefix);
            }
        }
        for name in inspected
            .files
            .keys()
            .filter(|name| name.starts_with(".CURRENT-"))
        {
            if name != &temporary {
                return Err(ManagementEffectStoreError::ForeignPrefix);
            }
        }
        let stage_exists = inspected.directories.contains_key(&stage);
        let target_exists = inspected.directories.contains_key(&target);
        if stage_exists && target_exists {
            return Err(ManagementEffectStoreError::ForeignPrefix);
        }
        if !stage_exists && !target_exists && !inspected.files.contains_key(&temporary) {
            return Ok(());
        }
        let artifacts = joined_seal_payloads(&inspected.state)?;
        for (directory, complete) in [(&stage, false), (&target, true)] {
            if !inspected.directories.contains_key(directory) {
                continue;
            }
            let mut unfinished = false;
            for (name, bytes, _) in artifacts.payloads() {
                match inspected.files.get(&format!("{directory}/{name}")) {
                    None if !complete => unfinished = true,
                    Some(file)
                        if !unfinished
                            && bytes.starts_with(&file.bytes)
                            && (!complete || file.bytes == bytes) =>
                    {
                        if file.bytes.len() != bytes.len() {
                            unfinished = true;
                        }
                    }
                    _ => return Err(ManagementEffectStoreError::ForeignPrefix),
                }
            }
        }
        if let Some(file) = inspected.files.get(&temporary) {
            if !target_exists || !artifacts.pointer_bytes.starts_with(&file.bytes) {
                return Err(ManagementEffectStoreError::ForeignPrefix);
            }
        }
        Ok(())
    }

    #[cfg(test)]
    #[cfg(unix)]
    pub(super) fn test_stage_joined_prefix(
        &self,
        index: usize,
        foreign: bool,
    ) -> Result<(), ManagementEffectStoreError> {
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let state = self.load()?;
        let artifacts = joined_seal_payloads(&state)?;
        let generations = self.root.join(GENERATIONS);
        fs::create_dir(&generations)?;
        fs::set_permissions(&generations, fs::Permissions::from_mode(0o700))?;
        let stage = generations.join(format!(".seal-{}.v1.tmp", artifacts.next));
        fs::create_dir(&stage)?;
        fs::set_permissions(&stage, fs::Permissions::from_mode(0o700))?;
        for (position, (name, bytes, _)) in artifacts.payloads().into_iter().enumerate() {
            if position > index {
                break;
            }
            let mut prefix = if position == index {
                bytes[..bytes.len() / 2].to_vec()
            } else {
                bytes.to_vec()
            };
            if foreign && position == index {
                prefix[0] ^= 1;
            }
            let mut file = fs::OpenOptions::new()
                .create_new(true)
                .write(true)
                .mode(0o600)
                .open(stage.join(name))?;
            file.write_all(&prefix)?;
            file.sync_all()?;
        }
        File::open(&stage)?.sync_all()?;
        File::open(&generations)?.sync_all()?;
        Ok(())
    }
}

struct JoinedSealPayloadsV1 {
    next: u64,
    archive_bytes: Vec<u8>,
    checkpoint_bytes: Vec<u8>,
    manifest_bytes: Vec<u8>,
    pointer_bytes: Vec<u8>,
}
impl JoinedSealPayloadsV1 {
    fn payloads(&self) -> [(&'static str, &[u8], u64); 4] {
        [
            (ARCHIVE, &self.archive_bytes, MAX_ARCHIVE_BYTES),
            (CHECKPOINT, &self.checkpoint_bytes, MAX_CHECKPOINT_BYTES),
            (MANIFEST, &self.manifest_bytes, MAX_MANIFEST_BYTES),
            (JOURNAL_NAME, b"", 0),
        ]
    }
}
/// Validate the next complete seal before admission budgets or filesystem writes.
pub(super) fn preflight_seal_capacity(
    state: &JournalStateV1,
) -> Result<(), ManagementEffectStoreError> {
    joined_seal_payloads(state).map(|_| ())
}

fn joined_seal_payloads(
    state: &JournalStateV1,
) -> Result<JoinedSealPayloadsV1, ManagementEffectStoreError> {
    let pending = state
        .effects
        .iter()
        .filter(|(_, effect)| !effect.state.is_terminal())
        .map(|(id, effect)| (*id, effect.clone()))
        .collect::<BTreeMap<_, _>>();
    let archive = state
        .effects
        .iter()
        .filter(|(_, effect)| effect.state.is_terminal())
        .map(|(id, effect)| (*id, effect.clone()))
        .collect::<BTreeMap<_, _>>();
    let next = state
        .generation
        .checked_add(1)
        .ok_or(ManagementEffectStoreError::StoreFull)?;
    let archive_bytes = serde_json::to_vec(&archive)?;
    let checkpoint_bytes = serde_json::to_vec(&CheckpointV1 {
        version: FORMAT_VERSION,
        rescue: !pending.is_empty(),
        pending,
    })?;
    let manifest_bytes = serde_json::to_vec(&ManifestV1 {
        version: FORMAT_VERSION,
        generation: next,
        previous_generation: state.generation,
        previous_manifest_hash: state.chain_hash,
        previous_journal_hash: state.journal_hash,
        previous_sequence: state.sequence,
        cumulative_effects: state
            .archived_effects
            .checked_add(state.effects.len() as u64)
            .ok_or(ManagementEffectStoreError::StoreFull)?,
        cumulative_records: state
            .prior_records
            .checked_add(state.records as u64)
            .ok_or(ManagementEffectStoreError::StoreFull)?,
        archive_hash: ManagementEffectHashV1::digest(&archive_bytes),
        checkpoint_hash: ManagementEffectHashV1::digest(&checkpoint_bytes),
    })?;
    let pointer_bytes = serde_json::to_vec(&CurrentV1 {
        version: FORMAT_VERSION,
        generation: next,
        manifest_hash: ManagementEffectHashV1::digest(&manifest_bytes),
    })?;
    let artifacts = JoinedSealPayloadsV1 {
        next,
        archive_bytes,
        checkpoint_bytes,
        manifest_bytes,
        pointer_bytes,
    };
    for (kind, bytes, max) in artifacts.payloads() {
        joined_payload_bound(kind, bytes, max)?;
    }
    joined_payload_bound(CURRENT, &artifacts.pointer_bytes, MAX_POINTER_BYTES)?;
    Ok(artifacts)
}

fn joined_payload_bound(
    kind: &'static str,
    expected: &[u8],
    max: u64,
) -> Result<(), ManagementEffectStoreError> {
    let actual =
        u64::try_from(expected.len()).map_err(|_| ManagementEffectStoreError::StoreFull)?;
    if actual > max {
        return Err(ManagementEffectStoreError::ExpectedPayloadTooLarge { kind, actual, max });
    }
    Ok(())
}

#[cfg(unix)]
fn optional_management_directory(
    parent: &File,
    name: &str,
) -> Result<Option<File>, ManagementEffectStoreError> {
    match management_child_directory(parent, name) {
        Err(ManagementEffectStoreError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(None)
        }
        result => result.map(Some),
    }
}

#[cfg(unix)]
fn create_management_directory(
    parent: &File,
    name: &str,
) -> Result<File, ManagementEffectStoreError> {
    let component = CString::new(name).map_err(|_| ManagementEffectStoreError::InvalidStore)?;
    if unsafe { libc::mkdirat(parent.as_raw_fd(), component.as_ptr(), 0o700) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    parent.sync_all()?;
    management_child_directory(parent, name)
}

#[cfg(unix)]
fn optional_management_bytes(
    parent: &File,
    name: &str,
    max: u64,
) -> Result<Option<Vec<u8>>, ManagementEffectStoreError> {
    match pin_management_file(parent, name, name, max) {
        Err(ManagementEffectStoreError::Io(error))
            if error.kind() == std::io::ErrorKind::NotFound =>
        {
            Ok(None)
        }
        result => result.map(|file| Some(file.bytes)),
    }
}

#[cfg(unix)]
fn validate_joined_payload_prefixes(
    directory: &File,
    expected: &[(&'static str, &[u8], u64)],
    complete: bool,
) -> Result<(), ManagementEffectStoreError> {
    let names = crate::project_mutation::directory_names_bounded(directory, 4)?;
    if names
        .iter()
        .any(|name| !expected.iter().any(|(expected, _, _)| name == *expected))
    {
        return Err(ManagementEffectStoreError::ForeignPrefix);
    }
    let mut unfinished = false;
    for (name, bytes, max) in expected {
        match optional_management_bytes(directory, name, *max)? {
            None if !complete => unfinished = true,
            Some(present)
                if !unfinished
                    && bytes.starts_with(&present)
                    && (!complete || present == *bytes) =>
            {
                if present.len() != bytes.len() {
                    unfinished = true;
                }
            }
            _ => return Err(ManagementEffectStoreError::ForeignPrefix),
        }
    }
    Ok(())
}

#[cfg(unix)]
fn resume_joined_exact_prefix(
    parent: &File,
    name: &'static str,
    expected: &[u8],
    max: u64,
) -> Result<(), ManagementEffectStoreError> {
    resume_joined_prefix(parent, name, name, expected, max)
}

#[cfg(unix)]
fn resume_joined_prefix(
    parent: &File,
    name: &str,
    kind: &'static str,
    expected: &[u8],
    max: u64,
) -> Result<(), ManagementEffectStoreError> {
    joined_payload_bound(kind, expected, max)?;
    let bound = (expected.len() as u64)
        .checked_add(1)
        .ok_or(ManagementEffectStoreError::StoreFull)?;
    let component = CString::new(name).map_err(|_| ManagementEffectStoreError::InvalidStore)?;
    let flags =
        libc::O_RDWR | libc::O_APPEND | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK;
    let mut fd = unsafe { libc::openat(parent.as_raw_fd(), component.as_ptr(), flags) };
    if fd < 0 {
        let error = std::io::Error::last_os_error();
        if error.kind() != std::io::ErrorKind::NotFound {
            return Err(error.into());
        }
        fd = unsafe {
            libc::openat(
                parent.as_raw_fd(),
                component.as_ptr(),
                flags | libc::O_CREAT | libc::O_EXCL,
                0o600,
            )
        };
        if fd < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
    }
    let mut file = unsafe { File::from_raw_fd(fd) };
    verify_management_entry(parent, &component, &file)?;
    let present = management_file_bytes(&file, bound)?;
    if !expected.starts_with(&present) {
        return Err(ManagementEffectStoreError::ForeignPrefix);
    }
    verify_management_entry(parent, &component, &file)?;
    if management_file_bytes(&file, bound)? != present {
        return Err(ManagementEffectStoreError::ForeignPrefix);
    }
    file.write_all(&expected[present.len()..])?;
    file.sync_data()?;
    if management_file_bytes(&file, bound)? != expected {
        return Err(ManagementEffectStoreError::ForeignPrefix);
    }
    verify_management_entry(parent, &component, &file)?;
    parent.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn joined_rename_noreplace(
    from: &File,
    source: &str,
    to: &File,
    target: &str,
) -> Result<(), ManagementEffectStoreError> {
    let source = CString::new(source).map_err(|_| ManagementEffectStoreError::InvalidStore)?;
    let target = CString::new(target).map_err(|_| ManagementEffectStoreError::InvalidStore)?;
    #[cfg(target_os = "linux")]
    let code = unsafe {
        libc::renameat2(
            from.as_raw_fd(),
            source.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let code = unsafe {
        libc::renameatx_np(
            from.as_raw_fd(),
            source.as_ptr(),
            to.as_raw_fd(),
            target.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if code != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(unix)]
pub(super) fn write_new(path: &Path, bytes: &[u8]) -> Result<(), ManagementEffectStoreError> {
    let mut file = OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)?;
    file.write_all(bytes)?;
    file.sync_all()?;
    Ok(())
}

#[cfg(unix)]
fn bounded(path: &Path, maximum: u64) -> Result<Vec<u8>, ManagementEffectStoreError> {
    read_bounded(path, maximum).map_err(|error| match error {
        ManagementEffectStoreError::StoreFull => ManagementEffectStoreError::CorruptJournal,
        other => other,
    })
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, ManagementEffectStoreError> {
    serde_json::from_slice(bytes).map_err(|_| ManagementEffectStoreError::CorruptJournal)
}
