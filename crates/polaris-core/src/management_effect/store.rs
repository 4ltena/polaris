//! Lease-bound management-effect journal storage and validation.

use super::*;
use crate::project_mutation::{ProjectActivityLeaseV1, ProjectMutationLeaseV1};
#[cfg(any(unix, all(test, windows)))]
#[path = "generation.rs"]
pub(crate) mod generation;
#[cfg(any(unix, all(test, windows)))]
use generation::ActiveGenerationV1;
#[cfg(all(test, windows))]
#[path = "store/windows_backend.rs"]
mod windows_backend;
#[cfg(all(test, windows))]
use windows_backend::PinnedManagementFileV1;
#[cfg(all(test, windows))]
type ManagementDirectoryV1 = std::sync::Arc<polaris_auth::protection::WindowsFamilyDirectory>;
#[cfg(not(all(test, windows)))]
type ManagementDirectoryV1 = File;
#[cfg(all(test, windows))]
type ManagementJournalFileV1 = polaris_auth::protection::WindowsFamilyFile;
#[cfg(not(all(test, windows)))]
type ManagementJournalFileV1 = File;
use serde::{Deserialize, Serialize};
#[cfg(all(test, windows))]
use std::io::{Read, Seek, SeekFrom, Write};
use std::{
    collections::BTreeMap,
    ffi::CString,
    fs::{self, File},
    path::{Path, PathBuf},
};
#[cfg(unix)]
use std::{
    fs::OpenOptions,
    io::{Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    },
};

// Private capability: public project mutators continue to require EX leases.
#[cfg(any(unix, all(test, windows)))]
#[derive(Clone, Copy)]
enum JournalLeaseV1<'a> {
    Mutation(&'a ProjectMutationLeaseV1),
    Runtime(&'a ProjectActivityLeaseV1),
}

#[cfg(any(unix, all(test, windows)))]
const JOURNAL_VERSION: u32 = 1;
#[cfg(any(unix, all(test, windows)))]
const STORE_NAME: &str = "management-effects-v1";
#[cfg(any(unix, all(test, windows)))]
const JOURNAL_NAME: &str = "journal.v1.jsonl";
#[cfg(any(unix, all(test, windows)))]
const OWNER_LOCK_NAME: &str = "owner.lock";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManagementEffectLimitsV1 {
    pub max_journal_bytes: u64,
    pub max_records: usize,
    pub max_effects: usize,
    pub max_record_bytes: usize,
}

impl Default for ManagementEffectLimitsV1 {
    fn default() -> Self {
        Self {
            max_journal_bytes: 16 * 1024 * 1024,
            max_records: 100_000,
            max_effects: 20_000,
            max_record_bytes: 16 * 1024,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ManagementEffectCapacityV1 {
    pub generation: u64,
    pub current_journal_bytes: u64,
    pub archived_effects: u64,
    pub prior_records: u64,
    pub retained_archive_bytes: u64,
}

#[derive(Debug, thiserror::Error)]
pub enum ManagementEffectStoreError {
    #[error("management effect I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("management effect serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("management effect store is not private and owned")]
    InvalidStore,
    #[error("management effect journal is corrupt")]
    CorruptJournal,
    #[error("management effect journal contains a torn tail")]
    TornTail,
    #[error("management effect journal limit reached")]
    StoreFull,
    #[error("project mutation lease belongs to another project")]
    WrongProjectLease,
    #[error("management effect project scope does not match the store")]
    WrongProjectScope,
    #[error("management effect request conflicts with its existing id")]
    RequestConflict,
    #[error("management effect request used a stale journal cursor")]
    StaleCursor,
    #[error("management effect handle is stale")]
    StaleEffect,
    #[error("management effect transition is not legal")]
    InvalidTransition,
    #[error("terminal management effect state is immutable")]
    ImmutableTerminal,
    #[error("management effect outcome is unresolved")]
    UnresolvedEffect,
    #[error("management effect request contains an invalid zero id or hash")]
    InvalidValue,
    #[error("management effect inventory fields contradict the approved operation contract")]
    InvalidInventoryContract,
    #[error("management generation expected {kind} payload exceeds its bound: {actual} > {max}")]
    ExpectedPayloadTooLarge {
        kind: &'static str,
        actual: u64,
        max: u64,
    },
    #[error("management generation contains a foreign or changed prefix")]
    ForeignPrefix,
}

#[derive(Debug)]
pub struct ManagementEffectStoreV1 {
    state_dir: PathBuf,
    root: PathBuf,
    journal_path: PathBuf,
    expected_lease_root: PathBuf,
    project_scope: ManagementEffectHashV1,
    limits: ManagementEffectLimitsV1,
    #[cfg(all(test, windows))]
    windows: Option<std::sync::Arc<windows_backend::WindowsStoreV1>>,
}

#[derive(Debug)]
pub enum ManagementEffectInspectOpenV1 {
    Absent,
    Present(Box<ManagementEffectInspectV1>),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ManagementEffectInspectedFileV1 {
    pub relative_name: String,
    pub length: u64,
    pub hash: ManagementEffectHashV1,
    pub device: u64,
    pub inode: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ManagementEffectTailV1 {
    Complete,
    NeedsRepair {
        offset: u64,
        bytes: u64,
        hash: ManagementEffectHashV1,
    },
}

#[cfg(not(all(test, windows)))]
#[derive(Debug)]
struct PinnedManagementFileV1 {
    parent: File,
    name: CString,
    file: File,
    evidence: ManagementEffectInspectedFileV1,
    bytes: Vec<u8>,
    metadata: fs::Metadata,
    migration_mount: MigrationMountV2,
}

#[cfg(any(unix, all(test, windows)))]
#[derive(Default)]
struct MigrationInspectionV2 {
    enabled: bool,
    mount: MigrationMountV2,
    state_directory: Option<(File, fs::Metadata)>,
    root_metadata: Option<fs::Metadata>,
}

#[derive(Debug)]
pub struct ManagementEffectInspectV1 {
    store: ManagementEffectStoreV1,
    root_directory: ManagementDirectoryV1,
    _owner_guard: ManagementJournalFileV1,
    files: BTreeMap<String, PinnedManagementFileV1>,
    directories: BTreeMap<String, (u64, u64)>,
    state: JournalStateV1,
    snapshots: BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV1>,
    events: BTreeMap<(u64, u64), VerifiedManagementEffectEventV1>,
    tail: ManagementEffectTailV1,
    complete_prefix_hash: ManagementEffectHashV1,
    migration_inspection: bool,
    migration_mount: MigrationMountV2,
    migration_state_directory: Option<(File, fs::Metadata)>,
    migration_root_metadata: Option<fs::Metadata>,
    migration_directories: BTreeMap<String, (File, fs::Metadata)>,
    #[cfg(all(test, windows))]
    windows_directories: BTreeMap<String, ManagementDirectoryV1>,
}

/// Only the shared owner can construct a token after the full intent join.
#[derive(Debug)]
#[cfg(unix)]
pub struct JoinedManagementEffectRecoveryV1 {
    inspected: ManagementEffectInspectV1,
    operation_id: ManagementOperationIdV1,
    descriptor_hash: ManagementEffectHashV1,
    intent_sequence: u64,
    intent_record_hash: ManagementEffectHashV1,
    generic_request: PrepareManagementEffectV1,
    generic_verified_event: VerifiedManagementEffectEventV1,
    inspect_cursor: ManagementEffectCursorV1,
    complete_prefix_hash: ManagementEffectHashV1,
    tail: ManagementEffectTailV1,
}

#[cfg(unix)]
impl JoinedManagementEffectRecoveryV1 {
    pub(crate) fn requires_generation_seal(&self) -> Result<bool, ManagementEffectStoreError> {
        let inspected = &self.inspected;
        let next = inspected
            .state
            .generation
            .checked_add(1)
            .ok_or(ManagementEffectStoreError::StoreFull)?;
        Ok(inspected
            .directories
            .contains_key(&format!("generations/.seal-{next}.v1.tmp"))
            || inspected
                .directories
                .contains_key(&format!("generations/{next}"))
            || inspected
                .files
                .contains_key(&format!(".CURRENT-{next}.v1.tmp"))
            || (!inspected.store.has_pending_reserve(&inspected.state)?
                && inspected
                    .state
                    .effects
                    .values()
                    .any(|effect| !effect.state.is_terminal())))
    }
    pub(crate) fn generic_event(&self) -> &VerifiedManagementEffectEventV1 {
        &self.generic_verified_event
    }
    pub(crate) fn intent_binding(&self) -> (ManagementEffectHashV1, u64, ManagementEffectHashV1) {
        (
            self.descriptor_hash,
            self.intent_sequence,
            self.intent_record_hash,
        )
    }

    pub(crate) fn recover(
        self,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<
        (ManagementEffectStoreV1, VerifiedManagementEffectEventV1),
        ManagementEffectStoreError,
    > {
        let inspected = self.inspected;
        inspected.store.check_lease(lease)?;
        inspected.verify_pins()?;
        let snapshot = inspected
            .snapshots
            .get(&self.operation_id)
            .ok_or(ManagementEffectStoreError::RequestConflict)?;
        if snapshot.request != self.generic_request
            || inspected
                .latest_event_evidence(self.operation_id, snapshot)?
                .as_ref()
                != Some(&self.generic_verified_event)
            || inspected.cursor() != self.inspect_cursor
            || inspected.complete_prefix_hash != self.complete_prefix_hash
            || inspected.tail != self.tail
        {
            return Err(ManagementEffectStoreError::StaleEffect);
        }
        if let ManagementEffectTailV1::NeedsRepair { offset, .. } = inspected.tail {
            let relative = if inspected.state.generation == 0 {
                JOURNAL_NAME.to_owned()
            } else {
                format!("generations/{}/{JOURNAL_NAME}", inspected.state.generation)
            };
            let pinned = inspected
                .files
                .get(&relative)
                .ok_or(ManagementEffectStoreError::CorruptJournal)?;
            let fd = unsafe {
                libc::openat(
                    pinned.parent.as_raw_fd(),
                    pinned.name.as_ptr(),
                    libc::O_RDWR | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            verify_management_entry(&pinned.parent, &pinned.name, &file)?;
            let meta = file.metadata()?;
            if (meta.dev(), meta.ino()) != (pinned.evidence.device, pinned.evidence.inode)
                || management_file_bytes(&file, pinned.evidence.length)? != pinned.bytes
            {
                return Err(ManagementEffectStoreError::StaleEffect);
            }
            file.set_len(offset)?;
            file.sync_all()?;
            pinned.parent.sync_all()?;
            if ManagementEffectHashV1::digest(&management_file_bytes(&file, offset)?)
                != self.complete_prefix_hash
            {
                return Err(ManagementEffectStoreError::CorruptJournal);
            }
        }
        let mut state = inspected.store.load()?;
        if state.cursor() != self.inspect_cursor {
            return Err(ManagementEffectStoreError::StaleCursor);
        }
        let next = state
            .generation
            .checked_add(1)
            .ok_or(ManagementEffectStoreError::StoreFull)?;
        let partial_seal = inspected
            .directories
            .contains_key(&format!("generations/.seal-{next}.v1.tmp"))
            || inspected
                .directories
                .contains_key(&format!("generations/{next}"))
            || inspected
                .files
                .contains_key(&format!(".CURRENT-{next}.v1.tmp"));
        if partial_seal
            || (!inspected.store.has_pending_reserve(&state)?
                && state
                    .effects
                    .values()
                    .any(|effect| !effect.state.is_terminal()))
        {
            inspected
                .store
                .seal_generation_joined(lease, &inspected.root_directory, &state)?;
            state = inspected.store.load()?;
            if !inspected.store.has_pending_reserve(&state)? {
                return Err(ManagementEffectStoreError::StoreFull);
            }
        }
        let event = match snapshot.state {
            ManagementEffectStateV1::EffectStarted => inspected.store.append(
                lease,
                &state,
                JournalEventV1::OutcomeUnknown {
                    operation_id: self.operation_id,
                    reason: ManagementUnknownReasonV1::ProcessRestart,
                },
            )?,
            _ => self.generic_verified_event,
        };
        // Normal methods acquire this lock themselves. The returned store is
        // kept private by the owner until the matching intent is durable.
        drop(inspected._owner_guard);
        Ok((inspected.store, event))
    }
}

impl ManagementEffectInspectV1 {
    #[cfg(unix)]
    pub(crate) fn joined_terminal_store(
        self,
        intent: &crate::apply_intent::InspectedApplyIntentV1,
    ) -> Result<ManagementEffectStoreV1, crate::apply_owner::ApplyOwnerError> {
        self.verify_pins()?;
        if self.tail != ManagementEffectTailV1::Complete || !intent.latest().phase.is_terminal() {
            return Err(crate::apply_owner::ApplyOwnerError::EvidenceMismatch);
        }
        let operation = intent.descriptor().management_operation_id;
        if let Some(snapshot) = self.snapshots.get(&operation) {
            let event = self
                .latest_event_evidence(operation, snapshot)?
                .ok_or(crate::apply_owner::ApplyOwnerError::EvidenceMismatch)?;
            crate::apply_owner::ApplyOwnerV1::verify_generic_join(intent, snapshot, &event)?;
        } else if intent.latest().evidence.generic_evidence().is_some() {
            return Err(crate::apply_owner::ApplyOwnerError::EvidenceMismatch);
        }
        drop(self._owner_guard);
        Ok(self.store)
    }

    #[cfg(unix)]
    pub(crate) fn joined_completed_readback(
        self,
        intent: &crate::apply_intent::InspectedApplyIntentV1,
        readback: ManagementEffectHashV1,
    ) -> Result<
        (ManagementEffectStoreV1, VerifiedManagementEffectEventV1),
        crate::apply_owner::ApplyOwnerError,
    > {
        use crate::apply_owner::{ApplyOwnerError, ApplyOwnerV1};
        self.verify_pins()?;
        if self.tail != ManagementEffectTailV1::Complete
            || !(intent.latest().phase == crate::apply_intent::ApplyPhaseV1::ParticipationCommitted
                || intent.latest().phase == crate::apply_intent::ApplyPhaseV1::OutcomeUnknown
                    && intent.generations.iter().any(|e| {
                        matches!(
                            e.generation.evidence,
                            crate::apply_intent::ApplyEvidenceV1::ParticipationCommitted { .. }
                        )
                    }))
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let request = ApplyOwnerV1::management_request(intent.descriptor())?;
        let snapshot = self
            .snapshots
            .get(&request.operation_id)
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let event = self
            .latest_event_evidence(request.operation_id, snapshot)?
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        if snapshot.request != request
            || event.event
                != (JournalEventV1::Completed {
                    operation_id: request.operation_id,
                    evidence_hash: readback,
                })
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        if let crate::apply_intent::ApplyEvidenceV1::OutcomeUnknown {
            reason,
            generic_generation,
            generic_event_hash,
            generic_sequence,
            ..
        } = intent.latest().evidence
        {
            self.event_evidence(
                request.operation_id,
                ManagementEffectEventEvidenceV1 {
                    generation: generic_generation,
                    event_hash: generic_event_hash,
                    sequence: generic_sequence,
                },
                &JournalEventV1::OutcomeUnknown {
                    operation_id: request.operation_id,
                    reason,
                },
            )?
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        }
        let prior = intent
            .generations
            .iter()
            .rev()
            .find_map(|e| match e.generation.evidence {
                crate::apply_intent::ApplyEvidenceV1::EffectStarted {
                    generic_generation,
                    generic_event_hash,
                    generic_sequence,
                } => Some(ManagementEffectEventEvidenceV1 {
                    generation: generic_generation,
                    event_hash: generic_event_hash,
                    sequence: generic_sequence,
                }),
                _ => None,
            })
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        self.event_evidence(
            request.operation_id,
            prior,
            &JournalEventV1::EffectStarted {
                operation_id: request.operation_id,
            },
        )?
        .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        if (prior.generation, prior.sequence)
            >= (event.evidence.generation, event.evidence.sequence)
        {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        drop(self._owner_guard);
        Ok((self.store, event))
    }

    /// Admission may recycle a terminal generation, but never a pending one.
    #[cfg(unix)]
    pub(crate) fn fresh_admission_limits(
        &self,
    ) -> Result<(u64, u64, u64), ManagementEffectStoreError> {
        self.verify_pins()?;
        if self.tail != ManagementEffectTailV1::Complete
            || self
                .snapshots
                .values()
                .any(|effect| effect.state.blocks_new_work())
        {
            return Err(ManagementEffectStoreError::UnresolvedEffect);
        }
        let limits = self.store.limits;
        let (records, bytes) = if self.store.has_prepare_reserve(&self.state)? {
            (
                self.state.records as u64,
                self.files[&if self.state.generation == 0 {
                    JOURNAL_NAME.to_owned()
                } else {
                    format!("generations/{}/{JOURNAL_NAME}", self.state.generation)
                }]
                    .evidence
                    .length,
            )
        } else {
            generation::preflight_seal_capacity(&self.state)?;
            (0, 0)
        };
        Ok((
            limits.max_record_bytes as u64,
            (limits.max_records as u64).saturating_sub(records),
            limits.max_journal_bytes.saturating_sub(bytes),
        ))
    }

    /// Synthetic cumulative-counter fault; never changes durable inspection data.
    #[cfg(test)]
    #[cfg(unix)]
    pub(super) fn test_seal_capacity_counter_overflow(&mut self) {
        self.state.archived_effects = u64::MAX;
    }

    #[cfg(unix)]
    pub(crate) fn admit_fresh(
        self,
        lease: &ProjectMutationLeaseV1,
        intent: &crate::apply_intent::InspectedApplyIntentV1,
    ) -> Result<ManagementEffectStoreV1, ManagementEffectStoreError> {
        self.fresh_admission_limits()?;
        self.store.check_lease(lease)?;
        let request = crate::apply_owner::ApplyOwnerV1::management_request(intent.descriptor())
            .map_err(|_| ManagementEffectStoreError::RequestConflict)?;
        if intent.latest().phase != crate::apply_intent::ApplyPhaseV1::Prepared
            || intent.latest().sequence != 0
            || request.scope.project != self.store.project_scope
            || self.snapshots.contains_key(&request.operation_id)
        {
            return Err(ManagementEffectStoreError::RequestConflict);
        }
        let next = self
            .state
            .generation
            .checked_add(1)
            .ok_or(ManagementEffectStoreError::StoreFull)?;
        if !self.store.has_prepare_reserve(&self.state)?
            || self
                .directories
                .contains_key(&format!("generations/.seal-{next}.v1.tmp"))
            || self
                .directories
                .contains_key(&format!("generations/{next}"))
            || self.files.contains_key(&format!(".CURRENT-{next}.v1.tmp"))
        {
            self.store
                .seal_generation_joined(lease, &self.root_directory, &self.state)?;
        }
        if !self.store.has_prepare_reserve(&self.store.load()?)? {
            return Err(ManagementEffectStoreError::StoreFull);
        }
        drop(self._owner_guard);
        Ok(self.store)
    }

    #[cfg(unix)]
    pub(crate) fn join_recovery(
        self,
        intent: &crate::apply_intent::InspectedApplyIntentV1,
    ) -> Result<JoinedManagementEffectRecoveryV1, crate::apply_owner::ApplyOwnerError> {
        use crate::apply_owner::{ApplyOwnerError, ApplyOwnerV1};
        if intent.latest().phase.is_terminal() {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let operation_id = intent.descriptor().management_operation_id;
        if self.snapshots.values().any(|effect| {
            effect.request.operation_id != operation_id && effect.state.blocks_new_work()
        }) {
            return Err(ApplyOwnerError::EvidenceMismatch);
        }
        let snapshot = self
            .snapshots
            .get(&operation_id)
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        let verified = self
            .latest_event_evidence(operation_id, snapshot)?
            .ok_or(ApplyOwnerError::EvidenceMismatch)?;
        if ApplyOwnerV1::verify_generic_join(intent, snapshot, &verified).is_err() {
            use crate::apply_intent::ApplyPhaseV1;
            let request = ApplyOwnerV1::management_request(intent.descriptor())?;
            if snapshot.request != request {
                return Err(ApplyOwnerError::EvidenceMismatch);
            }
            let prior_event = match (&verified.event, intent.latest().phase) {
                (
                    JournalEventV1::OutcomeUnknown {
                        reason: ManagementUnknownReasonV1::ProcessRestart,
                        ..
                    },
                    ApplyPhaseV1::EffectStarted,
                ) => JournalEventV1::EffectStarted { operation_id },
                (
                    JournalEventV1::Denied { .. },
                    ApplyPhaseV1::Prepared | ApplyPhaseV1::RecoveryPrepared,
                ) => JournalEventV1::Prepared {
                    request: request.clone(),
                },
                _ => return Err(ApplyOwnerError::EvidenceMismatch),
            };
            let prior = if let Some((generation, event_hash, sequence)) = intent
                .generations
                .iter()
                .rev()
                .find_map(|record| record.generation.evidence.generic_evidence())
            {
                self.event_evidence(
                    operation_id,
                    ManagementEffectEventEvidenceV1 {
                        generation,
                        event_hash,
                        sequence,
                    },
                    &prior_event,
                )?
                .ok_or(ApplyOwnerError::EvidenceMismatch)?
            } else if intent.latest().phase == ApplyPhaseV1::Prepared {
                self.events
                    .values()
                    .find(|event| event.event == prior_event)
                    .cloned()
                    .ok_or(ApplyOwnerError::EvidenceMismatch)?
            } else {
                return Err(ApplyOwnerError::EvidenceMismatch);
            };
            if (prior.evidence.generation, prior.evidence.sequence)
                >= (verified.evidence.generation, verified.evidence.sequence)
            {
                return Err(ApplyOwnerError::EvidenceMismatch);
            }
        }
        let generic_request = snapshot.request.clone();
        let token = JoinedManagementEffectRecoveryV1 {
            operation_id,
            descriptor_hash: intent.descriptor().descriptor_hash()?,
            intent_sequence: intent.latest().sequence,
            intent_record_hash: intent.latest().record_hash()?,
            generic_request,
            generic_verified_event: verified,
            inspect_cursor: self.cursor(),
            complete_prefix_hash: self.complete_prefix_hash,
            tail: self.tail,
            inspected: self,
        };
        Ok(token)
    }
    pub fn cursor(&self) -> ManagementEffectCursorV1 {
        self.state.cursor()
    }
    #[cfg(unix)]
    pub fn root_identity(&self) -> Result<(u64, u64), ManagementEffectStoreError> {
        let meta = self.root_directory.metadata()?;
        Ok((meta.dev(), meta.ino()))
    }
    pub fn effects(&self) -> impl Iterator<Item = &ManagementEffectSnapshotV1> {
        self.snapshots.values()
    }
    pub fn files(&self) -> impl Iterator<Item = &ManagementEffectInspectedFileV1> {
        self.files.values().map(|file| &file.evidence)
    }
    pub fn tail(&self) -> ManagementEffectTailV1 {
        self.tail
    }
    pub fn complete_prefix_hash(&self) -> ManagementEffectHashV1 {
        self.complete_prefix_hash
    }

    #[cfg(any(unix, all(test, windows)))]
    pub fn latest_event_evidence(
        &self,
        operation_id: ManagementOperationIdV1,
        expected_snapshot: &ManagementEffectSnapshotV1,
    ) -> Result<Option<VerifiedManagementEffectEventV1>, ManagementEffectStoreError> {
        self.verify_pins()?;
        let Some(latest) = self
            .events
            .values()
            .rev()
            .find(|event| event.event.operation_id() == operation_id)
        else {
            return Ok(None);
        };
        if self.snapshots.get(&operation_id) != Some(expected_snapshot)
            || expected_snapshot.last_sequence != latest.evidence.sequence
            || latest.event != event_from_snapshot(expected_snapshot)
        {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        self.event_evidence(operation_id, latest.evidence, &latest.event)
    }

    #[cfg(any(unix, all(test, windows)))]
    pub fn event_evidence(
        &self,
        operation_id: ManagementOperationIdV1,
        expected: ManagementEffectEventEvidenceV1,
        event: &ManagementEffectEventV1,
    ) -> Result<Option<VerifiedManagementEffectEventV1>, ManagementEffectStoreError> {
        self.verify_pins()?;
        let Some(found) = self.events.get(&(expected.generation, expected.sequence)) else {
            return Ok(None);
        };
        if found.evidence != expected
            || found.event.operation_id() != operation_id
            || found.event != *event
        {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        Ok(Some(found.clone()))
    }

    #[cfg(any(unix, all(test, windows)))]
    pub(crate) fn verify_read_snapshot(&self) -> Result<(), ManagementEffectStoreError> {
        self.verify_pins()?;
        if self.tail != ManagementEffectTailV1::Complete {
            return Err(ManagementEffectStoreError::TornTail);
        }
        Ok(())
    }

    /// Read-only continuation after the coordinator's physical rename. Keep
    /// the original root, child pins and owner lock; never reacquire a reader.
    #[cfg(all(test, unix))]
    pub(crate) fn rebind_retired_for_migration(
        mut self,
        state_directory: &File,
        migration_id: ManagementEffectHashV1,
    ) -> Result<Self, ManagementEffectStoreError> {
        if !self.migration_inspection
            || self.tail != ManagementEffectTailV1::Complete
            || migration_id == ManagementEffectHashV1::zero()
            || self.store.root != self.store.state_dir.join(STORE_NAME)
        {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        let named_state = crate::project_mutation::open_directory_nofollow(&self.store.state_dir)?;
        for directory in [state_directory, &named_state] {
            self.verify_migration_state(directory)?;
        }
        let state_stamp = management_stamp(&named_state.metadata()?);
        if management_stamp(&state_directory.metadata()?) != state_stamp {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        let name = format!("{STORE_NAME}.retired-{}", migration_id.as_hex());
        let named_root = management_child_directory(state_directory, &name)?;
        self.migration_mount.verify(&named_root)?;
        self.migration_mount.verify(&self.root_directory)?;
        let expected = self
            .migration_root_metadata
            .as_ref()
            .ok_or(ManagementEffectStoreError::InvalidStore)?;
        let current = self.root_directory.metadata()?;
        let mut retired_stamp = management_stamp(&current);
        let original_stamp = management_stamp(expected);
        // Rename changes ctime. Every other original field remains binding.
        retired_stamp.8 = original_stamp.8;
        retired_stamp.9 = original_stamp.9;
        if retired_stamp != original_stamp
            || management_stamp(&named_root.metadata()?) != management_stamp(&current)
        {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        self.store.root = self.store.state_dir.join(name);
        self.store.journal_path = self.store.root.join(JOURNAL_NAME);
        self.migration_root_metadata = Some(current);
        // Reinventory closes every original child stamp/body and named entry,
        // including the held owner lock, before this new binding can escape.
        self.verify_read_snapshot()?;
        self.verify_migration_state(state_directory)?;
        let closing_state =
            crate::project_mutation::open_directory_nofollow(&self.store.state_dir)?;
        self.verify_migration_state(&closing_state)?;
        if management_stamp(&closing_state.metadata()?) != state_stamp
            || management_stamp(&state_directory.metadata()?) != state_stamp
        {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        Ok(self)
    }

    #[cfg(unix)]
    fn verify_migration_state(&self, named: &File) -> Result<(), ManagementEffectStoreError> {
        let (held, expected) = self
            .migration_state_directory
            .as_ref()
            .ok_or(ManagementEffectStoreError::InvalidStore)?;
        // State membership changes during cutover; its original physical
        // identity and owner/mode must remain unchanged throughout.
        let identity = |meta: &fs::Metadata| (meta.dev(), meta.ino(), meta.uid(), meta.mode());
        for file in [held, named] {
            self.migration_mount.verify(file)?;
            if identity(&file.metadata()?) != identity(expected) {
                return Err(ManagementEffectStoreError::InvalidStore);
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    fn verify_pins(&self) -> Result<(), ManagementEffectStoreError> {
        if self.migration_inspection {
            self.verify_migration_state(&crate::project_mutation::open_directory_nofollow(
                &self.store.state_dir,
            )?)?;
        }
        self.migration_mount
            .verify(&crate::project_mutation::open_directory_nofollow(
                &self.store.state_dir,
            )?)?;
        let named_root = crate::project_mutation::open_directory_nofollow(&self.store.root)?;
        self.migration_mount.verify(&named_root)?;
        self.migration_mount.verify(&self.root_directory)?;
        let root = named_root.metadata()?;
        let pinned = self.root_directory.metadata()?;
        if let Some(expected) = &self.migration_root_metadata
            && (management_stamp(&root) != management_stamp(expected)
                || management_stamp(&pinned) != management_stamp(expected))
        {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        for (file, expected) in self.migration_directories.values() {
            self.migration_mount.verify(file)?;
            if management_stamp(&file.metadata()?) != management_stamp(expected) {
                return Err(ManagementEffectStoreError::InvalidStore);
            }
        }
        if root.uid() != unsafe { libc::geteuid() } || root.mode() & 0o7777 != 0o700 {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        if (root.dev(), root.ino()) != (pinned.dev(), pinned.ino()) {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        for file in self.files.values() {
            verify_management_file(file)?;
        }
        #[cfg(test)]
        if self.migration_inspection {
            management_migration_hook(&BEFORE_MIGRATION_REFRESH);
        }
        let fresh = management_inventory_mode(
            &self.root_directory,
            self.store.limits,
            self.migration_inspection,
        )?;
        if fresh.directories != self.directories
            || fresh.migration_directories.iter().any(|(name, (_, meta))| {
                self.migration_directories
                    .get(name)
                    .is_none_or(|(_, old)| management_stamp(meta) != management_stamp(old))
            })
            || fresh.files.len() != self.files.len()
            || fresh.files.iter().any(|(name, file)| {
                self.files
                    .get(name)
                    .is_none_or(|old| old.evidence != file.evidence)
            })
        {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        if self.migration_inspection {
            // Fresh inventory must not leave earlier pins stale, even when
            // body bytes or directory names have been restored by a race.
            let named = crate::project_mutation::open_directory_nofollow(&self.store.root)?;
            for file in [&named, &self.root_directory] {
                self.migration_mount.verify(file)?;
                let current = file.metadata()?;
                if self
                    .migration_root_metadata
                    .as_ref()
                    .is_none_or(|old| management_stamp(&current) != management_stamp(old))
                {
                    return Err(ManagementEffectStoreError::InvalidStore);
                }
            }
            for (file, old) in self.migration_directories.values() {
                self.migration_mount.verify(file)?;
                if management_stamp(&file.metadata()?) != management_stamp(old) {
                    return Err(ManagementEffectStoreError::InvalidStore);
                }
            }
            for pin in self.files.values() {
                pin.migration_mount.verify(&pin.file)?;
                verify_management_entry(&pin.parent, &pin.name, &pin.file)?;
                if management_stamp(&pin.file.metadata()?) != management_stamp(&pin.metadata) {
                    return Err(ManagementEffectStoreError::InvalidStore);
                }
            }
            self.verify_migration_state(&crate::project_mutation::open_directory_nofollow(
                &self.store.state_dir,
            )?)?;
        }
        Ok(())
    }
}

impl ManagementEffectStoreV1 {
    /// Creation is authorized only by the owner's already durable generation zero.
    /// Existing bytes go through inspect/admit; normal open's repairs are avoided.
    #[cfg(unix)]
    pub(crate) fn initialize_fresh_joined(
        project: &Path,
        state: &Path,
        lease: &ProjectMutationLeaseV1,
        intent: &crate::apply_intent::InspectedApplyIntentV1,
    ) -> Result<Self, ManagementEffectStoreError> {
        use crate::project_mutation::open_directory_nofollow;
        let request = crate::apply_owner::ApplyOwnerV1::management_request(intent.descriptor())
            .map_err(|_| ManagementEffectStoreError::RequestConflict)?;
        if intent.latest().sequence != 0
            || intent.latest().phase != crate::apply_intent::ApplyPhaseV1::Prepared
            || request.scope.project
                != ManagementEffectHashV1::digest(project.as_os_str().as_encoded_bytes())
            || lease.store_root() != state.join("rewind-workspace")
        {
            return Err(ManagementEffectStoreError::WrongProjectLease);
        }
        let parent = open_directory_nofollow(state)?;
        let metadata = parent.metadata()?;
        if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o7777 != 0o700 {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        let name = CString::new(STORE_NAME).expect("fixed component");
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } != 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        let root = management_child_directory(&parent, STORE_NAME)?;
        for name in [OWNER_LOCK_NAME, JOURNAL_NAME] {
            let name = CString::new(name).expect("fixed component");
            let fd = unsafe {
                libc::openat(
                    root.as_raw_fd(),
                    name.as_ptr(),
                    libc::O_RDWR
                        | libc::O_CREAT
                        | libc::O_EXCL
                        | libc::O_NOFOLLOW
                        | libc::O_CLOEXEC,
                    0o600,
                )
            };
            if fd < 0 {
                return Err(std::io::Error::last_os_error().into());
            }
            let file = unsafe { File::from_raw_fd(fd) };
            file.sync_all()?;
        }
        root.sync_all()?;
        parent.sync_all()?;
        match Self::inspect_open_at_state_dir(project, state, lease)? {
            ManagementEffectInspectOpenV1::Present(inspected) => {
                inspected.admit_fresh(lease, intent)
            }
            ManagementEffectInspectOpenV1::Absent => Err(ManagementEffectStoreError::InvalidStore),
        }
    }

    #[cfg(unix)]
    pub fn inspect_open_at_state_dir(
        project_root: &Path,
        state_dir: &Path,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<ManagementEffectInspectOpenV1, ManagementEffectStoreError> {
        let canonical_project = canonical_project_root(project_root)?;
        let project_scope =
            ManagementEffectHashV1::digest(canonical_project.as_os_str().as_encoded_bytes());
        Self::inspect_at(
            state_dir,
            project_scope,
            lease,
            ManagementEffectLimitsV1::default(),
        )
    }

    #[cfg(unix)]
    pub(crate) fn inspect_open_with_read_lease(
        project: &Path,
        state: &Path,
        lease: crate::project_mutation::ProjectReadLeaseV1<'_>,
    ) -> Result<ManagementEffectInspectOpenV1, ManagementEffectStoreError> {
        lease
            .verify(state)
            .map_err(|_| ManagementEffectStoreError::WrongProjectLease)?;
        let project = canonical_project_root(project)?;
        let scope = ManagementEffectHashV1::digest(project.as_os_str().as_encoded_bytes());
        Self::inspect_named_at_lease_identity(
            state,
            scope,
            (lease.store_root(), lease.store_identity()),
            ManagementEffectLimitsV1::default(),
            false,
            STORE_NAME,
        )
    }

    /// Read-only inventory of the exact retired root. The cutover owner
    /// supplies its durable migration ID and holds the project lease.
    /// This never opens or repairs the old tombstone path.
    #[cfg(unix)]
    pub fn inspect_retired_for_migration_at_state_dir(
        project_root: &Path,
        state_dir: &Path,
        migration_id: ManagementEffectHashV1,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<ManagementEffectInspectOpenV1, ManagementEffectStoreError> {
        if migration_id == ManagementEffectHashV1::zero() {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        let project = canonical_project_root(project_root)?;
        let scope = ManagementEffectHashV1::digest(project.as_os_str().as_encoded_bytes());
        let name = format!("{STORE_NAME}.retired-{}", migration_id.as_hex());
        Self::inspect_named_at(
            state_dir,
            scope,
            lease,
            ManagementEffectLimitsV1::default(),
            true,
            &name,
        )
    }

    #[cfg(unix)]
    fn inspect_at(
        state_dir: &Path,
        project_scope: ManagementEffectHashV1,
        lease: &ProjectMutationLeaseV1,
        limits: ManagementEffectLimitsV1,
    ) -> Result<ManagementEffectInspectOpenV1, ManagementEffectStoreError> {
        Self::inspect_at_mode(state_dir, project_scope, lease, limits, false)
    }

    #[cfg(unix)]
    fn inspect_at_mode(
        state_dir: &Path,
        project_scope: ManagementEffectHashV1,
        lease: &ProjectMutationLeaseV1,
        limits: ManagementEffectLimitsV1,
        migration_inspection: bool,
    ) -> Result<ManagementEffectInspectOpenV1, ManagementEffectStoreError> {
        Self::inspect_named_at(
            state_dir,
            project_scope,
            lease,
            limits,
            migration_inspection,
            STORE_NAME,
        )
    }

    #[cfg(unix)]
    fn inspect_named_at(
        state_dir: &Path,
        project_scope: ManagementEffectHashV1,
        lease: &ProjectMutationLeaseV1,
        limits: ManagementEffectLimitsV1,
        migration_inspection: bool,
        root_name: &str,
    ) -> Result<ManagementEffectInspectOpenV1, ManagementEffectStoreError> {
        Self::inspect_named_at_lease_identity(
            state_dir,
            project_scope,
            (lease.store_root(), lease.store_identity()),
            limits,
            migration_inspection,
            root_name,
        )
    }

    #[cfg(unix)]
    fn inspect_named_at_lease_identity(
        state_dir: &Path,
        project_scope: ManagementEffectHashV1,
        lease_binding: (&Path, (u64, u64)),
        limits: ManagementEffectLimitsV1,
        migration_inspection: bool,
        root_name: &str,
    ) -> Result<ManagementEffectInspectOpenV1, ManagementEffectStoreError> {
        Self::inspect_named_scoped_at_lease_identity(
            state_dir,
            Some(project_scope),
            lease_binding,
            limits,
            migration_inspection,
            root_name,
        )
    }
    /// Existing-only historical reader; its optional input is not an authority.
    /// An empty source without a namespace-derived scope fails closed.
    #[cfg(all(test, unix))]
    pub(crate) fn inspect_migration_history(
        state_dir: &Path,
        lease: &ProjectMutationLeaseV1,
        scope: Option<ManagementEffectHashV1>,
    ) -> Result<ManagementEffectInspectOpenV1, ManagementEffectStoreError> {
        Self::inspect_named_scoped_at_lease_identity(
            state_dir,
            scope,
            (lease.store_root(), lease.store_identity()),
            ManagementEffectLimitsV1::default(),
            true,
            STORE_NAME,
        )
    }
    #[cfg(unix)]
    fn inspect_named_scoped_at_lease_identity(
        state_dir: &Path,
        project_scope: Option<ManagementEffectHashV1>,
        lease_binding: (&Path, (u64, u64)),
        limits: ManagementEffectLimitsV1,
        migration_inspection: bool,
        root_name: &str,
    ) -> Result<ManagementEffectInspectOpenV1, ManagementEffectStoreError> {
        if project_scope.is_none() && !migration_inspection {
            return Err(ManagementEffectStoreError::WrongProjectScope);
        }
        let state_directory = crate::project_mutation::open_directory_nofollow(state_dir)?;
        let migration_mount = MigrationMountV2::inspect(&state_directory, migration_inspection)?;
        let state_meta = state_directory.metadata()?;
        if state_meta.uid() != unsafe { libc::geteuid() } || state_meta.mode() & 0o7077 != 0 {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        let expected_lease_root = state_dir.join("rewind-workspace");
        if lease_binding.0 != expected_lease_root {
            return Err(ManagementEffectStoreError::WrongProjectLease);
        }
        let lock_directory =
            crate::project_mutation::open_directory_nofollow(&expected_lease_root)?;
        migration_mount.verify(&lock_directory)?;
        let lock_root = lock_directory.metadata()?;
        if lease_binding.1 != (lock_root.dev(), lock_root.ino()) {
            return Err(ManagementEffectStoreError::WrongProjectLease);
        }
        let root_directory = match management_child_directory(&state_directory, root_name) {
            Err(ManagementEffectStoreError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(ManagementEffectInspectOpenV1::Absent);
            }
            other => other?,
        };
        migration_mount.verify(&root_directory)?;
        let migration_root_metadata = migration_inspection
            .then(|| root_directory.metadata())
            .transpose()?;
        let store = Self {
            state_dir: state_dir.to_owned(),
            root: state_dir.join(root_name),
            journal_path: state_dir.join(root_name).join(JOURNAL_NAME),
            expected_lease_root,
            project_scope: project_scope.unwrap_or_default(),
            limits,
        };
        // The existing lock is opened without O_CREAT and remains held until
        // inspection is discarded or an explicitly joined recovery finishes.
        let owner = pin_management_file_mounted(
            &root_directory,
            OWNER_LOCK_NAME,
            OWNER_LOCK_NAME,
            4096,
            &migration_mount,
        )?;
        if migration_inspection {
            owner.file.try_lock().map_err(|error| match error {
                std::fs::TryLockError::WouldBlock => {
                    std::io::Error::from(std::io::ErrorKind::WouldBlock)
                }
                std::fs::TryLockError::Error(error) => error,
            })?;
        } else {
            owner.file.lock()?;
        }
        let inventory = management_inventory_mode(&root_directory, limits, migration_inspection)?;
        Self::inspect_inventory(
            store,
            project_scope,
            root_directory,
            owner,
            inventory,
            MigrationInspectionV2 {
                enabled: migration_inspection,
                mount: migration_mount,
                state_directory: migration_inspection.then_some((state_directory, state_meta)),
                root_metadata: migration_root_metadata,
            },
        )
    }
    #[cfg(any(unix, all(test, windows)))]
    fn inspect_inventory(
        mut store: Self,
        project_scope: Option<ManagementEffectHashV1>,
        root_directory: ManagementDirectoryV1,
        owner: PinnedManagementFileV1,
        inventory: ManagementInventoryV1,
        migration: MigrationInspectionV2,
    ) -> Result<ManagementEffectInspectOpenV1, ManagementEffectStoreError> {
        let MigrationInspectionV2 {
            enabled: migration_inspection,
            mount: migration_mount,
            state_directory: migration_state_directory,
            root_metadata: migration_root_metadata,
        } = migration;
        let files = inventory.files;
        let read =
            |relative: &str, max: u64| -> Result<Option<Vec<u8>>, ManagementEffectStoreError> {
                match files.get(relative) {
                    Some(file) if file.bytes.len() as u64 <= max => Ok(Some(file.bytes.clone())),
                    Some(_) => Err(ManagementEffectStoreError::StoreFull),
                    None => Ok(None),
                }
            };
        let chain = store.verified_generations_reading_scoped(read, project_scope)?;
        let mut scopes = std::collections::BTreeSet::new();
        let mut prepared_operations = std::collections::BTreeSet::new();
        let mut snapshots = BTreeMap::new();
        let mut events = BTreeMap::new();
        let mut latest_state = None;
        let mut tail = ManagementEffectTailV1::Complete;
        let mut complete_prefix_hash = ManagementEffectHashV1::zero();
        for active in &chain {
            let relative = if active.number == 0 {
                JOURNAL_NAME.to_owned()
            } else {
                format!("generations/{}/{JOURNAL_NAME}", active.number)
            };
            let file = files
                .get(&relative)
                .ok_or(ManagementEffectStoreError::CorruptJournal)?;
            let prefix = complete_prefix_len(&file.bytes);
            let is_current = active.number == chain.last().expect("nonempty chain").number;
            if prefix != file.bytes.len() && !is_current {
                return Err(ManagementEffectStoreError::TornTail);
            }
            let state =
                store.parse_journal_bytes_scoped(active, &file.bytes[..prefix], project_scope)?;
            for effect in state.effects.values() {
                scopes.insert(effect.request.scope.project);
            }
            for (key, effect) in &state.effects {
                snapshots.insert(*key, effect.clone());
            }
            for verified in state.events.values() {
                // Check new preparations, not carried pending snapshots: a
                // checkpoint legitimately repeats its original operation.
                if let ManagementEffectEventV1::Prepared { request } = &verified.event
                    && !prepared_operations.insert(request.operation_id)
                {
                    return Err(ManagementEffectStoreError::CorruptJournal);
                }
                if events
                    .insert(
                        (verified.evidence.generation, verified.evidence.sequence),
                        verified.clone(),
                    )
                    .is_some()
                {
                    return Err(ManagementEffectStoreError::CorruptJournal);
                }
            }
            if is_current {
                complete_prefix_hash = ManagementEffectHashV1::digest(&file.bytes[..prefix]);
                if prefix != file.bytes.len() {
                    tail = ManagementEffectTailV1::NeedsRepair {
                        offset: prefix as u64,
                        bytes: (file.bytes.len() - prefix) as u64,
                        hash: ManagementEffectHashV1::digest(&file.bytes[prefix..]),
                    };
                }
                latest_state = Some(state);
            }
        }
        if migration_inspection {
            if scopes.len() > 1 {
                return Err(ManagementEffectStoreError::WrongProjectScope);
            }
            let historical = project_scope
                .or_else(|| scopes.first().copied())
                .ok_or(ManagementEffectStoreError::WrongProjectScope)?;
            if historical == ManagementEffectHashV1::zero()
                || scopes.iter().any(|scope| *scope != historical)
            {
                return Err(ManagementEffectStoreError::WrongProjectScope);
            }
            store.project_scope = historical;
            // Re-run the ordinary bound chain validator on exactly the held
            // bytes. Orphan inventory never selects the historical scope.
            store.verified_generations_reading(|relative, max| match files.get(relative) {
                Some(file) if file.bytes.len() as u64 <= max => Ok(Some(file.bytes.clone())),
                Some(_) => Err(ManagementEffectStoreError::StoreFull),
                None => Ok(None),
            })?;
            for effect in snapshots.values() {
                validate_request_shape(historical, &effect.request)?;
            }
        }
        let inspected = ManagementEffectInspectV1 {
            store,
            root_directory,
            _owner_guard: owner.file,
            files,
            directories: inventory.directories,
            state: latest_state.ok_or(ManagementEffectStoreError::CorruptJournal)?,
            snapshots,
            events,
            tail,
            complete_prefix_hash,
            migration_inspection,
            migration_mount,
            migration_state_directory,
            migration_root_metadata,
            migration_directories: inventory.migration_directories,
            #[cfg(all(test, windows))]
            windows_directories: inventory.windows_directories,
        };
        inspected.verify_pins()?;
        if !migration_inspection {
            inspected.store.validate_inspected_seal_layout(&inspected)?;
        }
        for snapshot in inspected.snapshots.values() {
            inspected
                .latest_event_evidence(snapshot.request.operation_id, snapshot)?
                .ok_or(ManagementEffectStoreError::CorruptJournal)?;
        }
        Ok(ManagementEffectInspectOpenV1::Present(Box::new(inspected)))
    }

    /// Opens the project store while holding the one shared mutation lease.
    ///
    /// Opening repairs only an incomplete final write. A complete invalid
    /// record is corruption. Any effect that had started before this process
    /// opened the store is durably advanced to `OutcomeUnknown`.
    #[cfg(unix)]
    pub fn open(
        project_root: &Path,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<Self, ManagementEffectStoreError> {
        let canonical_project = canonical_project_root(project_root)?;
        let state_dir = crate::project::state_dir(&canonical_project)?;
        Self::open_at_state_dir(&canonical_project, &state_dir, lease)
    }

    /// Trusted-host entry point for a state directory already selected by
    /// the CLI/TUI host. The project identity is still derived independently,
    /// and the supplied lease must be the lock inside this exact state root.
    #[cfg(unix)]
    pub fn open_at_state_dir(
        project_root: &Path,
        state_dir: &Path,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<Self, ManagementEffectStoreError> {
        let canonical_project = canonical_project_root(project_root)?;
        let state_dir = fs::canonicalize(state_dir)?;
        let expected_lease_root = fs::canonicalize(state_dir.join("rewind-workspace"))?;
        let project_scope =
            ManagementEffectHashV1::digest(canonical_project.as_os_str().as_encoded_bytes());
        Self::open_at(
            &state_dir,
            expected_lease_root,
            project_scope,
            lease,
            ManagementEffectLimitsV1::default(),
        )
    }

    #[cfg(unix)]
    fn open_at(
        state_dir: &Path,
        expected_lease_root: PathBuf,
        project_scope: ManagementEffectHashV1,
        lease: &ProjectMutationLeaseV1,
        limits: ManagementEffectLimitsV1,
    ) -> Result<Self, ManagementEffectStoreError> {
        if lease.store_root() != expected_lease_root {
            return Err(ManagementEffectStoreError::WrongProjectLease);
        }
        validate_limits(limits)?;
        let state_dir = fs::canonicalize(state_dir)?;
        let root = state_dir.join(STORE_NAME);
        ensure_private_directory(&root)?;
        let root = fs::canonicalize(root)?;
        let journal_path = root.join(JOURNAL_NAME);
        match fs::symlink_metadata(root.join("CURRENT")) {
            Ok(_) => validate_private_file(&journal_path)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                ensure_private_file(&journal_path)?;
            }
            Err(error) => return Err(error.into()),
        }
        ensure_private_file(&root.join(OWNER_LOCK_NAME))?;
        let store = Self {
            state_dir,
            root,
            journal_path,
            expected_lease_root,
            project_scope,
            limits,
        };
        store.check_lease(lease)?;
        let _owner = store.lock_owner()?;
        store.repair_torn_tail(lease)?;
        let state = store.load()?;
        if !state
            .effects
            .values()
            .all(|effect| effect.state.is_terminal())
            && !store.has_pending_reserve(&state)?
        {
            store.seal_generation(lease, &state, true)?;
        }
        store.recover_interrupted(lease)?;
        Ok(store)
    }

    pub fn project_scope_hash(&self) -> ManagementEffectHashV1 {
        self.project_scope
    }

    /// Canonical trusted state root bound to this journal and its lease.
    pub fn state_dir(&self) -> &Path {
        &self.state_dir
    }

    #[cfg(any(unix, all(test, windows)))]
    pub fn capacity(&self) -> Result<ManagementEffectCapacityV1, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        let state = self.load()?;
        let current_journal_bytes = self.m_length(self.journal_for_generation(state.generation))?;
        let mut retained_archive_bytes = 0_u64;
        for number in 1..=state.generation {
            let archive = self
                .root
                .join("generations")
                .join(number.to_string())
                .join("archive.v1.json");
            retained_archive_bytes = retained_archive_bytes
                .checked_add(self.m_length(archive)?)
                .ok_or(ManagementEffectStoreError::StoreFull)?;
        }
        Ok(ManagementEffectCapacityV1 {
            generation: state.generation,
            current_journal_bytes,
            archived_effects: state.archived_effects,
            prior_records: state.prior_records,
            retained_archive_bytes,
        })
    }

    #[cfg(test)]
    #[cfg(any(unix, all(test, windows)))]
    pub(crate) fn test_fail_seal_once_at(&self, point: u8) {
        generation::fail_seal_once_at(point);
    }

    #[cfg(test)]
    #[cfg(any(unix, all(test, windows)))]
    pub(crate) fn test_reset_file_sequence_for_restart(&self) {
        generation::reset_file_sequence_for_restart();
    }

    #[cfg(any(unix, all(test, windows)))]
    pub fn cursor(&self) -> Result<ManagementEffectCursorV1, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        let state = self.load()?;
        Ok(state.cursor())
    }

    #[cfg(any(unix, all(test, windows)))]
    pub fn effects(&self) -> Result<Vec<ManagementEffectSnapshotV1>, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        Ok(self.load()?.effects.into_values().collect())
    }

    /// Enumerates one bounded page in a sealed archive (1..=generation), or
    /// in the active generation (generation + 1). The caller must retain the
    /// cursor and pass the last returned operation id as `after` for the next
    /// page. A rollover or append invalidates the cursor.
    #[cfg(any(unix, all(test, windows)))]
    pub fn effects_page(
        &self,
        expected: ManagementEffectCursorV1,
        archive_or_active: u64,
        after: Option<ManagementOperationIdV1>,
        limit: usize,
    ) -> Result<Vec<ManagementEffectSnapshotV1>, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        if limit == 0 || limit > 256 {
            return Err(ManagementEffectStoreError::InvalidValue);
        }
        let state = self.load()?;
        ensure_cursor(&state, expected)?;
        let effects = if archive_or_active >= 1 && archive_or_active <= state.generation {
            self.archive_for_page(archive_or_active)?
        } else if Some(archive_or_active) == state.generation.checked_add(1) {
            state.effects
        } else {
            return Err(ManagementEffectStoreError::InvalidValue);
        };
        Ok(effects
            .into_iter()
            .filter(|(id, _)| after.is_none_or(|previous| *id > previous))
            .take(limit)
            .map(|(_, effect)| effect)
            .collect())
    }

    #[cfg(any(unix, all(test, windows)))]
    pub fn effect(
        &self,
        operation_id: ManagementOperationIdV1,
    ) -> Result<Option<ManagementEffectSnapshotV1>, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        if let Some(effect) = self.load()?.effects.remove(&operation_id) {
            return Ok(Some(effect));
        }
        self.archived_effect(operation_id)
    }

    #[cfg(any(unix, all(test, windows)))]
    pub fn event_evidence(
        &self,
        operation_id: ManagementOperationIdV1,
        expected_evidence: ManagementEffectEventEvidenceV1,
        expected_event: &ManagementEffectEventV1,
    ) -> Result<Option<VerifiedManagementEffectEventV1>, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        self.event_evidence_unlocked(operation_id, expected_evidence, expected_event)
    }

    #[cfg(any(unix, all(test, windows)))]
    fn event_evidence_unlocked(
        &self,
        operation_id: ManagementOperationIdV1,
        expected_evidence: ManagementEffectEventEvidenceV1,
        expected_event: &ManagementEffectEventV1,
    ) -> Result<Option<VerifiedManagementEffectEventV1>, ManagementEffectStoreError> {
        let chain = self.verified_generations()?;
        let Some(active) = chain
            .iter()
            .find(|active| active.number == expected_evidence.generation)
        else {
            return Ok(None);
        };
        let state = self.load_at(active)?;
        let Some(verified) = state.events.get(&expected_evidence.sequence) else {
            return Ok(None);
        };
        if verified.evidence != expected_evidence
            || verified.event.operation_id() != operation_id
            || verified.event != *expected_event
        {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        Ok(Some(verified.clone()))
    }

    #[cfg(any(unix, all(test, windows)))]
    pub fn latest_event_evidence(
        &self,
        operation_id: ManagementOperationIdV1,
        expected_snapshot: &ManagementEffectSnapshotV1,
    ) -> Result<Option<VerifiedManagementEffectEventV1>, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        self.latest_event_evidence_unlocked(operation_id, expected_snapshot)
    }

    #[cfg(any(unix, all(test, windows)))]
    fn latest_event_evidence_unlocked(
        &self,
        operation_id: ManagementOperationIdV1,
        expected_snapshot: &ManagementEffectSnapshotV1,
    ) -> Result<Option<VerifiedManagementEffectEventV1>, ManagementEffectStoreError> {
        let chain = self.verified_generations()?;
        let mut latest = None;
        let mut snapshot = None;
        for active in &chain {
            let state = self.load_at(active)?;
            for verified in state.events.values() {
                if verified.event.operation_id() == operation_id {
                    latest = Some(verified.clone());
                }
            }
            if let Some(effect) = state.effects.get(&operation_id) {
                snapshot = Some(effect.clone());
            }
        }
        let Some(latest) = latest else {
            return Ok(None);
        };
        if snapshot.as_ref() != Some(expected_snapshot)
            || expected_snapshot.request.operation_id != operation_id
            || expected_snapshot.last_sequence != latest.evidence.sequence
            || latest.event != event_from_snapshot(expected_snapshot)
        {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        let targeted =
            self.event_evidence_unlocked(operation_id, latest.evidence, &latest.event)?;
        if targeted.as_ref() != Some(&latest) {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        Ok(targeted)
    }

    #[cfg(any(unix, all(test, windows)))]
    fn latest_event_for_operation_unlocked(
        &self,
        operation_id: ManagementOperationIdV1,
    ) -> Result<VerifiedManagementEffectEventV1, ManagementEffectStoreError> {
        let snapshot = self
            .load()?
            .effects
            .remove(&operation_id)
            .or(self.archived_effect(operation_id)?)
            .ok_or(ManagementEffectStoreError::CorruptJournal)?;
        self.latest_event_evidence_unlocked(operation_id, &snapshot)?
            .ok_or(ManagementEffectStoreError::CorruptJournal)
    }

    /// Returns the oldest non-terminal effect. Callers must refuse new turns,
    /// retries and management operations while this returns a value.
    #[cfg(any(unix, all(test, windows)))]
    pub fn blocker(
        &self,
    ) -> Result<Option<ManagementEffectSnapshotV1>, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        Ok(self
            .load()?
            .effects
            .into_values()
            .filter(|effect| effect.state.blocks_new_work())
            .min_by_key(|effect| effect.last_sequence))
    }

    /// Re-acquires the capability needed to explicitly deny a preparation
    /// that survived a process restart. Other states return `None`.
    #[cfg(any(unix, all(test, windows)))]
    pub fn prepared_effect(
        &self,
        operation_id: ManagementOperationIdV1,
    ) -> Result<Option<PreparedManagementEffectV1>, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        let state = self.load()?;
        let Some(effect) = state.effects.get(&operation_id).cloned() else {
            return Ok(None);
        };
        Ok(
            matches!(effect.state, ManagementEffectStateV1::Prepared).then_some(
                PreparedManagementEffectV1 {
                    verified_event: Box::new(
                        self.latest_event_for_operation_unlocked(effect.request.operation_id)?,
                    ),
                    request: effect.request,
                    generation: state.generation,
                    chain_hash: state.chain_hash,
                    last_sequence: effect.last_sequence,
                },
            ),
        )
    }

    /// Re-acquires only the reconciliation capability for an unresolved
    /// outcome. It cannot be used to restart the original side effect.
    #[cfg(any(unix, all(test, windows)))]
    pub fn unknown_effect(
        &self,
        operation_id: ManagementOperationIdV1,
    ) -> Result<Option<UnknownManagementEffectV1>, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        let state = self.load()?;
        let Some(effect) = state.effects.get(&operation_id).cloned() else {
            return Ok(None);
        };
        Ok(
            matches!(effect.state, ManagementEffectStateV1::OutcomeUnknown { .. }).then_some(
                UnknownManagementEffectV1 {
                    verified_event: self
                        .latest_event_for_operation_unlocked(effect.request.operation_id)?,
                    request: effect.request,
                    generation: state.generation,
                    chain_hash: state.chain_hash,
                    last_sequence: effect.last_sequence,
                },
            ),
        )
    }

    #[cfg(any(unix, all(test, windows)))]
    pub fn prepare(
        &self,
        lease: &ProjectMutationLeaseV1,
        expected: ManagementEffectCursorV1,
        request: PrepareManagementEffectV1,
    ) -> Result<PrepareManagementEffectOutcomeV1, ManagementEffectStoreError> {
        self.prepare_with_lease(JournalLeaseV1::Mutation(lease), expected, request)
    }

    /// Restricts shared-activity journal writes to runtime-local session controls.
    #[cfg(unix)]
    pub fn prepare_runtime(
        &self,
        lease: &ProjectActivityLeaseV1,
        expected: ManagementEffectCursorV1,
        request: PrepareManagementEffectV1,
    ) -> Result<PrepareManagementEffectOutcomeV1, ManagementEffectStoreError> {
        self.prepare_with_lease(JournalLeaseV1::Runtime(lease), expected, request)
    }

    #[cfg(any(unix, all(test, windows)))]
    fn prepare_with_lease(
        &self,
        lease: JournalLeaseV1<'_>,
        expected: ManagementEffectCursorV1,
        request: PrepareManagementEffectV1,
    ) -> Result<PrepareManagementEffectOutcomeV1, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        self.check_journal_lease(lease)?;
        validate_request_shape(self.project_scope, &request)?;
        validate_new_inventory_contract(&request)?;
        self.check_runtime_request(lease, &request)?;
        let mut state = self.load()?;
        if let Some(existing) = state.effects.get(&request.operation_id) {
            if existing.request != request {
                return Err(ManagementEffectStoreError::RequestConflict);
            }
            return Ok(match existing.state {
                ManagementEffectStateV1::Prepared => {
                    PrepareManagementEffectOutcomeV1::Prepared(PreparedManagementEffectV1 {
                        verified_event: Box::new(
                            self.latest_event_for_operation_unlocked(request.operation_id)?,
                        ),
                        request,
                        generation: state.generation,
                        chain_hash: state.chain_hash,
                        last_sequence: existing.last_sequence,
                    })
                }
                _ => PrepareManagementEffectOutcomeV1::Existing(existing.clone()),
            });
        }
        if let Some(existing) = self.archived_effect(request.operation_id)? {
            return if existing.request == request {
                Ok(PrepareManagementEffectOutcomeV1::Existing(existing))
            } else {
                Err(ManagementEffectStoreError::RequestConflict)
            };
        }
        if state
            .effects
            .values()
            .any(|effect| effect.state.blocks_new_work())
        {
            return Err(ManagementEffectStoreError::UnresolvedEffect);
        }
        ensure_cursor(&state, expected)?;
        if self.limits.max_records < 4
            || self.limits.max_journal_bytes
                < 4_u64.saturating_mul(self.limits.max_record_bytes as u64)
        {
            return Err(ManagementEffectStoreError::StoreFull);
        }
        if !self.has_prepare_reserve(&state)? {
            match lease {
                JournalLeaseV1::Mutation(lease) => self.seal_generation(lease, &state, false)?,
                JournalLeaseV1::Runtime(_) => return Err(ManagementEffectStoreError::StoreFull),
            }
            state = self.load()?;
            if !self.has_prepare_reserve(&state)? {
                return Err(ManagementEffectStoreError::StoreFull);
            }
        }
        let sequence = self.append_with_lease(
            lease,
            &state,
            JournalEventV1::Prepared {
                request: request.clone(),
            },
        )?;
        Ok(PrepareManagementEffectOutcomeV1::Prepared(
            PreparedManagementEffectV1 {
                verified_event: Box::new(
                    self.latest_event_for_operation_unlocked(request.operation_id)?,
                ),
                request,
                generation: state.generation,
                chain_hash: state.chain_hash,
                last_sequence: sequence.evidence.sequence,
            },
        ))
    }

    #[cfg(any(unix, all(test, windows)))]
    pub fn deny(
        &self,
        lease: &ProjectMutationLeaseV1,
        effect: &PreparedManagementEffectV1,
        reason: ManagementDenialReasonV1,
    ) -> Result<DeniedManagementEffectV1, ManagementEffectStoreError> {
        self.deny_with_lease(JournalLeaseV1::Mutation(lease), effect, reason)
    }

    /// Restricts shared-activity journal writes to runtime-local session controls.
    #[cfg(unix)]
    pub fn deny_runtime(
        &self,
        lease: &ProjectActivityLeaseV1,
        effect: &PreparedManagementEffectV1,
        reason: ManagementDenialReasonV1,
    ) -> Result<DeniedManagementEffectV1, ManagementEffectStoreError> {
        self.deny_with_lease(JournalLeaseV1::Runtime(lease), effect, reason)
    }

    #[cfg(any(unix, all(test, windows)))]
    fn deny_with_lease(
        &self,
        lease: JournalLeaseV1<'_>,
        effect: &PreparedManagementEffectV1,
        reason: ManagementDenialReasonV1,
    ) -> Result<DeniedManagementEffectV1, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        let state = self.checked_effect_state_with_lease(
            lease,
            &effect.request,
            effect.generation,
            effect.chain_hash,
        )?;
        let current = state
            .effects
            .get(&effect.operation_id())
            .ok_or(ManagementEffectStoreError::StaleEffect)?;
        match &current.state {
            ManagementEffectStateV1::Prepared if current.last_sequence == effect.last_sequence => {
                let sequence = self.append_with_lease(
                    lease,
                    &state,
                    JournalEventV1::Denied {
                        operation_id: effect.operation_id(),
                        reason,
                    },
                )?;
                Ok(DeniedManagementEffectV1 {
                    verified_event: self
                        .latest_event_for_operation_unlocked(effect.operation_id())?,
                    request: effect.request.clone(),
                    generation: state.generation,
                    chain_hash: state.chain_hash,
                    last_sequence: sequence.evidence.sequence,
                })
            }
            ManagementEffectStateV1::Denied { reason: found } if *found == reason => {
                Ok(DeniedManagementEffectV1 {
                    verified_event: self
                        .latest_event_for_operation_unlocked(effect.operation_id())?,
                    request: effect.request.clone(),
                    generation: state.generation,
                    chain_hash: state.chain_hash,
                    last_sequence: current.last_sequence,
                })
            }
            terminal if terminal.is_terminal() => {
                Err(ManagementEffectStoreError::ImmutableTerminal)
            }
            _ => Err(ManagementEffectStoreError::InvalidTransition),
        }
    }

    #[cfg(any(unix, all(test, windows)))]
    pub fn start(
        &self,
        lease: &ProjectMutationLeaseV1,
        effect: &PreparedManagementEffectV1,
    ) -> Result<StartedManagementEffectV1, ManagementEffectStoreError> {
        self.start_with_lease(JournalLeaseV1::Mutation(lease), effect)
    }

    /// Restricts shared-activity journal writes to runtime-local session controls.
    #[cfg(unix)]
    pub fn start_runtime(
        &self,
        lease: &ProjectActivityLeaseV1,
        effect: &PreparedManagementEffectV1,
    ) -> Result<StartedManagementEffectV1, ManagementEffectStoreError> {
        self.start_with_lease(JournalLeaseV1::Runtime(lease), effect)
    }

    #[cfg(any(unix, all(test, windows)))]
    fn start_with_lease(
        &self,
        lease: JournalLeaseV1<'_>,
        effect: &PreparedManagementEffectV1,
    ) -> Result<StartedManagementEffectV1, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        let state = self.checked_effect_state_with_lease(
            lease,
            &effect.request,
            effect.generation,
            effect.chain_hash,
        )?;
        let current = state
            .effects
            .get(&effect.operation_id())
            .ok_or(ManagementEffectStoreError::StaleEffect)?;
        match &current.state {
            ManagementEffectStateV1::Prepared if current.last_sequence == effect.last_sequence => {
                let sequence = self.append_with_lease(
                    lease,
                    &state,
                    JournalEventV1::EffectStarted {
                        operation_id: effect.operation_id(),
                    },
                )?;
                Ok(StartedManagementEffectV1 {
                    verified_event: self
                        .latest_event_for_operation_unlocked(effect.operation_id())?,
                    request: effect.request.clone(),
                    generation: state.generation,
                    chain_hash: state.chain_hash,
                    last_sequence: sequence.evidence.sequence,
                })
            }
            ManagementEffectStateV1::EffectStarted => Ok(StartedManagementEffectV1 {
                verified_event: self.latest_event_for_operation_unlocked(effect.operation_id())?,
                request: effect.request.clone(),
                generation: state.generation,
                chain_hash: state.chain_hash,
                last_sequence: current.last_sequence,
            }),
            ManagementEffectStateV1::OutcomeUnknown { .. } => {
                Err(ManagementEffectStoreError::UnresolvedEffect)
            }
            terminal if terminal.is_terminal() => {
                Err(ManagementEffectStoreError::ImmutableTerminal)
            }
            _ => Err(ManagementEffectStoreError::InvalidTransition),
        }
    }

    #[cfg(any(unix, all(test, windows)))]
    pub fn complete_started(
        &self,
        lease: &ProjectMutationLeaseV1,
        effect: &StartedManagementEffectV1,
        evidence_hash: ManagementEffectHashV1,
    ) -> Result<CompletedManagementEffectV1, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        validate_evidence_hash(evidence_hash)?;
        self.complete_from_started_or_replay(JournalLeaseV1::Mutation(lease), effect, evidence_hash)
    }

    #[cfg(unix)]
    pub fn complete_started_runtime(
        &self,
        lease: &ProjectActivityLeaseV1,
        effect: &StartedManagementEffectV1,
        evidence_hash: ManagementEffectHashV1,
    ) -> Result<CompletedManagementEffectV1, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        validate_evidence_hash(evidence_hash)?;
        self.complete_from_started_or_replay(JournalLeaseV1::Runtime(lease), effect, evidence_hash)
    }

    #[cfg(any(unix, all(test, windows)))]
    pub fn mark_outcome_unknown(
        &self,
        lease: &ProjectMutationLeaseV1,
        effect: &StartedManagementEffectV1,
        reason: ManagementUnknownReasonV1,
    ) -> Result<UnknownManagementEffectV1, ManagementEffectStoreError> {
        self.mark_outcome_unknown_with_lease(JournalLeaseV1::Mutation(lease), effect, reason)
    }

    /// Restricts shared-activity journal writes to runtime-local session controls.
    #[cfg(unix)]
    pub fn mark_outcome_unknown_runtime(
        &self,
        lease: &ProjectActivityLeaseV1,
        effect: &StartedManagementEffectV1,
        reason: ManagementUnknownReasonV1,
    ) -> Result<UnknownManagementEffectV1, ManagementEffectStoreError> {
        self.mark_outcome_unknown_with_lease(JournalLeaseV1::Runtime(lease), effect, reason)
    }

    #[cfg(any(unix, all(test, windows)))]
    fn mark_outcome_unknown_with_lease(
        &self,
        lease: JournalLeaseV1<'_>,
        effect: &StartedManagementEffectV1,
        reason: ManagementUnknownReasonV1,
    ) -> Result<UnknownManagementEffectV1, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        let state = self.checked_effect_state_with_lease(
            lease,
            &effect.request,
            effect.generation,
            effect.chain_hash,
        )?;
        let current = state
            .effects
            .get(&effect.operation_id())
            .ok_or(ManagementEffectStoreError::StaleEffect)?;
        match &current.state {
            ManagementEffectStateV1::EffectStarted
                if current.last_sequence == effect.last_sequence =>
            {
                let sequence = self.append_with_lease(
                    lease,
                    &state,
                    JournalEventV1::OutcomeUnknown {
                        operation_id: effect.operation_id(),
                        reason,
                    },
                )?;
                Ok(UnknownManagementEffectV1 {
                    verified_event: self
                        .latest_event_for_operation_unlocked(effect.operation_id())?,
                    request: effect.request.clone(),
                    generation: state.generation,
                    chain_hash: state.chain_hash,
                    last_sequence: sequence.evidence.sequence,
                })
            }
            ManagementEffectStateV1::OutcomeUnknown { reason: found } if *found == reason => {
                Ok(UnknownManagementEffectV1 {
                    verified_event: self
                        .latest_event_for_operation_unlocked(effect.operation_id())?,
                    request: effect.request.clone(),
                    generation: state.generation,
                    chain_hash: state.chain_hash,
                    last_sequence: current.last_sequence,
                })
            }
            terminal if terminal.is_terminal() => {
                Err(ManagementEffectStoreError::ImmutableTerminal)
            }
            _ => Err(ManagementEffectStoreError::InvalidTransition),
        }
    }

    #[cfg(any(unix, all(test, windows)))]
    pub fn complete_unknown(
        &self,
        lease: &ProjectMutationLeaseV1,
        effect: &UnknownManagementEffectV1,
        evidence_hash: ManagementEffectHashV1,
    ) -> Result<CompletedManagementEffectV1, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        validate_evidence_hash(evidence_hash)?;
        let state = self.checked_effect_state(
            lease,
            &effect.request,
            effect.generation,
            effect.chain_hash,
        )?;
        let current = state
            .effects
            .get(&effect.operation_id())
            .ok_or(ManagementEffectStoreError::StaleEffect)?;
        match &current.state {
            ManagementEffectStateV1::OutcomeUnknown { .. }
                if current.last_sequence == effect.last_sequence =>
            {
                let sequence = self.append(
                    lease,
                    &state,
                    JournalEventV1::Completed {
                        operation_id: effect.operation_id(),
                        evidence_hash,
                    },
                )?;
                Ok(CompletedManagementEffectV1 {
                    verified_event: self
                        .latest_event_for_operation_unlocked(effect.operation_id())?,
                    request: effect.request.clone(),
                    generation: state.generation,
                    chain_hash: state.chain_hash,
                    last_sequence: sequence.evidence.sequence,
                })
            }
            ManagementEffectStateV1::Completed {
                evidence_hash: found,
            } if *found == evidence_hash => Ok(CompletedManagementEffectV1 {
                verified_event: self.latest_event_for_operation_unlocked(effect.operation_id())?,
                request: effect.request.clone(),
                generation: state.generation,
                chain_hash: state.chain_hash,
                last_sequence: current.last_sequence,
            }),
            terminal if terminal.is_terminal() => {
                Err(ManagementEffectStoreError::ImmutableTerminal)
            }
            _ => Err(ManagementEffectStoreError::InvalidTransition),
        }
    }

    #[cfg(any(unix, all(test, windows)))]
    pub fn reconcile_abandon(
        &self,
        lease: &ProjectMutationLeaseV1,
        effect: &UnknownManagementEffectV1,
        evidence_hash: ManagementEffectHashV1,
    ) -> Result<AbandonedManagementEffectV1, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        validate_evidence_hash(evidence_hash)?;
        let state = self.checked_effect_state(
            lease,
            &effect.request,
            effect.generation,
            effect.chain_hash,
        )?;
        let current = state
            .effects
            .get(&effect.operation_id())
            .ok_or(ManagementEffectStoreError::StaleEffect)?;
        match &current.state {
            ManagementEffectStateV1::OutcomeUnknown { .. }
                if current.last_sequence == effect.last_sequence =>
            {
                let sequence = self.append(
                    lease,
                    &state,
                    JournalEventV1::ReconciledAbandon {
                        operation_id: effect.operation_id(),
                        evidence_hash,
                    },
                )?;
                Ok(AbandonedManagementEffectV1 {
                    verified_event: self
                        .latest_event_for_operation_unlocked(effect.operation_id())?,
                    request: effect.request.clone(),
                    generation: state.generation,
                    chain_hash: state.chain_hash,
                    last_sequence: sequence.evidence.sequence,
                })
            }
            ManagementEffectStateV1::ReconciledAbandon {
                evidence_hash: found,
            } if *found == evidence_hash => Ok(AbandonedManagementEffectV1 {
                verified_event: self.latest_event_for_operation_unlocked(effect.operation_id())?,
                request: effect.request.clone(),
                generation: state.generation,
                chain_hash: state.chain_hash,
                last_sequence: current.last_sequence,
            }),
            terminal if terminal.is_terminal() => {
                Err(ManagementEffectStoreError::ImmutableTerminal)
            }
            _ => Err(ManagementEffectStoreError::InvalidTransition),
        }
    }

    #[cfg(any(unix, all(test, windows)))]
    fn complete_from_started_or_replay(
        &self,
        lease: JournalLeaseV1<'_>,
        effect: &StartedManagementEffectV1,
        evidence_hash: ManagementEffectHashV1,
    ) -> Result<CompletedManagementEffectV1, ManagementEffectStoreError> {
        let state = self.checked_effect_state_with_lease(
            lease,
            &effect.request,
            effect.generation,
            effect.chain_hash,
        )?;
        let current = state
            .effects
            .get(&effect.operation_id())
            .ok_or(ManagementEffectStoreError::StaleEffect)?;
        match &current.state {
            ManagementEffectStateV1::EffectStarted
                if current.last_sequence == effect.last_sequence =>
            {
                let sequence = self.append_with_lease(
                    lease,
                    &state,
                    JournalEventV1::Completed {
                        operation_id: effect.operation_id(),
                        evidence_hash,
                    },
                )?;
                Ok(CompletedManagementEffectV1 {
                    verified_event: self
                        .latest_event_for_operation_unlocked(effect.operation_id())?,
                    request: effect.request.clone(),
                    generation: state.generation,
                    chain_hash: state.chain_hash,
                    last_sequence: sequence.evidence.sequence,
                })
            }
            ManagementEffectStateV1::Completed {
                evidence_hash: found,
            } if *found == evidence_hash => Ok(CompletedManagementEffectV1 {
                verified_event: self.latest_event_for_operation_unlocked(effect.operation_id())?,
                request: effect.request.clone(),
                generation: state.generation,
                chain_hash: state.chain_hash,
                last_sequence: current.last_sequence,
            }),
            ManagementEffectStateV1::OutcomeUnknown { .. } => {
                Err(ManagementEffectStoreError::UnresolvedEffect)
            }
            terminal if terminal.is_terminal() => {
                Err(ManagementEffectStoreError::ImmutableTerminal)
            }
            _ => Err(ManagementEffectStoreError::InvalidTransition),
        }
    }

    #[cfg(any(unix, all(test, windows)))]
    fn checked_effect_state(
        &self,
        lease: &ProjectMutationLeaseV1,
        request: &PrepareManagementEffectV1,
        generation: u64,
        chain_hash: ManagementEffectHashV1,
    ) -> Result<JournalStateV1, ManagementEffectStoreError> {
        self.checked_effect_state_with_lease(
            JournalLeaseV1::Mutation(lease),
            request,
            generation,
            chain_hash,
        )
    }

    #[cfg(any(unix, all(test, windows)))]
    fn checked_effect_state_with_lease(
        &self,
        lease: JournalLeaseV1<'_>,
        request: &PrepareManagementEffectV1,
        generation: u64,
        chain_hash: ManagementEffectHashV1,
    ) -> Result<JournalStateV1, ManagementEffectStoreError> {
        self.check_journal_lease(lease)?;
        self.check_runtime_request(lease, request)?;
        let state = self.load()?;
        if state.generation != generation || state.chain_hash != chain_hash {
            return Err(ManagementEffectStoreError::StaleEffect);
        }
        let current = state
            .effects
            .get(&request.operation_id)
            .ok_or(ManagementEffectStoreError::StaleEffect)?;
        if current.request != *request {
            return Err(ManagementEffectStoreError::RequestConflict);
        }
        Ok(state)
    }

    #[cfg(any(unix, all(test, windows)))]
    fn recover_interrupted(
        &self,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<(), ManagementEffectStoreError> {
        loop {
            let state = self.load()?;
            let Some(operation_id) = state.effects.iter().find_map(|(operation_id, effect)| {
                matches!(effect.state, ManagementEffectStateV1::EffectStarted)
                    .then_some(*operation_id)
            }) else {
                return Ok(());
            };
            self.append(
                lease,
                &state,
                JournalEventV1::OutcomeUnknown {
                    operation_id,
                    reason: ManagementUnknownReasonV1::ProcessRestart,
                },
            )?;
        }
    }

    #[cfg(any(unix, all(test, windows)))]
    fn has_prepare_reserve(
        &self,
        state: &JournalStateV1,
    ) -> Result<bool, ManagementEffectStoreError> {
        let bytes = self.m_length(self.journal_for_generation(state.generation))?;
        Ok(state.effects.len() < self.limits.max_effects
            && state
                .records
                .checked_add(4)
                .is_some_and(|count| count <= self.limits.max_records)
            && bytes
                .checked_add(4_u64.saturating_mul(self.limits.max_record_bytes as u64))
                .is_some_and(|length| length <= self.limits.max_journal_bytes))
    }

    #[cfg(any(unix, all(test, windows)))]
    fn has_pending_reserve(
        &self,
        state: &JournalStateV1,
    ) -> Result<bool, ManagementEffectStoreError> {
        let needed = state
            .effects
            .values()
            .try_fold(0_usize, |total, effect| {
                total.checked_add(match effect.state {
                    ManagementEffectStateV1::Prepared => 3,
                    ManagementEffectStateV1::EffectStarted => 2,
                    ManagementEffectStateV1::OutcomeUnknown { .. } => 1,
                    _ => 0,
                })
            })
            .ok_or(ManagementEffectStoreError::StoreFull)?;
        let bytes = self.m_length(self.journal_for_generation(state.generation))?;
        Ok(state
            .records
            .checked_add(needed)
            .is_some_and(|count| count <= self.limits.max_records)
            && bytes
                .checked_add((needed as u64).saturating_mul(self.limits.max_record_bytes as u64))
                .is_some_and(|length| length <= self.limits.max_journal_bytes))
    }

    #[cfg(any(unix, all(test, windows)))]
    fn append(
        &self,
        lease: &ProjectMutationLeaseV1,
        state: &JournalStateV1,
        event: JournalEventV1,
    ) -> Result<VerifiedManagementEffectEventV1, ManagementEffectStoreError> {
        self.append_with_lease(JournalLeaseV1::Mutation(lease), state, event)
    }

    #[cfg(any(unix, all(test, windows)))]
    fn append_with_lease(
        &self,
        lease: JournalLeaseV1<'_>,
        state: &JournalStateV1,
        event: JournalEventV1,
    ) -> Result<VerifiedManagementEffectEventV1, ManagementEffectStoreError> {
        self.check_journal_lease(lease)?;
        if let JournalLeaseV1::Runtime(_) = lease {
            let request = match &event {
                JournalEventV1::Prepared { request } => request,
                JournalEventV1::ReconciledAbandon { .. } => {
                    return Err(ManagementEffectStoreError::InvalidTransition);
                }
                _ => {
                    &state
                        .effects
                        .get(&event.operation_id())
                        .ok_or(ManagementEffectStoreError::StaleEffect)?
                        .request
                }
            };
            self.check_runtime_request(lease, request)?;
        }
        let fresh = self.load()?;
        if fresh.cursor() != state.cursor() {
            return Err(ManagementEffectStoreError::StaleCursor);
        }
        if state.records >= self.limits.max_records {
            return Err(ManagementEffectStoreError::StoreFull);
        }
        let sequence = state.sequence + 1;
        let unsigned = UnsignedEnvelopeV1 {
            version: JOURNAL_VERSION,
            sequence,
            previous_hash: state.journal_hash,
            event: &event,
        };
        let event_hash = ManagementEffectHashV1::digest(&serde_json::to_vec(&unsigned)?);
        let envelope = JournalEnvelopeV1 {
            version: JOURNAL_VERSION,
            sequence,
            previous_hash: state.journal_hash,
            event,
            event_hash,
        };
        let mut bytes = serde_json::to_vec(&envelope)?;
        bytes.push(b'\n');
        if bytes.len() > self.limits.max_record_bytes {
            return Err(ManagementEffectStoreError::StoreFull);
        }
        let journal_path = self.journal_for_generation(state.generation);
        let current_bytes = self.m_length(&journal_path)?;
        if current_bytes.saturating_add(bytes.len() as u64) > self.limits.max_journal_bytes {
            return Err(ManagementEffectStoreError::StoreFull);
        }
        let mut file = self.m_open(&journal_path, true)?;
        file.seek(SeekFrom::End(0))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        self.m_sync(
            journal_path
                .parent()
                .ok_or(ManagementEffectStoreError::InvalidStore)?,
        )?;
        file.seek(SeekFrom::Start(current_bytes))?;
        let mut readback = Vec::new();
        file.take(bytes.len() as u64 + 1)
            .read_to_end(&mut readback)?;
        if readback != bytes {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        let readback: JournalEnvelopeV1 = serde_json::from_slice(&readback[..readback.len() - 1])?;
        Ok(VerifiedManagementEffectEventV1 {
            evidence: ManagementEffectEventEvidenceV1 {
                generation: state.generation,
                sequence: readback.sequence,
                event_hash: readback.event_hash,
            },
            event: readback.event,
        })
    }

    #[cfg(any(unix, all(test, windows)))]
    fn repair_torn_tail(
        &self,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<(), ManagementEffectStoreError> {
        self.check_lease(lease)?;
        let active = self.active_generation()?;
        let journal_path = active.journal_path.clone();
        let bytes = self.m_read(&journal_path, self.limits.max_journal_bytes)?;
        let valid = complete_prefix_len(&bytes);
        if valid == bytes.len() {
            return Ok(());
        }
        self.parse_journal_bytes(&active, &bytes[..valid])?;
        let mut file = self.m_open(&journal_path, true)?;
        file.set_len(valid as u64)?;
        file.seek(SeekFrom::Start(valid as u64))?;
        file.sync_all()?;
        self.m_sync(
            journal_path
                .parent()
                .ok_or(ManagementEffectStoreError::InvalidStore)?,
        )?;
        Ok(())
    }

    #[cfg(any(unix, all(test, windows)))]
    fn load(&self) -> Result<JournalStateV1, ManagementEffectStoreError> {
        self.m_validate_directory(&self.root)?;
        let active = self.active_generation()?;
        self.load_at(&active)
    }

    #[cfg(any(unix, all(test, windows)))]
    fn load_at(
        &self,
        active: &ActiveGenerationV1,
    ) -> Result<JournalStateV1, ManagementEffectStoreError> {
        self.m_validate_directory(
            active
                .journal_path
                .parent()
                .ok_or(ManagementEffectStoreError::InvalidStore)?,
        )?;
        self.m_validate_file(&active.journal_path)?;
        let bytes = self.m_read(&active.journal_path, self.limits.max_journal_bytes)?;
        if complete_prefix_len(&bytes) != bytes.len() {
            return Err(ManagementEffectStoreError::TornTail);
        }
        self.parse_journal_bytes(active, &bytes)
    }

    #[cfg(any(unix, all(test, windows)))]
    fn parse_journal_bytes(
        &self,
        active: &ActiveGenerationV1,
        bytes: &[u8],
    ) -> Result<JournalStateV1, ManagementEffectStoreError> {
        self.parse_journal_bytes_scoped(active, bytes, Some(self.project_scope))
    }
    #[cfg(any(unix, all(test, windows)))]
    fn parse_journal_bytes_scoped(
        &self,
        active: &ActiveGenerationV1,
        bytes: &[u8],
        scope: Option<ManagementEffectHashV1>,
    ) -> Result<JournalStateV1, ManagementEffectStoreError> {
        let mut state = JournalStateV1 {
            generation: active.number,
            chain_hash: active.chain_hash,
            effects: active.pending.clone(),
            archived_effects: active.archived_effects,
            prior_records: active.prior_records,
            ..Default::default()
        };
        let complete = bytes.strip_suffix(b"\n").unwrap_or(bytes);
        for line in complete
            .split(|byte| *byte == b'\n')
            .filter(|_| !complete.is_empty())
        {
            if line.is_empty() || line.len() > self.limits.max_record_bytes {
                return Err(ManagementEffectStoreError::CorruptJournal);
            }
            if state.records >= self.limits.max_records {
                return Err(ManagementEffectStoreError::StoreFull);
            }
            let envelope: JournalEnvelopeV1 = serde_json::from_slice(line)
                .map_err(|_| ManagementEffectStoreError::CorruptJournal)?;
            state.apply(envelope)?;
        }
        if state.effects.len() > self.limits.max_effects {
            return Err(ManagementEffectStoreError::StoreFull);
        }
        if state.effects.values().any(|effect| {
            validate_request_shape(
                scope.unwrap_or(effect.request.scope.project),
                &effect.request,
            )
            .is_err()
        }) {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        #[cfg(all(test, windows))]
        for effect in state.effects.values() {
            windows_backend::require_inventory(&effect.request)?;
        }
        Ok(state)
    }

    #[cfg(unix)]
    fn check_runtime_request(
        &self,
        lease: JournalLeaseV1<'_>,
        request: &PrepareManagementEffectV1,
    ) -> Result<(), ManagementEffectStoreError> {
        if let JournalLeaseV1::Runtime(_) = lease {
            validate_request_shape(self.project_scope, request)?;
            validate_new_inventory_contract(request)?;
            if !matches!(
                request.inventory.encoded().as_str(),
                "D10" | "D12" | "D21" | "D23" | "D24"
            ) || request.scope.permission != ManagementPermissionScopeV1::LocalSession
                || request.rewind != ManagementRewindDispositionV1::IrreversibleLocal
                || request.scope.session.is_none()
                || request.scope.account_binding.is_some()
            {
                return Err(ManagementEffectStoreError::InvalidInventoryContract);
            }
        }
        Ok(())
    }

    #[cfg(unix)]
    fn check_journal_lease(
        &self,
        lease: JournalLeaseV1<'_>,
    ) -> Result<(), ManagementEffectStoreError> {
        match lease {
            JournalLeaseV1::Mutation(lease) => self.check_lease(lease),
            JournalLeaseV1::Runtime(lease) => {
                if lease.store_root() != self.expected_lease_root {
                    return Err(ManagementEffectStoreError::WrongProjectLease);
                }
                let root = crate::project_mutation::open_directory_nofollow(lease.store_root())?;
                let metadata = root.metadata()?;
                if (metadata.dev(), metadata.ino()) != lease.store_identity()
                    || metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.mode() & 0o7777 != 0o700
                {
                    return Err(ManagementEffectStoreError::WrongProjectLease);
                }
                let name = CString::new("writer.lease").expect("fixed lock name");
                let fd = unsafe {
                    libc::openat(
                        root.as_raw_fd(),
                        name.as_ptr(),
                        libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
                    )
                };
                if fd < 0 {
                    return Err(std::io::Error::last_os_error().into());
                }
                let lock = unsafe { File::from_raw_fd(fd) };
                let metadata = lock.metadata()?;
                if !metadata.is_file()
                    || metadata.nlink() != 1
                    || metadata.uid() != unsafe { libc::geteuid() }
                    || metadata.mode() & 0o7177 != 0
                    || (metadata.dev(), metadata.ino()) != lease.lock_identity()?
                {
                    return Err(ManagementEffectStoreError::WrongProjectLease);
                }
                Ok(())
            }
        }
    }

    #[cfg(unix)]
    fn check_lease(
        &self,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<(), ManagementEffectStoreError> {
        if lease.store_root() != self.expected_lease_root {
            return Err(ManagementEffectStoreError::WrongProjectLease);
        }
        Ok(())
    }

    #[cfg(unix)]
    fn lock_owner(&self) -> Result<File, ManagementEffectStoreError> {
        let file = open_journal(&self.root.join(OWNER_LOCK_NAME), true)?;
        file.lock()?;
        Ok(file)
    }

    #[cfg(test)]
    #[cfg(any(unix, all(test, windows)))]
    pub(super) fn test_open(
        state_dir: &Path,
        expected_lease_root: PathBuf,
        project_scope: ManagementEffectHashV1,
        lease: &ProjectMutationLeaseV1,
        limits: ManagementEffectLimitsV1,
    ) -> Result<Self, ManagementEffectStoreError> {
        Self::open_at(state_dir, expected_lease_root, project_scope, lease, limits)
    }

    #[cfg(test)]
    #[cfg(any(unix, all(test, windows)))]
    pub(super) fn test_journal_path(&self) -> &Path {
        &self.journal_path
    }

    #[cfg(test)]
    #[cfg(unix)]
    pub(crate) fn test_joined_seal(
        &self,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<(), ManagementEffectStoreError> {
        self.check_lease(lease)?;
        let _guard = self.lock_owner()?;
        let state = self.load()?;
        let root = crate::project_mutation::open_directory_nofollow(&self.root)?;
        self.seal_generation_joined(lease, &root, &state)
    }

    /// Exercises ordinary seal ordering with one synthetic in-memory counter.
    #[cfg(test)]
    #[cfg(unix)]
    pub(super) fn test_ordinary_seal_capacity_counter_overflow(
        &self,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<(), ManagementEffectStoreError> {
        self.check_lease(lease)?;
        let _guard = self.lock_owner()?;
        let mut state = self.load()?;
        state.archived_effects = u64::MAX;
        self.seal_generation(lease, &state, false)
    }

    #[cfg(test)]
    #[cfg(unix)]
    pub(super) fn test_joined_seal_prefix(
        &self,
        lease: &ProjectMutationLeaseV1,
        index: usize,
        foreign: bool,
    ) -> Result<(), ManagementEffectStoreError> {
        self.check_lease(lease)?;
        let _guard = self.lock_owner()?;
        self.test_stage_joined_prefix(index, foreign)
    }

    #[cfg(test)]
    #[cfg(unix)]
    pub(super) fn test_inspect(
        &self,
        lease: &ProjectMutationLeaseV1,
        limits: ManagementEffectLimitsV1,
    ) -> Result<ManagementEffectInspectOpenV1, ManagementEffectStoreError> {
        Self::inspect_at(&self.state_dir, self.project_scope, lease, limits)
    }

    #[cfg(test)]
    #[cfg(unix)]
    pub(super) fn test_inspect_migration(
        &self,
        lease: &ProjectMutationLeaseV1,
        limits: ManagementEffectLimitsV1,
    ) -> Result<ManagementEffectInspectOpenV1, ManagementEffectStoreError> {
        Self::inspect_at_mode(&self.state_dir, self.project_scope, lease, limits, true)
    }

    #[cfg(test)]
    #[cfg(unix)]
    pub(super) fn test_append_zero_evidence(
        &self,
        lease: &ProjectMutationLeaseV1,
        operation_id: ManagementOperationIdV1,
        abandon: bool,
    ) -> Result<(), ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        let state = self.load()?;
        let event = if abandon {
            JournalEventV1::ReconciledAbandon {
                operation_id,
                evidence_hash: ManagementEffectHashV1::zero(),
            }
        } else {
            JournalEventV1::Completed {
                operation_id,
                evidence_hash: ManagementEffectHashV1::zero(),
            }
        };
        self.append(lease, &state, event)?;
        Ok(())
    }

    #[cfg(test)]
    #[cfg(any(unix, all(test, windows)))]
    pub(super) fn test_append_legacy_prepared(
        &self,
        lease: &ProjectMutationLeaseV1,
        request: PrepareManagementEffectV1,
    ) -> Result<(), ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        self.check_lease(lease)?;
        let state = self.load()?;
        self.append(lease, &state, JournalEventV1::Prepared { request })?;
        Ok(())
    }
}

type JournalEventV1 = ManagementEffectEventV1;

#[derive(Debug, Serialize)]
struct UnsignedEnvelopeV1<'a> {
    version: u32,
    sequence: u64,
    previous_hash: ManagementEffectHashV1,
    event: &'a JournalEventV1,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct JournalEnvelopeV1 {
    version: u32,
    sequence: u64,
    previous_hash: ManagementEffectHashV1,
    event: JournalEventV1,
    event_hash: ManagementEffectHashV1,
}

#[derive(Debug, Default)]
struct JournalStateV1 {
    events: BTreeMap<u64, VerifiedManagementEffectEventV1>,
    generation: u64,
    chain_hash: ManagementEffectHashV1,
    archived_effects: u64,
    prior_records: u64,
    sequence: u64,
    journal_hash: ManagementEffectHashV1,
    records: usize,
    effects: BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV1>,
}

impl Default for ManagementEffectHashV1 {
    fn default() -> Self {
        Self::zero()
    }
}

impl JournalStateV1 {
    fn cursor(&self) -> ManagementEffectCursorV1 {
        ManagementEffectCursorV1 {
            generation: self.generation,
            chain_hash: self.chain_hash,
            sequence: self.sequence,
            journal_hash: self.journal_hash,
        }
    }

    #[cfg(any(unix, all(test, windows)))]
    fn apply(&mut self, envelope: JournalEnvelopeV1) -> Result<(), ManagementEffectStoreError> {
        if envelope.version != JOURNAL_VERSION
            || envelope.sequence != self.sequence + 1
            || envelope.previous_hash != self.journal_hash
        {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        let expected = ManagementEffectHashV1::digest(&serde_json::to_vec(&UnsignedEnvelopeV1 {
            version: envelope.version,
            sequence: envelope.sequence,
            previous_hash: envelope.previous_hash,
            event: &envelope.event,
        })?);
        if expected != envelope.event_hash {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        let verified = VerifiedManagementEffectEventV1 {
            evidence: ManagementEffectEventEvidenceV1 {
                generation: self.generation,
                sequence: envelope.sequence,
                event_hash: envelope.event_hash,
            },
            event: envelope.event.clone(),
        };
        self.apply_event(envelope.sequence, envelope.event)?;
        if self.events.insert(envelope.sequence, verified).is_some() {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        self.sequence = envelope.sequence;
        self.journal_hash = envelope.event_hash;
        self.records += 1;
        Ok(())
    }

    #[cfg(any(unix, all(test, windows)))]
    fn apply_event(
        &mut self,
        sequence: u64,
        event: JournalEventV1,
    ) -> Result<(), ManagementEffectStoreError> {
        match event {
            JournalEventV1::Prepared { request } => {
                if self.effects.contains_key(&request.operation_id) {
                    return Err(ManagementEffectStoreError::CorruptJournal);
                }
                self.effects.insert(
                    request.operation_id,
                    ManagementEffectSnapshotV1 {
                        request,
                        state: ManagementEffectStateV1::Prepared,
                        last_sequence: sequence,
                    },
                );
            }
            JournalEventV1::EffectStarted { operation_id } => self.transition(
                operation_id,
                sequence,
                |state| matches!(state, ManagementEffectStateV1::Prepared),
                ManagementEffectStateV1::EffectStarted,
            )?,
            JournalEventV1::OutcomeUnknown {
                operation_id,
                reason,
            } => self.transition(
                operation_id,
                sequence,
                |state| matches!(state, ManagementEffectStateV1::EffectStarted),
                ManagementEffectStateV1::OutcomeUnknown { reason },
            )?,
            JournalEventV1::Completed {
                operation_id,
                evidence_hash,
            } => {
                validate_evidence_hash(evidence_hash)
                    .map_err(|_| ManagementEffectStoreError::CorruptJournal)?;
                self.transition(
                    operation_id,
                    sequence,
                    |state| {
                        matches!(
                            state,
                            ManagementEffectStateV1::EffectStarted
                                | ManagementEffectStateV1::OutcomeUnknown { .. }
                        )
                    },
                    ManagementEffectStateV1::Completed { evidence_hash },
                )?;
            }
            JournalEventV1::Denied {
                operation_id,
                reason,
            } => self.transition(
                operation_id,
                sequence,
                |state| matches!(state, ManagementEffectStateV1::Prepared),
                ManagementEffectStateV1::Denied { reason },
            )?,
            JournalEventV1::ReconciledAbandon {
                operation_id,
                evidence_hash,
            } => {
                validate_evidence_hash(evidence_hash)
                    .map_err(|_| ManagementEffectStoreError::CorruptJournal)?;
                self.transition(
                    operation_id,
                    sequence,
                    |state| matches!(state, ManagementEffectStateV1::OutcomeUnknown { .. }),
                    ManagementEffectStateV1::ReconciledAbandon { evidence_hash },
                )?;
            }
        }
        Ok(())
    }

    #[cfg(any(unix, all(test, windows)))]
    fn transition(
        &mut self,
        operation_id: ManagementOperationIdV1,
        sequence: u64,
        allowed: impl FnOnce(&ManagementEffectStateV1) -> bool,
        next: ManagementEffectStateV1,
    ) -> Result<(), ManagementEffectStoreError> {
        let effect = self
            .effects
            .get_mut(&operation_id)
            .ok_or(ManagementEffectStoreError::CorruptJournal)?;
        if effect.state.is_terminal() || !allowed(&effect.state) {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        effect.state = next;
        effect.last_sequence = sequence;
        Ok(())
    }
}

#[cfg(unix)]
fn management_child_directory(
    parent: &File,
    name: &str,
) -> Result<File, ManagementEffectStoreError> {
    let name = CString::new(name).map_err(|_| ManagementEffectStoreError::InvalidStore)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    let meta = file.metadata()?;
    if !meta.is_dir() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o7777 != 0o700 {
        return Err(ManagementEffectStoreError::InvalidStore);
    }
    verify_management_entry(parent, &name, &file)?;
    Ok(file)
}

#[cfg(unix)]
fn verify_management_entry(
    parent: &File,
    name: &CString,
    file: &File,
) -> Result<(), ManagementEffectStoreError> {
    let mut entry = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            entry.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    let entry = unsafe { entry.assume_init() };
    let meta = file.metadata()?;
    if meta.dev() != entry.st_dev as u64 || meta.ino() != entry.st_ino {
        return Err(ManagementEffectStoreError::InvalidStore);
    }
    Ok(())
}

#[derive(Clone, Default)]
struct MigrationMountV2 {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    native: Option<std::sync::Arc<polaris_mcp::NativeMountIdentityV2>>,
}
impl std::fmt::Debug for MigrationMountV2 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("MigrationMountV2")
    }
}
#[cfg(unix)]
impl MigrationMountV2 {
    fn inspect(file: &File, enabled: bool) -> Result<Self, ManagementEffectStoreError> {
        if !enabled {
            return Ok(Self::default());
        }
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        {
            Ok(Self {
                native: Some(std::sync::Arc::new(management_native_mount(file)?)),
            })
        }
        #[cfg(not(any(target_os = "macos", target_os = "linux")))]
        {
            let _ = file;
            Err(ManagementEffectStoreError::InvalidStore)
        }
    }
    fn verify(&self, file: &File) -> Result<(), ManagementEffectStoreError> {
        #[cfg(any(target_os = "macos", target_os = "linux"))]
        if let Some(expected) = &self.native
            && management_native_mount(file)? != **expected
        {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        let _ = file;
        Ok(())
    }
}
#[cfg(any(target_os = "macos", target_os = "linux"))]
#[cfg(unix)]
fn management_native_mount(
    file: &File,
) -> Result<polaris_mcp::NativeMountIdentityV2, ManagementEffectStoreError> {
    #[cfg(test)]
    let metadata = file.metadata()?;
    #[cfg(test)]
    if REJECT_MIGRATION_MOUNT.with(|slot| slot.get() == Some((metadata.dev(), metadata.ino()))) {
        return Err(ManagementEffectStoreError::InvalidStore);
    }
    polaris_mcp::NativeMountIdentityV2::inspect(file)
        .map_err(|_| ManagementEffectStoreError::InvalidStore)
}
#[cfg(unix)]
fn management_stamp(meta: &fs::Metadata) -> (u64, u64, u32, u32, u64, u64, i64, i64, i64, i64) {
    (
        meta.dev(),
        meta.ino(),
        meta.uid(),
        meta.mode(),
        meta.nlink(),
        meta.len(),
        meta.mtime(),
        meta.mtime_nsec(),
        meta.ctime(),
        meta.ctime_nsec(),
    )
}
#[cfg(all(test, unix))]
type ManagementMigrationHookV1 = std::cell::RefCell<Option<Box<dyn FnOnce()>>>;

#[cfg(all(test, unix))]
thread_local! {
    static REJECT_MIGRATION_MOUNT: std::cell::Cell<Option<(u64,u64)>> = const { std::cell::Cell::new(None) };
    static BEFORE_MIGRATION_REFRESH: ManagementMigrationHookV1 = const { std::cell::RefCell::new(None) };
    static AFTER_MIGRATION_CENSUS: ManagementMigrationHookV1 = const { std::cell::RefCell::new(None) };
}
#[cfg(test)]
#[cfg(unix)]
fn management_migration_hook(slot: &'static std::thread::LocalKey<ManagementMigrationHookV1>) {
    if let Some(hook) = slot.with(|slot| slot.borrow_mut().take()) {
        hook();
    }
}

#[cfg(unix)]
fn management_file_bytes(file: &File, max: u64) -> Result<Vec<u8>, ManagementEffectStoreError> {
    use std::os::unix::fs::FileExt;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.nlink() != 1
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o7777 != 0o600
    {
        return Err(ManagementEffectStoreError::InvalidStore);
    }
    if meta.len() > max {
        return Err(ManagementEffectStoreError::StoreFull);
    }
    let length = usize::try_from(meta.len()).map_err(|_| ManagementEffectStoreError::StoreFull)?;
    let mut data = vec![0; length];
    file.read_exact_at(&mut data, 0)?;
    if management_stamp(&file.metadata()?) != management_stamp(&meta) {
        return Err(ManagementEffectStoreError::InvalidStore);
    }
    Ok(data)
}

#[cfg(unix)]
fn pin_management_file(
    parent: &File,
    name: &str,
    relative: &str,
    max: u64,
) -> Result<PinnedManagementFileV1, ManagementEffectStoreError> {
    pin_management_file_mounted(parent, name, relative, max, &MigrationMountV2::default())
}

#[cfg(unix)]
fn pin_management_file_mounted(
    parent: &File,
    name: &str,
    relative: &str,
    max: u64,
    migration_mount: &MigrationMountV2,
) -> Result<PinnedManagementFileV1, ManagementEffectStoreError> {
    let name = CString::new(name).map_err(|_| ManagementEffectStoreError::InvalidStore)?;
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_CLOEXEC | libc::O_NONBLOCK,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    let file = unsafe { File::from_raw_fd(fd) };
    migration_mount.verify(parent)?;
    migration_mount.verify(&file)?;
    let metadata = file.metadata()?;
    verify_management_entry(parent, &name, &file)?;
    let bytes = management_file_bytes(&file, max)?;
    let meta = file.metadata()?;
    let evidence = ManagementEffectInspectedFileV1 {
        relative_name: relative.to_owned(),
        length: bytes.len() as u64,
        hash: ManagementEffectHashV1::digest(&bytes),
        device: meta.dev(),
        inode: meta.ino(),
    };
    let pinned = PinnedManagementFileV1 {
        parent: parent.try_clone()?,
        name,
        file,
        evidence,
        bytes,
        metadata,
        migration_mount: migration_mount.clone(),
    };
    verify_management_file(&pinned)?;
    Ok(pinned)
}

#[cfg(unix)]
fn verify_management_file(file: &PinnedManagementFileV1) -> Result<(), ManagementEffectStoreError> {
    file.migration_mount.verify(&file.parent)?;
    file.migration_mount.verify(&file.file)?;
    verify_management_entry(&file.parent, &file.name, &file.file)?;
    let meta = file.file.metadata()?;
    if management_stamp(&meta) != management_stamp(&file.metadata) {
        return Err(ManagementEffectStoreError::InvalidStore);
    }
    let bytes = management_file_bytes(&file.file, file.evidence.length)?;
    if (meta.dev(), meta.ino()) != (file.evidence.device, file.evidence.inode)
        || bytes != file.bytes
        || ManagementEffectHashV1::digest(&bytes) != file.evidence.hash
    {
        return Err(ManagementEffectStoreError::InvalidStore);
    }
    Ok(())
}

struct ManagementInventoryV1 {
    files: BTreeMap<String, PinnedManagementFileV1>,
    directories: BTreeMap<String, (u64, u64)>,
    migration_directories: BTreeMap<String, (File, fs::Metadata)>,
    #[cfg(all(test, windows))]
    windows_directories: BTreeMap<String, ManagementDirectoryV1>,
}

#[cfg(unix)]
fn management_inventory_mode(
    root: &File,
    limits: ManagementEffectLimitsV1,
    migration_inspection: bool,
) -> Result<ManagementInventoryV1, ManagementEffectStoreError> {
    use crate::project_mutation::directory_names_bounded;
    let mount = MigrationMountV2::inspect(root, migration_inspection)?;
    let root_metadata = root.metadata()?;
    let mut files = BTreeMap::new();
    let mut directories = BTreeMap::new();
    let mut migration_directories = BTreeMap::new();
    for name in directory_names_bounded(root, limits.max_records)? {
        let name = name
            .into_string()
            .map_err(|_| ManagementEffectStoreError::InvalidStore)?;
        let max = match name.as_str() {
            OWNER_LOCK_NAME => 4096,
            JOURNAL_NAME => limits.max_journal_bytes,
            "CURRENT" => 4096,
            "generations" => {
                let generations = management_child_directory(root, &name)?;
                mount.verify(&generations)?;
                let meta = generations.metadata()?;
                if migration_inspection {
                    migration_directories
                        .insert(name.clone(), (generations.try_clone()?, meta.clone()));
                }
                directories.insert(name.clone(), (meta.dev(), meta.ino()));
                for number in directory_names_bounded(&generations, limits.max_records)? {
                    let number = number
                        .into_string()
                        .map_err(|_| ManagementEffectStoreError::InvalidStore)?;
                    let orphan = if migration_inspection {
                        number
                            .strip_prefix(".orphan-")
                            .and_then(|name| name.split_once('-'))
                    } else {
                        None
                    };
                    if let Some((_, suffix)) = orphan
                        && suffix
                            .parse::<u64>()
                            .ok()
                            .is_none_or(|n| n.to_string() != suffix)
                    {
                        return Err(ManagementEffectStoreError::CorruptJournal);
                    }
                    let decimal = orphan.map(|(number, _)| number).unwrap_or_else(|| {
                        number
                            .strip_prefix(".seal-")
                            .and_then(|n| n.strip_suffix(".v1.tmp"))
                            .unwrap_or(&number)
                    });
                    let parsed = decimal
                        .parse::<u64>()
                        .map_err(|_| ManagementEffectStoreError::CorruptJournal)?;
                    if parsed == 0 || parsed.to_string() != decimal {
                        return Err(ManagementEffectStoreError::CorruptJournal);
                    }
                    let directory = management_child_directory(&generations, &number)?;
                    mount.verify(&directory)?;
                    let meta = directory.metadata()?;
                    if migration_inspection {
                        migration_directories.insert(
                            format!("generations/{number}"),
                            (directory.try_clone()?, meta.clone()),
                        );
                    }
                    directories.insert(format!("generations/{number}"), (meta.dev(), meta.ino()));
                    for entry in directory_names_bounded(&directory, 4)? {
                        let entry = entry
                            .into_string()
                            .map_err(|_| ManagementEffectStoreError::InvalidStore)?;
                        let max = match entry.as_str() {
                            "archive.v1.json" => 64 * 1024 * 1024,
                            "checkpoint.v1.json" => 64 * 1024,
                            "manifest.v1.json" => 4096,
                            JOURNAL_NAME => limits.max_journal_bytes,
                            _ => return Err(ManagementEffectStoreError::CorruptJournal),
                        };
                        let relative = format!("generations/{number}/{entry}");
                        files.insert(
                            relative.clone(),
                            pin_management_file_mounted(
                                &directory, &entry, &relative, max, &mount,
                            )?,
                        );
                    }
                    verify_management_entry(
                        &generations,
                        &CString::new(number).expect("validated name"),
                        &directory,
                    )?;
                }
                verify_management_entry(
                    root,
                    &CString::new(name).expect("fixed name"),
                    &generations,
                )?;
                continue;
            }
            other
                if other
                    .strip_prefix(".CURRENT-")
                    .and_then(|n| n.strip_suffix(".v1.tmp"))
                    .and_then(|n| n.parse::<u64>().ok())
                    .is_some() =>
            {
                4096
            }
            _ => return Err(ManagementEffectStoreError::CorruptJournal),
        };
        files.insert(
            name.clone(),
            pin_management_file_mounted(root, &name, &name, max, &mount)?,
        );
    }
    #[cfg(test)]
    if migration_inspection {
        management_migration_hook(&AFTER_MIGRATION_CENSUS);
    }
    for file in files.values() {
        verify_management_file(file)?;
    }
    mount.verify(root)?;
    if migration_inspection
        && management_stamp(&root.metadata()?) != management_stamp(&root_metadata)
    {
        return Err(ManagementEffectStoreError::InvalidStore);
    }
    for (file, metadata) in migration_directories.values() {
        mount.verify(file)?;
        if management_stamp(&file.metadata()?) != management_stamp(metadata) {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
    }
    Ok(ManagementInventoryV1 {
        files,
        directories,
        migration_directories,
    })
}

#[cfg(any(unix, all(test, windows)))]
fn event_from_snapshot(effect: &ManagementEffectSnapshotV1) -> ManagementEffectEventV1 {
    let operation_id = effect.request.operation_id;
    match effect.state {
        ManagementEffectStateV1::Prepared => JournalEventV1::Prepared {
            request: effect.request.clone(),
        },
        ManagementEffectStateV1::EffectStarted => JournalEventV1::EffectStarted { operation_id },
        ManagementEffectStateV1::OutcomeUnknown { reason } => JournalEventV1::OutcomeUnknown {
            operation_id,
            reason,
        },
        ManagementEffectStateV1::Completed { evidence_hash } => JournalEventV1::Completed {
            operation_id,
            evidence_hash,
        },
        ManagementEffectStateV1::Denied { reason } => JournalEventV1::Denied {
            operation_id,
            reason,
        },
        ManagementEffectStateV1::ReconciledAbandon { evidence_hash } => {
            JournalEventV1::ReconciledAbandon {
                operation_id,
                evidence_hash,
            }
        }
    }
}

#[cfg(any(unix, all(test, windows)))]
fn ensure_cursor(
    state: &JournalStateV1,
    expected: ManagementEffectCursorV1,
) -> Result<(), ManagementEffectStoreError> {
    if state.cursor() == expected {
        Ok(())
    } else {
        Err(ManagementEffectStoreError::StaleCursor)
    }
}

pub(super) fn validate_request_shape(
    project_scope: ManagementEffectHashV1,
    request: &PrepareManagementEffectV1,
) -> Result<(), ManagementEffectStoreError> {
    if request.scope.project != project_scope {
        return Err(ManagementEffectStoreError::WrongProjectScope);
    }
    if request.operation_id.0.0 == [0; ID_BYTES]
        || request.request_hash.0.0 == [0; HASH_BYTES]
        || request.scope.project.0.0 == [0; HASH_BYTES]
        || request
            .scope
            .session
            .is_some_and(|value| value.0.0 == [0; HASH_BYTES])
        || request
            .scope
            .account_binding
            .is_some_and(|value| value.0.0 == [0; HASH_BYTES])
    {
        return Err(ManagementEffectStoreError::InvalidValue);
    }
    Ok(())
}

#[derive(Clone, Copy)]
enum FieldPresenceV1 {
    Forbidden,
    Optional,
    Required,
}

impl FieldPresenceV1 {
    fn accepts<T>(self, value: &Option<T>) -> bool {
        match self {
            Self::Forbidden => value.is_none(),
            Self::Optional => true,
            Self::Required => value.is_some(),
        }
    }
}

pub(super) fn validate_new_inventory_contract(
    request: &PrepareManagementEffectV1,
) -> Result<(), ManagementEffectStoreError> {
    use FieldPresenceV1::{Forbidden, Optional, Required};
    use ManagementPermissionScopeV1::{Clipboard, LocalSession, Project, Remote, System};
    use ManagementRewindDispositionV1::{
        CapturedRestorable, IrreversibleLocal, IrreversibleRemote, RuntimeUnknown,
    };

    let (permissions, rewind, session, account): (
        &[ManagementPermissionScopeV1],
        ManagementRewindDispositionV1,
        FieldPresenceV1,
        FieldPresenceV1,
    ) = match request.inventory.encoded().as_str() {
        "T03" | "T04" | "D17" | "D19" | "D20" | "F12" | "F15" => {
            (&[LocalSession], CapturedRestorable, Required, Forbidden)
        }
        "T06" => (&[LocalSession], IrreversibleLocal, Optional, Forbidden),
        "T07" | "T08" | "T16" | "D07" | "D10" | "D12" | "D18" | "D21" | "D23" | "D24" | "F06"
        | "F10" | "F16" | "F17" | "F18" | "C20" => {
            (&[LocalSession], IrreversibleLocal, Required, Forbidden)
        }
        "T09" | "D13" => (&[LocalSession], IrreversibleLocal, Required, Optional),
        "C08" | "C11" | "C12" | "C13" | "C14" | "C18" | "C19" | "C30" | "C31" | "C35" | "D27"
        | "T10" | "F07" | "F08" | "F09" => (&[Project], IrreversibleLocal, Optional, Forbidden),
        "C22" | "C23" | "C24" | "C25" | "C26" | "C27" | "C28" | "D31" | "D32" | "D33" | "D34"
        | "D35" | "D37" | "C02" | "C06" | "D41" => {
            (&[System], IrreversibleLocal, Optional, Forbidden)
        }
        "C03" | "C04" | "C05" | "C07" | "T20" => (&[System], IrreversibleLocal, Optional, Required),
        "T19" => (&[Clipboard], IrreversibleLocal, Required, Forbidden),
        "C10" => (&[Remote], IrreversibleRemote, Optional, Required),
        "T17" => (&[Project, System], IrreversibleLocal, Required, Forbidden),
        "C37" | "D08" | "D22" | "T14" => (
            &[LocalSession, Project, System, Remote],
            RuntimeUnknown,
            Required,
            Optional,
        ),
        "C38" => (
            &[Project, System, Remote],
            RuntimeUnknown,
            Optional,
            Forbidden,
        ),
        _ => return Err(ManagementEffectStoreError::InvalidInventoryContract),
    };
    if !permissions.contains(&request.scope.permission)
        || request.rewind != rewind
        || !session.accepts(&request.scope.session)
        || !account.accepts(&request.scope.account_binding)
    {
        return Err(ManagementEffectStoreError::InvalidInventoryContract);
    }
    Ok(())
}

#[cfg(any(unix, all(test, windows)))]
fn validate_evidence_hash(
    evidence_hash: ManagementEffectHashV1,
) -> Result<(), ManagementEffectStoreError> {
    if evidence_hash.0.0 == [0; HASH_BYTES] {
        Err(ManagementEffectStoreError::InvalidValue)
    } else {
        Ok(())
    }
}

#[cfg(any(unix, all(test, windows)))]
fn validate_limits(limits: ManagementEffectLimitsV1) -> Result<(), ManagementEffectStoreError> {
    if limits.max_journal_bytes == 0
        || limits.max_records == 0
        || limits.max_effects == 0
        || limits.max_record_bytes == 0
        || limits.max_record_bytes as u64 > limits.max_journal_bytes
    {
        return Err(ManagementEffectStoreError::StoreFull);
    }
    Ok(())
}

#[cfg(unix)]
fn canonical_project_root(project_root: &Path) -> Result<PathBuf, ManagementEffectStoreError> {
    let resolved = crate::project::resolve_root(project_root);
    Ok(fs::canonicalize(resolved)?)
}

#[cfg(unix)]
fn ensure_private_directory(path: &Path) -> Result<(), ManagementEffectStoreError> {
    match fs::symlink_metadata(path) {
        Ok(_) => validate_private_directory(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let mut builder = fs::DirBuilder::new();
            builder.mode(0o700);
            match builder.create(path) {
                Ok(()) => {
                    sync_directory(
                        path.parent()
                            .ok_or(ManagementEffectStoreError::InvalidStore)?,
                    )?;
                    validate_private_directory(path)
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    validate_private_directory(path)
                }
                Err(error) => Err(error.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn ensure_private_file(path: &Path) -> Result<(), ManagementEffectStoreError> {
    match fs::symlink_metadata(path) {
        Ok(_) => validate_private_file(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            match OpenOptions::new()
                .create_new(true)
                .read(true)
                .write(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
                .open(path)
            {
                Ok(file) => {
                    file.sync_all()?;
                    sync_directory(
                        path.parent()
                            .ok_or(ManagementEffectStoreError::InvalidStore)?,
                    )?;
                    validate_private_file(path)
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    validate_private_file(path)
                }
                Err(error) => Err(error.into()),
            }
        }
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn validate_private_directory(path: &Path) -> Result<(), ManagementEffectStoreError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_dir()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o077 != 0
    {
        return Err(ManagementEffectStoreError::InvalidStore);
    }
    Ok(())
}

#[cfg(unix)]
fn validate_private_file(path: &Path) -> Result<(), ManagementEffectStoreError> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o177 != 0
        || metadata.nlink() != 1
    {
        return Err(ManagementEffectStoreError::InvalidStore);
    }
    Ok(())
}

#[cfg(unix)]
fn open_journal(path: &Path, write: bool) -> Result<File, ManagementEffectStoreError> {
    validate_private_file(path)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(write)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = options.open(path)?;
    let metadata = file.metadata()?;
    if !metadata.is_file()
        || metadata.uid() != unsafe { libc::geteuid() }
        || metadata.mode() & 0o177 != 0
        || metadata.nlink() != 1
    {
        return Err(ManagementEffectStoreError::InvalidStore);
    }
    Ok(file)
}

#[cfg(unix)]
fn read_bounded(path: &Path, max: u64) -> Result<Vec<u8>, ManagementEffectStoreError> {
    let file = open_journal(path, false)?;
    let length = file.metadata()?.len();
    if length > max {
        return Err(ManagementEffectStoreError::StoreFull);
    }
    let mut bytes = Vec::with_capacity(length as usize);
    file.take(max + 1).read_to_end(&mut bytes)?;
    if bytes.len() as u64 > max {
        return Err(ManagementEffectStoreError::StoreFull);
    }
    Ok(bytes)
}

#[cfg(any(unix, all(test, windows)))]
fn complete_prefix_len(bytes: &[u8]) -> usize {
    if bytes.is_empty() || bytes.last() == Some(&b'\n') {
        bytes.len()
    } else {
        bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map_or(0, |index| index + 1)
    }
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), ManagementEffectStoreError> {
    File::open(path)?.sync_all()?;
    Ok(())
}

impl ManagementEffectStoreV1 {
    #[cfg(unix)]
    pub fn rewind_checkpoint_for_project_at_state_dir(
        project: &Path,
        state_dir: &Path,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<ManagementRewindCheckpointV1, ManagementEffectStoreError> {
        let canonical = canonical_project_root(project)?;
        let project_scope =
            ManagementEffectHashV1::digest(canonical.as_os_str().as_encoded_bytes());
        match Self::inspect_open_at_state_dir(project, state_dir, lease)? {
            ManagementEffectInspectOpenV1::Absent => Ok(empty_rewind_checkpoint(project_scope)),
            ManagementEffectInspectOpenV1::Present(inspected) => {
                inspected.rewind_checkpoint_if_quiescent()
            }
        }
    }

    /// Historical proof only. A running input may already hold an activity
    /// lease; it must not reacquire the exclusive project writer lease.
    /// Shared activity does not authorize capture, restore or any mutation.
    #[cfg(unix)]
    pub fn verify_rewind_crossing_for_project_at_state_dir_with_activity(
        project: &Path,
        state_dir: &Path,
        lease: &crate::project_mutation::ProjectActivityLeaseV1,
        from: &ManagementRewindCheckpointV1,
        through: &ManagementRewindCheckpointV1,
    ) -> Result<ManagementRewindCrossingV1, ManagementEffectStoreError> {
        let canonical = canonical_project_root(project)?;
        let scope = ManagementEffectHashV1::digest(canonical.as_os_str().as_encoded_bytes());
        match Self::inspect_named_at_lease_identity(
            state_dir,
            scope,
            (lease.store_root(), lease.store_identity()),
            ManagementEffectLimitsV1::default(),
            false,
            STORE_NAME,
        )? {
            ManagementEffectInspectOpenV1::Absent => {
                let empty = empty_rewind_checkpoint(scope);
                if *from != empty || *through != empty {
                    return Err(ManagementEffectStoreError::CorruptJournal);
                }
                Ok(ManagementRewindCrossingV1::Clear)
            }
            ManagementEffectInspectOpenV1::Present(inspected) => {
                inspected.verify_rewind_crossing(from, through, false)
            }
        }
    }

    #[cfg(unix)]
    pub fn verify_rewind_crossing_for_project_at_state_dir(
        project: &Path,
        state_dir: &Path,
        lease: &ProjectMutationLeaseV1,
        from: &ManagementRewindCheckpointV1,
        through: &ManagementRewindCheckpointV1,
        require_current_head: bool,
    ) -> Result<ManagementRewindCrossingV1, ManagementEffectStoreError> {
        let canonical = canonical_project_root(project)?;
        let project_scope =
            ManagementEffectHashV1::digest(canonical.as_os_str().as_encoded_bytes());
        match Self::inspect_open_at_state_dir(project, state_dir, lease)? {
            ManagementEffectInspectOpenV1::Absent => {
                let empty = empty_rewind_checkpoint(project_scope);
                if *from != empty || *through != empty {
                    return Err(ManagementEffectStoreError::CorruptJournal);
                }
                Ok(ManagementRewindCrossingV1::Clear)
            }
            ManagementEffectInspectOpenV1::Present(inspected) => {
                inspected.verify_rewind_crossing(from, through, require_current_head)
            }
        }
    }
}
#[cfg(unix)]
fn empty_rewind_checkpoint(project: ManagementEffectHashV1) -> ManagementRewindCheckpointV1 {
    ManagementRewindCheckpointV1 {
        version: 1,
        project,
        cursor: ManagementEffectCursorV1 {
            generation: 0,
            chain_hash: ManagementEffectHashV1::zero(),
            sequence: 0,
            journal_hash: ManagementEffectHashV1::zero(),
        },
    }
}
impl ManagementEffectInspectV1 {
    #[cfg(unix)]
    pub fn rewind_checkpoint_if_quiescent(
        &self,
    ) -> Result<ManagementRewindCheckpointV1, ManagementEffectStoreError> {
        self.verify_pins()?;
        if self.tail != ManagementEffectTailV1::Complete
            || self
                .snapshots
                .values()
                .any(|effect| !effect.state.is_terminal())
        {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        Ok(ManagementRewindCheckpointV1 {
            version: 1,
            project: self.store.project_scope,
            cursor: self.cursor(),
        })
    }

    #[cfg(unix)]
    pub fn verify_rewind_crossing(
        &self,
        from: &ManagementRewindCheckpointV1,
        through: &ManagementRewindCheckpointV1,
        require_current_head: bool,
    ) -> Result<ManagementRewindCrossingV1, ManagementEffectStoreError> {
        self.verify_pins()?;
        if self.tail != ManagementEffectTailV1::Complete
            || from.version != 1
            || through.version != 1
            || from.project != self.store.project_scope
            || through.project != self.store.project_scope
            || (from.cursor.generation, from.cursor.sequence)
                > (through.cursor.generation, through.cursor.sequence)
            || (require_current_head && through.cursor != self.cursor())
        {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        let chain =
            self.store
                .verified_generations_reading(|name, max| match self.files.get(name) {
                    Some(file) if file.bytes.len() as u64 <= max => Ok(Some(file.bytes.clone())),
                    Some(_) => Err(ManagementEffectStoreError::StoreFull),
                    None => Ok(None),
                })?;
        let mut prepared_ids = std::collections::BTreeSet::new();
        for active in &chain {
            let name = if active.number == 0 {
                JOURNAL_NAME.to_owned()
            } else {
                format!("generations/{}/{JOURNAL_NAME}", active.number)
            };
            let file = self
                .files
                .get(&name)
                .ok_or(ManagementEffectStoreError::CorruptJournal)?;
            let state = self.store.parse_journal_bytes(active, &file.bytes)?;
            for event in state.events.values() {
                if matches!(event.event, ManagementEffectEventV1::Prepared { .. })
                    && !prepared_ids.insert(event.event.operation_id())
                {
                    return Err(ManagementEffectStoreError::CorruptJournal);
                }
            }
        }
        for checkpoint in [from, through] {
            let active = chain
                .iter()
                .find(|active| active.number == checkpoint.cursor.generation)
                .ok_or(ManagementEffectStoreError::CorruptJournal)?;
            if checkpoint.cursor.chain_hash != active.chain_hash {
                return Err(ManagementEffectStoreError::CorruptJournal);
            }
            let name = if active.number == 0 {
                JOURNAL_NAME.to_owned()
            } else {
                format!("generations/{}/{JOURNAL_NAME}", active.number)
            };
            let file = self
                .files
                .get(&name)
                .ok_or(ManagementEffectStoreError::CorruptJournal)?;
            let full = self.store.parse_journal_bytes(active, &file.bytes)?;
            let mut prefix = JournalStateV1 {
                generation: active.number,
                chain_hash: active.chain_hash,
                effects: active.pending.clone(),
                ..Default::default()
            };
            let count = usize::try_from(checkpoint.cursor.sequence)
                .map_err(|_| ManagementEffectStoreError::CorruptJournal)?;
            for event in full.events.values().take(count) {
                prefix.apply_event(event.evidence.sequence, event.event.clone())?;
                prefix.sequence = event.evidence.sequence;
                prefix.journal_hash = event.evidence.event_hash;
            }
            if prefix.cursor() != checkpoint.cursor
                || prefix
                    .effects
                    .values()
                    .any(|effect| !effect.state.is_terminal())
            {
                return Err(ManagementEffectStoreError::CorruptJournal);
            }
        }
        // The entire project journal participates, including session=None.
        let lower = (from.cursor.generation, from.cursor.sequence);
        let upper = (through.cursor.generation, through.cursor.sequence);
        for (position, verified) in &self.events {
            if *position <= lower
                || *position > upper
                || !matches!(
                    verified.event,
                    ManagementEffectEventV1::EffectStarted { .. }
                )
            {
                continue;
            }
            let id = verified.event.operation_id();
            let effect = self
                .snapshots
                .get(&id)
                .ok_or(ManagementEffectStoreError::CorruptJournal)?;
            if effect.request.rewind != ManagementRewindDispositionV1::CapturedRestorable {
                return Ok(ManagementRewindCrossingV1::Blocked {
                    operation_id: id,
                    inventory: effect.request.inventory,
                    rewind: effect.request.rewind,
                    state: effect.state.clone(),
                });
            }
        }
        self.verify_pins()?;
        Ok(ManagementRewindCrossingV1::Clear)
    }
}

#[cfg(not(unix))]
fn unsupported() -> ManagementEffectStoreError {
    ManagementEffectStoreError::Io(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "management effect storage is unavailable on this platform",
    ))
}

#[cfg(not(unix))]
impl ManagementEffectStoreV1 {
    #[cfg(not(all(test, windows)))]
    pub fn inspect_open_at_state_dir(
        _project_root: &Path,
        _state_dir: &Path,
        _lease: &ProjectMutationLeaseV1,
    ) -> Result<ManagementEffectInspectOpenV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub(crate) fn inspect_open_with_read_lease(
        _project: &Path,
        _state: &Path,
        _lease: crate::project_mutation::ProjectReadLeaseV1<'_>,
    ) -> Result<ManagementEffectInspectOpenV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    pub fn inspect_retired_for_migration_at_state_dir(
        _project_root: &Path,
        _state_dir: &Path,
        _migration_id: ManagementEffectHashV1,
        _lease: &ProjectMutationLeaseV1,
    ) -> Result<ManagementEffectInspectOpenV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(test)]
    pub(crate) fn inspect_migration_history(
        _state_dir: &Path,
        _lease: &ProjectMutationLeaseV1,
        _scope: Option<ManagementEffectHashV1>,
    ) -> Result<ManagementEffectInspectOpenV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn open(
        _project_root: &Path,
        _lease: &ProjectMutationLeaseV1,
    ) -> Result<Self, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn open_at_state_dir(
        _project_root: &Path,
        _state_dir: &Path,
        _lease: &ProjectMutationLeaseV1,
    ) -> Result<Self, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn capacity(&self) -> Result<ManagementEffectCapacityV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn cursor(&self) -> Result<ManagementEffectCursorV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn effects(&self) -> Result<Vec<ManagementEffectSnapshotV1>, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn effects_page(
        &self,
        _expected: ManagementEffectCursorV1,
        _archive_or_active: u64,
        _after: Option<ManagementOperationIdV1>,
        _limit: usize,
    ) -> Result<Vec<ManagementEffectSnapshotV1>, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn effect(
        &self,
        _operation_id: ManagementOperationIdV1,
    ) -> Result<Option<ManagementEffectSnapshotV1>, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn event_evidence(
        &self,
        _operation_id: ManagementOperationIdV1,
        _expected_evidence: ManagementEffectEventEvidenceV1,
        _expected_event: &ManagementEffectEventV1,
    ) -> Result<Option<VerifiedManagementEffectEventV1>, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn latest_event_evidence(
        &self,
        _operation_id: ManagementOperationIdV1,
        _expected_snapshot: &ManagementEffectSnapshotV1,
    ) -> Result<Option<VerifiedManagementEffectEventV1>, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn blocker(
        &self,
    ) -> Result<Option<ManagementEffectSnapshotV1>, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn prepared_effect(
        &self,
        _operation_id: ManagementOperationIdV1,
    ) -> Result<Option<PreparedManagementEffectV1>, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn unknown_effect(
        &self,
        _operation_id: ManagementOperationIdV1,
    ) -> Result<Option<UnknownManagementEffectV1>, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn prepare(
        &self,
        _lease: &ProjectMutationLeaseV1,
        _expected: ManagementEffectCursorV1,
        _request: PrepareManagementEffectV1,
    ) -> Result<PrepareManagementEffectOutcomeV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    pub fn prepare_runtime(
        &self,
        _lease: &ProjectActivityLeaseV1,
        _expected: ManagementEffectCursorV1,
        _request: PrepareManagementEffectV1,
    ) -> Result<PrepareManagementEffectOutcomeV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn deny(
        &self,
        _lease: &ProjectMutationLeaseV1,
        _effect: &PreparedManagementEffectV1,
        _reason: ManagementDenialReasonV1,
    ) -> Result<DeniedManagementEffectV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    pub fn deny_runtime(
        &self,
        _lease: &ProjectActivityLeaseV1,
        _effect: &PreparedManagementEffectV1,
        _reason: ManagementDenialReasonV1,
    ) -> Result<DeniedManagementEffectV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn start(
        &self,
        _lease: &ProjectMutationLeaseV1,
        _effect: &PreparedManagementEffectV1,
    ) -> Result<StartedManagementEffectV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    pub fn start_runtime(
        &self,
        _lease: &ProjectActivityLeaseV1,
        _effect: &PreparedManagementEffectV1,
    ) -> Result<StartedManagementEffectV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn complete_started(
        &self,
        _lease: &ProjectMutationLeaseV1,
        _effect: &StartedManagementEffectV1,
        _evidence_hash: ManagementEffectHashV1,
    ) -> Result<CompletedManagementEffectV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    pub fn complete_started_runtime(
        &self,
        _lease: &ProjectActivityLeaseV1,
        _effect: &StartedManagementEffectV1,
        _evidence_hash: ManagementEffectHashV1,
    ) -> Result<CompletedManagementEffectV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn mark_outcome_unknown(
        &self,
        _lease: &ProjectMutationLeaseV1,
        _effect: &StartedManagementEffectV1,
        _reason: ManagementUnknownReasonV1,
    ) -> Result<UnknownManagementEffectV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    pub fn mark_outcome_unknown_runtime(
        &self,
        _lease: &ProjectActivityLeaseV1,
        _effect: &StartedManagementEffectV1,
        _reason: ManagementUnknownReasonV1,
    ) -> Result<UnknownManagementEffectV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn complete_unknown(
        &self,
        _lease: &ProjectMutationLeaseV1,
        _effect: &UnknownManagementEffectV1,
        _evidence_hash: ManagementEffectHashV1,
    ) -> Result<CompletedManagementEffectV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn reconcile_abandon(
        &self,
        _lease: &ProjectMutationLeaseV1,
        _effect: &UnknownManagementEffectV1,
        _evidence_hash: ManagementEffectHashV1,
    ) -> Result<AbandonedManagementEffectV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    pub fn rewind_checkpoint_for_project_at_state_dir(
        _project: &Path,
        _state_dir: &Path,
        _lease: &ProjectMutationLeaseV1,
    ) -> Result<ManagementRewindCheckpointV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    pub fn verify_rewind_crossing_for_project_at_state_dir_with_activity(
        _project: &Path,
        _state_dir: &Path,
        _lease: &crate::project_mutation::ProjectActivityLeaseV1,
        _from: &ManagementRewindCheckpointV1,
        _through: &ManagementRewindCheckpointV1,
    ) -> Result<ManagementRewindCrossingV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    pub fn verify_rewind_crossing_for_project_at_state_dir(
        _project: &Path,
        _state_dir: &Path,
        _lease: &ProjectMutationLeaseV1,
        _from: &ManagementRewindCheckpointV1,
        _through: &ManagementRewindCheckpointV1,
        _require_current_head: bool,
    ) -> Result<ManagementRewindCrossingV1, ManagementEffectStoreError> {
        Err(unsupported())
    }
}

#[cfg(not(unix))]
impl ManagementEffectInspectV1 {
    pub(crate) fn fresh_admission_limits(
        &self,
    ) -> Result<(u64, u64, u64), ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn root_identity(&self) -> Result<(u64, u64), ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn latest_event_evidence(
        &self,
        _operation_id: ManagementOperationIdV1,
        _expected_snapshot: &ManagementEffectSnapshotV1,
    ) -> Result<Option<VerifiedManagementEffectEventV1>, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub fn event_evidence(
        &self,
        _operation_id: ManagementOperationIdV1,
        _expected: ManagementEffectEventEvidenceV1,
        _event: &ManagementEffectEventV1,
    ) -> Result<Option<VerifiedManagementEffectEventV1>, ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(not(all(test, windows)))]
    pub(crate) fn verify_read_snapshot(&self) -> Result<(), ManagementEffectStoreError> {
        Err(unsupported())
    }

    #[cfg(test)]
    pub(crate) fn rebind_retired_for_migration(
        self,
        _state_directory: &File,
        _migration_id: ManagementEffectHashV1,
    ) -> Result<Self, ManagementEffectStoreError> {
        Err(unsupported())
    }

    pub fn rewind_checkpoint_if_quiescent(
        &self,
    ) -> Result<ManagementRewindCheckpointV1, ManagementEffectStoreError> {
        Err(unsupported())
    }

    pub fn verify_rewind_crossing(
        &self,
        _from: &ManagementRewindCheckpointV1,
        _through: &ManagementRewindCheckpointV1,
        _require_current_head: bool,
    ) -> Result<ManagementRewindCrossingV1, ManagementEffectStoreError> {
        Err(unsupported())
    }
}

#[cfg(any(unix, all(test, windows)))]
impl ManagementEffectStoreV1 {
    fn m_open(
        &self,
        path: &Path,
        write: bool,
    ) -> Result<ManagementJournalFileV1, ManagementEffectStoreError> {
        #[cfg(unix)]
        {
            open_journal(path, write)
        }
        #[cfg(all(test, windows))]
        {
            self.w_open(path, write)
        }
    }
    fn m_read(&self, path: &Path, max: u64) -> Result<Vec<u8>, ManagementEffectStoreError> {
        #[cfg(unix)]
        {
            read_bounded(path, max)
        }
        #[cfg(all(test, windows))]
        {
            self.w_read(path, max)
        }
    }
    fn m_bounded(&self, path: &Path, max: u64) -> Result<Vec<u8>, ManagementEffectStoreError> {
        self.m_read(path, max).map_err(|error| match error {
            ManagementEffectStoreError::StoreFull => ManagementEffectStoreError::CorruptJournal,
            other => other,
        })
    }
    fn m_validate_directory(&self, path: &Path) -> Result<(), ManagementEffectStoreError> {
        #[cfg(unix)]
        {
            validate_private_directory(path)
        }
        #[cfg(all(test, windows))]
        {
            self.w_directory(path)
        }
    }
    fn m_validate_file(&self, path: &Path) -> Result<(), ManagementEffectStoreError> {
        #[cfg(unix)]
        {
            validate_private_file(path)
        }
        #[cfg(all(test, windows))]
        {
            self.w_open(path, false).map(|_| ())
        }
    }
    fn m_ensure_directory(&self, path: &Path) -> Result<(), ManagementEffectStoreError> {
        #[cfg(unix)]
        {
            ensure_private_directory(path)
        }
        #[cfg(all(test, windows))]
        {
            self.w_ensure(path)
        }
    }
    fn m_length(&self, path: impl AsRef<Path>) -> Result<u64, ManagementEffectStoreError> {
        #[cfg(unix)]
        {
            Ok(fs::symlink_metadata(path)?.len())
        }
        #[cfg(all(test, windows))]
        {
            Ok(self.w_open(path.as_ref(), false)?.metadata()?.len())
        }
    }
    fn m_sync(&self, path: &Path) -> Result<(), ManagementEffectStoreError> {
        #[cfg(unix)]
        {
            sync_directory(path)
        }
        #[cfg(all(test, windows))]
        {
            self.w_sync(path)
        }
    }
    fn m_write_new(&self, path: &Path, bytes: &[u8]) -> Result<(), ManagementEffectStoreError> {
        #[cfg(unix)]
        {
            generation::write_new(path, bytes)
        }
        #[cfg(all(test, windows))]
        {
            self.w_write_new(path, bytes)
        }
    }
    fn m_directory_present(&self, path: &Path) -> Result<bool, ManagementEffectStoreError> {
        #[cfg(unix)]
        {
            match fs::symlink_metadata(path) {
                Ok(_) => Ok(true),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(error) => Err(error.into()),
            }
        }
        #[cfg(all(test, windows))]
        {
            self.w_present(path)
        }
    }
    fn m_rename_directory(
        &self,
        source: &Path,
        target: &Path,
    ) -> Result<(), ManagementEffectStoreError> {
        #[cfg(unix)]
        {
            fs::rename(source, target).map_err(Into::into)
        }
        #[cfg(all(test, windows))]
        {
            self.w_rename(source, target)
        }
    }
    fn m_replace(&self, source: &Path, target: &Path) -> Result<(), ManagementEffectStoreError> {
        #[cfg(unix)]
        {
            fs::rename(source, target).map_err(Into::into)
        }
        #[cfg(all(test, windows))]
        {
            self.w_replace(source, target)
        }
    }
}

#[cfg(all(test, windows))]
mod windows_tests;
