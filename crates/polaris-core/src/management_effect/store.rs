//! Lease-bound management-effect journal storage and validation.

use super::*;
use crate::project_mutation::{ProjectActivityLeaseV1, ProjectMutationLeaseV1};
#[path = "generation.rs"]
mod generation;
use generation::ActiveGenerationV1;
use serde::{Deserialize, Serialize};
use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

// Private capability: public project mutators continue to require EX leases.
#[derive(Clone, Copy)]
enum JournalLeaseV1<'a> {
    Mutation(&'a ProjectMutationLeaseV1),
    Runtime(&'a ProjectActivityLeaseV1),
}

const JOURNAL_VERSION: u32 = 1;
const STORE_NAME: &str = "management-effects-v1";
const JOURNAL_NAME: &str = "journal.v1.jsonl";
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
}

#[derive(Debug)]
pub enum ManagementEffectInspectOpenV1 {
    Absent,
    Present(ManagementEffectInspectV1),
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

#[derive(Debug)]
struct PinnedManagementFileV1 {
    parent: File,
    name: CString,
    file: File,
    evidence: ManagementEffectInspectedFileV1,
    bytes: Vec<u8>,
}

#[derive(Debug)]
pub struct ManagementEffectInspectV1 {
    store: ManagementEffectStoreV1,
    root_directory: File,
    _owner_guard: File,
    files: BTreeMap<String, PinnedManagementFileV1>,
    directories: BTreeMap<String, (u64, u64)>,
    state: JournalStateV1,
    snapshots: BTreeMap<ManagementOperationIdV1, ManagementEffectSnapshotV1>,
    events: BTreeMap<(u64, u64), VerifiedManagementEffectEventV1>,
    tail: ManagementEffectTailV1,
    complete_prefix_hash: ManagementEffectHashV1,
}

/// Only the shared owner can construct a token after the full intent join.
#[derive(Debug)]
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
            (0, 0)
        };
        Ok((
            limits.max_record_bytes as u64,
            (limits.max_records as u64).saturating_sub(records),
            limits.max_journal_bytes.saturating_sub(bytes),
        ))
    }

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

    fn verify_pins(&self) -> Result<(), ManagementEffectStoreError> {
        let root =
            crate::project_mutation::open_directory_nofollow(&self.store.root)?.metadata()?;
        let pinned = self.root_directory.metadata()?;
        if root.uid() != unsafe { libc::geteuid() } || root.mode() & 0o7777 != 0o700 {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        if (root.dev(), root.ino()) != (pinned.dev(), pinned.ino()) {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        for file in self.files.values() {
            verify_management_file(file)?;
        }
        let fresh = management_inventory(&self.root_directory, self.store.limits)?;
        if fresh.directories != self.directories
            || fresh.files.len() != self.files.len()
            || fresh.files.iter().any(|(name, file)| {
                self.files
                    .get(name)
                    .is_none_or(|old| old.evidence != file.evidence)
            })
        {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        Ok(())
    }
}

impl ManagementEffectStoreV1 {
    /// Creation is authorized only by the owner's already durable generation zero.
    /// Existing bytes go through inspect/admit; normal open's repairs are avoided.
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

    fn inspect_at(
        state_dir: &Path,
        project_scope: ManagementEffectHashV1,
        lease: &ProjectMutationLeaseV1,
        limits: ManagementEffectLimitsV1,
    ) -> Result<ManagementEffectInspectOpenV1, ManagementEffectStoreError> {
        let state_directory = crate::project_mutation::open_directory_nofollow(state_dir)?;
        let state_meta = state_directory.metadata()?;
        if state_meta.uid() != unsafe { libc::geteuid() } || state_meta.mode() & 0o7077 != 0 {
            return Err(ManagementEffectStoreError::InvalidStore);
        }
        let expected_lease_root = state_dir.join("rewind-workspace");
        if lease.store_root() != expected_lease_root {
            return Err(ManagementEffectStoreError::WrongProjectLease);
        }
        let lock_root =
            crate::project_mutation::open_directory_nofollow(&expected_lease_root)?.metadata()?;
        if lease.store_identity() != (lock_root.dev(), lock_root.ino()) {
            return Err(ManagementEffectStoreError::WrongProjectLease);
        }
        let root_directory = match management_child_directory(&state_directory, STORE_NAME) {
            Err(ManagementEffectStoreError::Io(error))
                if error.kind() == std::io::ErrorKind::NotFound =>
            {
                return Ok(ManagementEffectInspectOpenV1::Absent);
            }
            other => other?,
        };
        let store = Self {
            state_dir: state_dir.to_owned(),
            root: state_dir.join(STORE_NAME),
            journal_path: state_dir.join(STORE_NAME).join(JOURNAL_NAME),
            expected_lease_root,
            project_scope,
            limits,
        };
        // The existing lock is opened without O_CREAT and remains held until
        // inspection is discarded or an explicitly joined recovery finishes.
        let owner = pin_management_file(&root_directory, OWNER_LOCK_NAME, OWNER_LOCK_NAME, 4096)?;
        owner.file.lock()?;
        let inventory = management_inventory(&root_directory, limits)?;
        let files = inventory.files;
        let read =
            |relative: &str, max: u64| -> Result<Option<Vec<u8>>, ManagementEffectStoreError> {
                match files.get(relative) {
                    Some(file) if file.bytes.len() as u64 <= max => Ok(Some(file.bytes.clone())),
                    Some(_) => Err(ManagementEffectStoreError::StoreFull),
                    None => Ok(None),
                }
            };
        let chain = store.verified_generations_reading(read)?;
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
            let state = store.parse_journal_bytes(active, &file.bytes[..prefix])?;
            for (key, effect) in &state.effects {
                snapshots.insert(*key, effect.clone());
            }
            for verified in state.events.values() {
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
        };
        inspected.verify_pins()?;
        inspected.store.validate_inspected_seal_layout(&inspected)?;
        for snapshot in inspected.snapshots.values() {
            inspected
                .latest_event_evidence(snapshot.request.operation_id, snapshot)?
                .ok_or(ManagementEffectStoreError::CorruptJournal)?;
        }
        Ok(ManagementEffectInspectOpenV1::Present(inspected))
    }
    /// Opens the project store while holding the one shared mutation lease.
    ///
    /// Opening repairs only an incomplete final write. A complete invalid
    /// record is corruption. Any effect that had started before this process
    /// opened the store is durably advanced to `OutcomeUnknown`.
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

    pub fn capacity(&self) -> Result<ManagementEffectCapacityV1, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        let state = self.load()?;
        let current_journal_bytes =
            fs::symlink_metadata(self.journal_for_generation(state.generation))?.len();
        let mut retained_archive_bytes = 0_u64;
        for number in 1..=state.generation {
            let archive = self
                .root
                .join("generations")
                .join(number.to_string())
                .join("archive.v1.json");
            retained_archive_bytes = retained_archive_bytes
                .checked_add(fs::symlink_metadata(archive)?.len())
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
    pub(crate) fn test_fail_seal_once_at(&self, point: u8) {
        generation::fail_seal_once_at(point);
    }

    #[cfg(test)]
    pub(crate) fn test_reset_file_sequence_for_restart(&self) {
        generation::reset_file_sequence_for_restart();
    }

    pub fn cursor(&self) -> Result<ManagementEffectCursorV1, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        let state = self.load()?;
        Ok(state.cursor())
    }

    pub fn effects(&self) -> Result<Vec<ManagementEffectSnapshotV1>, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        Ok(self.load()?.effects.into_values().collect())
    }

    /// Enumerates one bounded page in a sealed archive (1..=generation), or
    /// in the active generation (generation + 1). The caller must retain the
    /// cursor and pass the last returned operation id as `after` for the next
    /// page. A rollover or append invalidates the cursor.
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

    pub fn event_evidence(
        &self,
        operation_id: ManagementOperationIdV1,
        expected_evidence: ManagementEffectEventEvidenceV1,
        expected_event: &ManagementEffectEventV1,
    ) -> Result<Option<VerifiedManagementEffectEventV1>, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        self.event_evidence_unlocked(operation_id, expected_evidence, expected_event)
    }

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

    pub fn latest_event_evidence(
        &self,
        operation_id: ManagementOperationIdV1,
        expected_snapshot: &ManagementEffectSnapshotV1,
    ) -> Result<Option<VerifiedManagementEffectEventV1>, ManagementEffectStoreError> {
        let _owner = self.lock_owner()?;
        self.latest_event_evidence_unlocked(operation_id, expected_snapshot)
    }

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

    /// Re-acquires only the reconciliation capability for an unresolved
    /// outcome. It cannot be used to restart the original side effect.
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

    pub fn prepare(
        &self,
        lease: &ProjectMutationLeaseV1,
        expected: ManagementEffectCursorV1,
        request: PrepareManagementEffectV1,
    ) -> Result<PrepareManagementEffectOutcomeV1, ManagementEffectStoreError> {
        self.prepare_with_lease(JournalLeaseV1::Mutation(lease), expected, request)
    }

    /// Restricts shared-activity journal writes to runtime-local session controls.
    pub fn prepare_runtime(
        &self,
        lease: &ProjectActivityLeaseV1,
        expected: ManagementEffectCursorV1,
        request: PrepareManagementEffectV1,
    ) -> Result<PrepareManagementEffectOutcomeV1, ManagementEffectStoreError> {
        self.prepare_with_lease(JournalLeaseV1::Runtime(lease), expected, request)
    }

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
                        verified_event: self
                            .latest_event_for_operation_unlocked(request.operation_id)?,
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
                verified_event: self.latest_event_for_operation_unlocked(request.operation_id)?,
                request,
                generation: state.generation,
                chain_hash: state.chain_hash,
                last_sequence: sequence.evidence.sequence,
            },
        ))
    }

    pub fn deny(
        &self,
        lease: &ProjectMutationLeaseV1,
        effect: &PreparedManagementEffectV1,
        reason: ManagementDenialReasonV1,
    ) -> Result<DeniedManagementEffectV1, ManagementEffectStoreError> {
        self.deny_with_lease(JournalLeaseV1::Mutation(lease), effect, reason)
    }

    /// Restricts shared-activity journal writes to runtime-local session controls.
    pub fn deny_runtime(
        &self,
        lease: &ProjectActivityLeaseV1,
        effect: &PreparedManagementEffectV1,
        reason: ManagementDenialReasonV1,
    ) -> Result<DeniedManagementEffectV1, ManagementEffectStoreError> {
        self.deny_with_lease(JournalLeaseV1::Runtime(lease), effect, reason)
    }

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

    pub fn start(
        &self,
        lease: &ProjectMutationLeaseV1,
        effect: &PreparedManagementEffectV1,
    ) -> Result<StartedManagementEffectV1, ManagementEffectStoreError> {
        self.start_with_lease(JournalLeaseV1::Mutation(lease), effect)
    }

    /// Restricts shared-activity journal writes to runtime-local session controls.
    pub fn start_runtime(
        &self,
        lease: &ProjectActivityLeaseV1,
        effect: &PreparedManagementEffectV1,
    ) -> Result<StartedManagementEffectV1, ManagementEffectStoreError> {
        self.start_with_lease(JournalLeaseV1::Runtime(lease), effect)
    }

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

    pub fn mark_outcome_unknown(
        &self,
        lease: &ProjectMutationLeaseV1,
        effect: &StartedManagementEffectV1,
        reason: ManagementUnknownReasonV1,
    ) -> Result<UnknownManagementEffectV1, ManagementEffectStoreError> {
        self.mark_outcome_unknown_with_lease(JournalLeaseV1::Mutation(lease), effect, reason)
    }

    /// Restricts shared-activity journal writes to runtime-local session controls.
    pub fn mark_outcome_unknown_runtime(
        &self,
        lease: &ProjectActivityLeaseV1,
        effect: &StartedManagementEffectV1,
        reason: ManagementUnknownReasonV1,
    ) -> Result<UnknownManagementEffectV1, ManagementEffectStoreError> {
        self.mark_outcome_unknown_with_lease(JournalLeaseV1::Runtime(lease), effect, reason)
    }

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

    fn has_prepare_reserve(
        &self,
        state: &JournalStateV1,
    ) -> Result<bool, ManagementEffectStoreError> {
        let bytes = fs::symlink_metadata(self.journal_for_generation(state.generation))?.len();
        Ok(state.effects.len() < self.limits.max_effects
            && state
                .records
                .checked_add(4)
                .is_some_and(|count| count <= self.limits.max_records)
            && bytes
                .checked_add(4_u64.saturating_mul(self.limits.max_record_bytes as u64))
                .is_some_and(|length| length <= self.limits.max_journal_bytes))
    }

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
        let bytes = fs::symlink_metadata(self.journal_for_generation(state.generation))?.len();
        Ok(state
            .records
            .checked_add(needed)
            .is_some_and(|count| count <= self.limits.max_records)
            && bytes
                .checked_add((needed as u64).saturating_mul(self.limits.max_record_bytes as u64))
                .is_some_and(|length| length <= self.limits.max_journal_bytes))
    }

    fn append(
        &self,
        lease: &ProjectMutationLeaseV1,
        state: &JournalStateV1,
        event: JournalEventV1,
    ) -> Result<VerifiedManagementEffectEventV1, ManagementEffectStoreError> {
        self.append_with_lease(JournalLeaseV1::Mutation(lease), state, event)
    }

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
        let current_bytes = fs::symlink_metadata(&journal_path)?.len();
        if current_bytes.saturating_add(bytes.len() as u64) > self.limits.max_journal_bytes {
            return Err(ManagementEffectStoreError::StoreFull);
        }
        let mut file = open_journal(&journal_path, true)?;
        file.seek(SeekFrom::End(0))?;
        file.write_all(&bytes)?;
        file.sync_all()?;
        sync_directory(
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

    fn repair_torn_tail(
        &self,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<(), ManagementEffectStoreError> {
        self.check_lease(lease)?;
        let active = self.active_generation()?;
        let journal_path = active.journal_path.clone();
        let bytes = read_bounded(&journal_path, self.limits.max_journal_bytes)?;
        let valid = complete_prefix_len(&bytes);
        if valid == bytes.len() {
            return Ok(());
        }
        self.parse_journal_bytes(&active, &bytes[..valid])?;
        let mut file = open_journal(&journal_path, true)?;
        file.set_len(valid as u64)?;
        file.seek(SeekFrom::Start(valid as u64))?;
        file.sync_all()?;
        sync_directory(
            journal_path
                .parent()
                .ok_or(ManagementEffectStoreError::InvalidStore)?,
        )?;
        Ok(())
    }

    fn load(&self) -> Result<JournalStateV1, ManagementEffectStoreError> {
        validate_private_directory(&self.root)?;
        let active = self.active_generation()?;
        self.load_at(&active)
    }

    fn load_at(
        &self,
        active: &ActiveGenerationV1,
    ) -> Result<JournalStateV1, ManagementEffectStoreError> {
        validate_private_directory(
            active
                .journal_path
                .parent()
                .ok_or(ManagementEffectStoreError::InvalidStore)?,
        )?;
        validate_private_file(&active.journal_path)?;
        let bytes = read_bounded(&active.journal_path, self.limits.max_journal_bytes)?;
        if complete_prefix_len(&bytes) != bytes.len() {
            return Err(ManagementEffectStoreError::TornTail);
        }
        self.parse_journal_bytes(active, &bytes)
    }

    fn parse_journal_bytes(
        &self,
        active: &ActiveGenerationV1,
        bytes: &[u8],
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
        if state
            .effects
            .values()
            .any(|effect| validate_request_shape(self.project_scope, &effect.request).is_err())
        {
            return Err(ManagementEffectStoreError::CorruptJournal);
        }
        Ok(state)
    }

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

    fn check_lease(
        &self,
        lease: &ProjectMutationLeaseV1,
    ) -> Result<(), ManagementEffectStoreError> {
        if lease.store_root() != self.expected_lease_root {
            return Err(ManagementEffectStoreError::WrongProjectLease);
        }
        Ok(())
    }

    fn lock_owner(&self) -> Result<File, ManagementEffectStoreError> {
        let file = open_journal(&self.root.join(OWNER_LOCK_NAME), true)?;
        file.lock()?;
        Ok(file)
    }

    #[cfg(test)]
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
    pub(super) fn test_journal_path(&self) -> &Path {
        &self.journal_path
    }

    #[cfg(test)]
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

    #[cfg(test)]
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
    pub(super) fn test_inspect(
        &self,
        lease: &ProjectMutationLeaseV1,
        limits: ManagementEffectLimitsV1,
    ) -> Result<ManagementEffectInspectOpenV1, ManagementEffectStoreError> {
        Self::inspect_at(&self.state_dir, self.project_scope, lease, limits)
    }

    #[cfg(test)]
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
    if meta.dev() != entry.st_dev as u64 || meta.ino() != entry.st_ino as u64 {
        return Err(ManagementEffectStoreError::InvalidStore);
    }
    Ok(())
}

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
    if file.metadata()?.len() != meta.len() {
        return Err(ManagementEffectStoreError::InvalidStore);
    }
    Ok(data)
}

fn pin_management_file(
    parent: &File,
    name: &str,
    relative: &str,
    max: u64,
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
    };
    verify_management_file(&pinned)?;
    Ok(pinned)
}

fn verify_management_file(file: &PinnedManagementFileV1) -> Result<(), ManagementEffectStoreError> {
    verify_management_entry(&file.parent, &file.name, &file.file)?;
    let meta = file.file.metadata()?;
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
}

fn management_inventory(
    root: &File,
    limits: ManagementEffectLimitsV1,
) -> Result<ManagementInventoryV1, ManagementEffectStoreError> {
    use crate::project_mutation::directory_names_bounded;
    let mut files = BTreeMap::new();
    let mut directories = BTreeMap::new();
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
                let meta = generations.metadata()?;
                directories.insert(name.clone(), (meta.dev(), meta.ino()));
                for number in directory_names_bounded(&generations, limits.max_records)? {
                    let number = number
                        .into_string()
                        .map_err(|_| ManagementEffectStoreError::InvalidStore)?;
                    let decimal = number
                        .strip_prefix(".seal-")
                        .and_then(|n| n.strip_suffix(".v1.tmp"))
                        .unwrap_or(&number);
                    let parsed = decimal
                        .parse::<u64>()
                        .map_err(|_| ManagementEffectStoreError::CorruptJournal)?;
                    if parsed == 0 || parsed.to_string() != decimal {
                        return Err(ManagementEffectStoreError::CorruptJournal);
                    }
                    let directory = management_child_directory(&generations, &number)?;
                    let meta = directory.metadata()?;
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
                            pin_management_file(&directory, &entry, &relative, max)?,
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
        files.insert(name.clone(), pin_management_file(root, &name, &name, max)?);
    }
    Ok(ManagementInventoryV1 { files, directories })
}

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

fn validate_request_shape(
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

fn validate_new_inventory_contract(
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

fn validate_evidence_hash(
    evidence_hash: ManagementEffectHashV1,
) -> Result<(), ManagementEffectStoreError> {
    if evidence_hash.0.0 == [0; HASH_BYTES] {
        Err(ManagementEffectStoreError::InvalidValue)
    } else {
        Ok(())
    }
}

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

fn canonical_project_root(project_root: &Path) -> Result<PathBuf, ManagementEffectStoreError> {
    let resolved = crate::project::resolve_root(project_root);
    Ok(fs::canonicalize(resolved)?)
}

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

fn sync_directory(path: &Path) -> Result<(), ManagementEffectStoreError> {
    File::open(path)?.sync_all()?;
    Ok(())
}
