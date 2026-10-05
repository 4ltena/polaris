//! Session-manager generation publication, sealing and archive recovery.

use super::*;
use crate::session_manager::{
    AdmissionId, AdmissionState, AdmissionV1, ManagedSessionV1, SessionId, SubmissionPayloadV1,
};
use std::collections::BTreeMap;

const CURRENT: &str = "CURRENT";
const GENERATIONS: &str = "generations";
const MANIFEST: &str = "manifest.json";
const ARCHIVE: &str = "archive.json";
const INDEX: &str = "admission-index.json";
const BOOTSTRAP: &str = "bootstrap.json";
const FORMAT_VERSION: u8 = 2;
const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
const MAX_INDEX_BYTES: u64 = 2 * 1024 * 1024;

#[cfg(test)]
thread_local! {
    static SEAL_FAILURE: std::cell::Cell<Option<u8>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) fn fail_seal_once_at(point: u8) {
    SEAL_FAILURE.with(|slot| slot.set(Some(point)));
}

#[cfg(test)]
fn seal_fault(point: u8) -> StoreResult<()> {
    let fail = SEAL_FAILURE.with(|slot| slot.get() == Some(point));
    if fail {
        SEAL_FAILURE.with(|slot| slot.set(None));
        return Err(StoreError::Io(std::io::Error::other(format!(
            "injected seal failure {point}"
        ))));
    }
    Ok(())
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Pointer {
    version: u8,
    generation: u64,
    manifest_hash: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    version: u8,
    generation: u64,
    previous_generation: u64,
    previous_commit_hash: String,
    previous_journal_hash: String,
    previous_journal_len: u64,
    previous_commit_count: usize,
    previous_revision: u64,
    previous_state_hash: String,
    logical_state_hash: String,
    bootstrap_hash: String,
    archive_hash: String,
    index_hash: String,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactAdmission {
    id: AdmissionId,
    revision: u64,
    payload_hash: String,
    parent_session: SessionId,
    route_reason: Option<crate::session_manager::RouteReason>,
    state: AdmissionState,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CompactState {
    version: u8,
    revision: u64,
    generation: u64,
    new_submit_closed: bool,
    sessions: BTreeMap<SessionId, ManagedSessionV1>,
    queue: Vec<AdmissionId>,
    live: BTreeMap<AdmissionId, CompactAdmission>,
}

impl SessionManagerStore {
    fn legacy_journal_bytes(&self) -> StoreResult<Vec<u8>> {
        let mut bytes = Self::read_bounded(&self.directory, JOURNAL, MAX_JOURNAL_BYTES as u64)?;
        let end = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1);
        bytes.truncate(end);
        if bytes.is_empty() {
            return Err(StoreError::Corrupt(
                "session manager legacy committed prefix",
            ));
        }
        Ok(bytes)
    }

    pub(super) fn generation_directory(&self) -> StoreResult<Directory> {
        self.directory.child(GENERATIONS, false)?.child("1", false)
    }

    fn payload_name(hash: &str) -> StoreResult<String> {
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(StoreError::Corrupt("session manager payload hash"));
        }
        Ok(format!("payload-{hash}.json"))
    }

    fn read_bounded(directory: &Directory, name: &str, maximum: u64) -> StoreResult<Vec<u8>> {
        let mut file = directory.open(name, false, false)?;
        let len = file.metadata()?.len();
        if len == 0 || len > maximum {
            return Err(StoreError::Corrupt("session manager generation file size"));
        }
        let mut bytes = Vec::with_capacity(len as usize);
        file.read_to_end(&mut bytes)?;
        if bytes.len() as u64 != len {
            return Err(StoreError::Corrupt(
                "session manager generation file length",
            ));
        }
        Ok(bytes)
    }

    pub(super) fn read_pointer(&self) -> StoreResult<Option<Pointer>> {
        let bytes = match Self::read_bounded(&self.directory, CURRENT, 4096) {
            Ok(bytes) => bytes,
            Err(StoreError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let pointer: Pointer = serde_json::from_slice(&bytes)
            .map_err(|_| StoreError::Corrupt("session manager pointer"))?;
        if pointer.version != FORMAT_VERSION || pointer.generation != 1 {
            return Err(StoreError::Corrupt("session manager pointer generation"));
        }
        Ok(Some(pointer))
    }

    fn compact(
        manager: &SessionManagerV1,
        archive: &BTreeMap<AdmissionId, AdmissionV1>,
    ) -> CompactState {
        let live = manager
            .admissions
            .iter()
            .filter(|(id, _)| !archive.contains_key(*id))
            .map(|(id, admission)| {
                (
                    id.clone(),
                    CompactAdmission {
                        id: id.clone(),
                        revision: admission.revision,
                        payload_hash: admission.payload_hash.clone(),
                        parent_session: admission.parent_session.clone(),
                        route_reason: admission.route_reason,
                        state: admission.state.clone(),
                    },
                )
            })
            .collect();
        CompactState {
            version: FORMAT_VERSION,
            revision: manager.revision,
            generation: manager.generation,
            new_submit_closed: manager.new_submit_closed,
            sessions: manager.sessions.clone(),
            queue: manager.queue.clone(),
            live,
        }
    }

    fn expand(
        &self,
        generation: &Directory,
        compact: CompactState,
        archive: &BTreeMap<AdmissionId, AdmissionV1>,
    ) -> StoreResult<SessionManagerV1> {
        if compact.version != FORMAT_VERSION
            || compact.generation != 1
            || !compact.new_submit_closed
        {
            return Err(StoreError::Corrupt("session manager compact state header"));
        }
        let mut admissions = archive.clone();
        for (id, item) in compact.live {
            if id != item.id || admissions.contains_key(&id) {
                return Err(StoreError::Corrupt("session manager compact admission id"));
            }
            let bytes = Self::read_bounded(
                generation,
                &Self::payload_name(&item.payload_hash)?,
                64 * 1024,
            )?;
            if content_hash(&bytes) != item.payload_hash {
                return Err(StoreError::Corrupt("session manager payload object hash"));
            }
            let payload: SubmissionPayloadV1 = serde_json::from_slice(&bytes)
                .map_err(|_| StoreError::Corrupt("session manager payload object"))?;
            if serde_json::to_vec(&payload)? != bytes {
                return Err(StoreError::Corrupt(
                    "session manager payload canonical form",
                ));
            }
            admissions.insert(
                id.clone(),
                AdmissionV1 {
                    id,
                    revision: item.revision,
                    payload_hash: item.payload_hash,
                    parent_session: item.parent_session,
                    payload,
                    route_reason: item.route_reason,
                    state: item.state,
                },
            );
        }
        let manager = SessionManagerV1 {
            version: 1,
            revision: compact.revision,
            generation: compact.generation,
            new_submit_closed: compact.new_submit_closed,
            sessions: compact.sessions,
            admissions,
            queue: compact.queue,
        };
        manager.validate().map_err(admission_restore_error)?;
        Ok(manager)
    }

    fn generation_evidence(
        &self,
    ) -> StoreResult<(Directory, Manifest, BTreeMap<AdmissionId, AdmissionV1>)> {
        let pointer = self
            .read_pointer()?
            .ok_or(StoreError::Corrupt("session manager pointer missing"))?;
        let old_commits = self.read_commits()?;
        self.verify_legacy_evidence(&old_commits)?;
        let old_journal = self.legacy_journal_bytes()?;
        let last = old_commits
            .last()
            .ok_or(StoreError::Corrupt("session manager legacy commit missing"))?;
        let old = self.read_state(last)?;
        let generation = self.generation_directory()?;
        let manifest_bytes = Self::read_bounded(&generation, MANIFEST, 4096)?;
        if content_hash(&manifest_bytes) != pointer.manifest_hash {
            return Err(StoreError::Corrupt("session manager manifest hash"));
        }
        let manifest: Manifest = serde_json::from_slice(&manifest_bytes)
            .map_err(|_| StoreError::Corrupt("session manager manifest"))?;
        if manifest.version != FORMAT_VERSION
            || manifest.generation != 1
            || manifest.previous_generation != 0
            || manifest.previous_revision != last.revision
            || manifest.previous_state_hash != last.content_hash
            || manifest.previous_commit_hash != content_hash(&serde_json::to_vec(last)?)
            || manifest.previous_journal_hash != content_hash(&old_journal)
            || manifest.previous_journal_len != old_journal.len() as u64
            || manifest.previous_commit_count != old_commits.len()
            || manifest.logical_state_hash != content_hash(&canonical_state(&old)?)
        {
            return Err(StoreError::Corrupt("session manager generation chain"));
        }
        let archive_bytes = Self::read_bounded(&generation, ARCHIVE, MAX_ARCHIVE_BYTES)?;
        let index_bytes = Self::read_bounded(&generation, INDEX, MAX_INDEX_BYTES)?;
        if content_hash(&archive_bytes) != manifest.archive_hash
            || content_hash(&index_bytes) != manifest.index_hash
        {
            return Err(StoreError::Corrupt("session manager archive hash"));
        }
        let archive: BTreeMap<AdmissionId, AdmissionV1> = serde_json::from_slice(&archive_bytes)
            .map_err(|_| StoreError::Corrupt("session manager archive"))?;
        let index: BTreeMap<AdmissionId, String> = serde_json::from_slice(&index_bytes)
            .map_err(|_| StoreError::Corrupt("session manager index"))?;
        if serde_json::to_vec(&archive)? != archive_bytes
            || serde_json::to_vec(&index)? != index_bytes
        {
            return Err(StoreError::Corrupt(
                "session manager archive canonical form",
            ));
        }
        let expected = old
            .admissions
            .iter()
            .filter(|(_, admission)| {
                matches!(admission.state, AdmissionState::Terminal { .. })
                    && !matches!(
                        admission.state,
                        AdmissionState::Terminal {
                            status: crate::session_manager::TerminalStatus::OutcomeUnknown,
                            ..
                        }
                    )
            })
            .map(|(id, admission)| (id.clone(), admission.clone()))
            .collect::<BTreeMap<_, _>>();
        if archive != expected
            || index.len() != archive.len()
            || index.iter().any(|(id, hash)| {
                archive.get(id).is_none_or(|admission| {
                    content_hash(&serde_json::to_vec(admission).unwrap_or_default()) != *hash
                })
            })
        {
            return Err(StoreError::Corrupt("session manager archive index"));
        }
        let bootstrap_bytes = Self::read_bounded(
            &generation,
            BOOTSTRAP,
            MAX_SESSION_MANAGER_STATE_BYTES as u64,
        )?;
        if content_hash(&bootstrap_bytes) != manifest.bootstrap_hash {
            return Err(StoreError::Corrupt("session manager bootstrap hash"));
        }
        let bootstrap: CompactState = serde_json::from_slice(&bootstrap_bytes)
            .map_err(|_| StoreError::Corrupt("session manager bootstrap"))?;
        let boot = self.expand(&generation, bootstrap, &archive)?;
        let mut expected_boot = old;
        expected_boot.generation = 1;
        expected_boot.new_submit_closed = true;
        if boot != expected_boot {
            return Err(StoreError::Corrupt("session manager bootstrap state"));
        }
        Ok((generation, manifest, archive))
    }

    pub(super) fn load_generation(&self) -> StoreResult<SessionManagerV1> {
        let (generation, manifest, archive) = self.generation_evidence()?;
        let commits =
            Self::read_commits_from(&generation, FORMAT_VERSION, Some(&manifest.bootstrap_hash))?;
        if !self.generation_verified.get() {
            for commit in &commits {
                let bytes = Self::read_bounded(
                    &generation,
                    &commit.state_name,
                    MAX_SESSION_MANAGER_STATE_BYTES as u64,
                )?;
                if content_hash(&bytes) != commit.content_hash {
                    return Err(StoreError::Corrupt(
                        "session manager generation history hash",
                    ));
                }
            }
            self.generation_verified.set(true);
        }
        let (name, expected_hash, expected_revision) = if let Some(last) = commits.last() {
            (&last.state_name[..], &last.content_hash[..], last.revision)
        } else {
            (
                BOOTSTRAP,
                &manifest.bootstrap_hash[..],
                manifest.previous_revision,
            )
        };
        let bytes = Self::read_bounded(&generation, name, MAX_SESSION_MANAGER_STATE_BYTES as u64)?;
        if content_hash(&bytes) != expected_hash {
            return Err(StoreError::Corrupt("session manager generation state hash"));
        }
        let compact: CompactState = serde_json::from_slice(&bytes)
            .map_err(|_| StoreError::Corrupt("session manager generation state"))?;
        if serde_json::to_vec(&compact)? != bytes || compact.revision != expected_revision {
            return Err(StoreError::Corrupt(
                "session manager generation state revision",
            ));
        }
        self.expand(&generation, compact, &archive)
    }

    pub(super) fn rescue_if_needed(&self) -> StoreResult<()> {
        self.verify_owner()?;
        if self.read_pointer()?.is_some() {
            self.load_generation()?;
            return Ok(());
        }
        let Some(old) = self.load_legacy()? else {
            return Ok(());
        };
        if !old
            .admissions
            .values()
            .any(|admission| !matches!(admission.state, AdmissionState::Terminal { .. }))
        {
            return Ok(());
        }
        match self.check_capacity(
            self.read_commits()?.len(),
            self.journal_len()?,
            canonical_state(&old)?.len(),
            &old,
            false,
        ) {
            Ok(()) => return Ok(()),
            Err(StoreError::Capacity(_)) => {}
            Err(error) => return Err(error),
        }
        self.seal_rescue(&old)
    }

    fn seal_rescue(&self, old: &SessionManagerV1) -> StoreResult<()> {
        let old_commits = self.read_commits()?;
        self.verify_legacy_evidence(&old_commits)?;
        let old_journal = self.legacy_journal_bytes()?;
        let last = old_commits
            .last()
            .ok_or(StoreError::Corrupt("session manager legacy commit missing"))?;
        let mut boot = old.clone();
        boot.generation = 1;
        boot.new_submit_closed = true;
        let archive = old
            .admissions
            .iter()
            .filter(|(_, admission)| {
                matches!(admission.state, AdmissionState::Terminal { .. })
                    && !matches!(
                        admission.state,
                        AdmissionState::Terminal {
                            status: crate::session_manager::TerminalStatus::OutcomeUnknown,
                            ..
                        }
                    )
            })
            .map(|(id, admission)| (id.clone(), admission.clone()))
            .collect::<BTreeMap<_, _>>();
        let compact = Self::compact(&boot, &archive);
        let bootstrap_bytes = serde_json::to_vec(&compact)?;
        self.check_capacity(0, 0, bootstrap_bytes.len(), &boot, false)?;
        let archive_bytes = serde_json::to_vec(&archive)?;
        let index = archive
            .iter()
            .map(|(id, admission)| Ok((id.clone(), content_hash(&serde_json::to_vec(admission)?))))
            .collect::<StoreResult<BTreeMap<_, _>>>()?;
        let index_bytes = serde_json::to_vec(&index)?;
        if archive_bytes.len() as u64 > MAX_ARCHIVE_BYTES
            || index_bytes.len() as u64 > MAX_INDEX_BYTES
        {
            return Err(StoreError::Capacity("rescue archive"));
        }
        let generations = self.directory.ensure_child(GENERATIONS)?;
        if generations.child("1", false).is_ok() {
            let mut suffix = TEMP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            loop {
                let orphan = format!(".orphan-1-{suffix}");
                if generations.child(&orphan, false).is_ok() {
                    suffix += 1;
                    continue;
                }
                std::fs::rename(
                    generations.path().join("1"),
                    generations.path().join(orphan),
                )?;
                generations.sync()?;
                break;
            }
        }
        let generation = generations.ensure_child("1")?;
        for admission in boot
            .admissions
            .values()
            .filter(|admission| !archive.contains_key(&admission.id))
        {
            let bytes = serde_json::to_vec(&admission.payload)?;
            if content_hash(&bytes) != admission.payload_hash {
                return Err(StoreError::Corrupt("session manager payload hash"));
            }
            write_immutable(
                &generation,
                &Self::payload_name(&admission.payload_hash)?,
                &bytes,
            )?;
        }
        #[cfg(test)]
        seal_fault(1)?;
        write_immutable(&generation, ARCHIVE, &archive_bytes)?;
        #[cfg(test)]
        seal_fault(2)?;
        write_immutable(&generation, INDEX, &index_bytes)?;
        #[cfg(test)]
        seal_fault(3)?;
        write_immutable(&generation, BOOTSTRAP, &bootstrap_bytes)?;
        #[cfg(test)]
        seal_fault(4)?;
        let manifest = Manifest {
            version: FORMAT_VERSION,
            generation: 1,
            previous_generation: 0,
            previous_commit_hash: content_hash(&serde_json::to_vec(last)?),
            previous_journal_hash: content_hash(&old_journal),
            previous_journal_len: old_journal.len() as u64,
            previous_commit_count: old_commits.len(),
            previous_revision: last.revision,
            previous_state_hash: last.content_hash.clone(),
            logical_state_hash: content_hash(&canonical_state(old)?),
            bootstrap_hash: content_hash(&bootstrap_bytes),
            archive_hash: content_hash(&archive_bytes),
            index_hash: content_hash(&index_bytes),
        };
        let manifest_bytes = serde_json::to_vec(&manifest)?;
        if manifest_bytes.len() > 4096 {
            return Err(StoreError::Capacity("rescue manifest"));
        }
        write_immutable(&generation, MANIFEST, &manifest_bytes)?;
        #[cfg(test)]
        seal_fault(5)?;
        let journal = generation.open(JOURNAL, true, true)?;
        journal.sync_all()?;
        drop(journal);
        generation.sync()?;
        generations.sync()?;
        #[cfg(test)]
        seal_fault(6)?;
        let pointer = Pointer {
            version: FORMAT_VERSION,
            generation: 1,
            manifest_hash: content_hash(&manifest_bytes),
        };
        let bytes = serde_json::to_vec(&pointer)?;
        let temporary = temporary_name();
        write_immutable(&self.directory, &temporary, &bytes)?;
        #[cfg(test)]
        seal_fault(7)?;
        std::fs::rename(
            self.directory.path().join(&temporary),
            self.directory.path().join(CURRENT),
        )?;
        #[cfg(test)]
        seal_fault(8)?;
        self.directory.sync()?;
        #[cfg(test)]
        seal_fault(9)?;
        self.load_generation()?;
        Ok(())
    }

    pub(super) fn persist_generation(
        &self,
        _current: &SessionManagerV1,
        next: &SessionManagerV1,
        new_submit: bool,
    ) -> StoreResult<()> {
        if new_submit || !next.new_submit_closed {
            return Err(StoreError::Capacity("family closed after rescue"));
        }
        let (generation, manifest, archive) = self.generation_evidence()?;
        let compact = Self::compact(next, &archive);
        let bytes = serde_json::to_vec(&compact)?;
        let commits =
            Self::read_commits_from(&generation, FORMAT_VERSION, Some(&manifest.bootstrap_hash))?;
        self.check_capacity(
            commits.len(),
            Self::journal_len_in(&generation)?,
            bytes.len(),
            next,
            false,
        )?;
        let name = state_name(next.revision)?;
        write_immutable(&generation, &name, &bytes)?;
        let commit = CommitV1 {
            schema_version: FORMAT_VERSION,
            revision: next.revision,
            state_name: name,
            content_hash: content_hash(&bytes),
            previous_commit_hash: Some(match commits.last() {
                Some(previous) => content_hash(&serde_json::to_vec(previous)?),
                None => manifest.bootstrap_hash,
            }),
        };
        let mut line = serde_json::to_vec(&commit)?;
        line.push(b'\n');
        if line.len() > MAX_GENERATION_COMMIT_RECORD_BYTES {
            return Err(StoreError::Capacity("commit record size"));
        }
        let mut journal = generation.open(JOURNAL, true, false)?;
        journal.seek(SeekFrom::End(0))?;
        journal.write_all(&line)?;
        journal.sync_all()?;
        generation.sync()
    }
}
