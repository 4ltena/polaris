//! Durable local-session workflow changes.  A started change is reconciled
//! from its descriptor and marker; it is never published a second time.

use super::{
    ManagementDenialReasonV1, ManagementEffectHashV1, ManagementEffectScopeV1,
    ManagementEffectStateV1, ManagementEffectStoreError, ManagementEffectStoreV1,
    ManagementInventoryIdV1, ManagementOperationIdV1, ManagementPermissionScopeV1,
    ManagementRewindDispositionV1, ManagementUnknownReasonV1, PrepareManagementEffectOutcomeV1,
    PrepareManagementEffectV1, PreparedManagementEffectV1, StartedManagementEffectV1,
};
use crate::{
    conversation_state::{
        ConversationSnapshot, ConversationStateV2, SavedWorkflowConfigV1, content_hash,
    },
    project_mutation::ProjectMutationLeaseV1,
    session_store::PersistedSession,
    workflow::SessionWorkflow,
};
use serde::{Deserialize, Serialize};
use std::{
    ffi::{CStr, CString},
    fs::{self, File, OpenOptions},
    io::{self, Read, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::fs::{MetadataExt, OpenOptionsExt},
    },
    path::Path,
};

const DIRECTORY: &str = "workflow-effects-v1";
const MAX_DESCRIPTOR_BYTES: usize = 16 * 1024;
const MAX_DESCRIPTORS: usize = 4096;
const MAX_DESCRIPTOR_TOTAL_BYTES: u64 = 64 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkflowEffectInventoryV1 {
    T03,
    T04,
    F15,
}

impl WorkflowEffectInventoryV1 {
    pub fn id(self) -> &'static str {
        match self {
            Self::T03 => "T03",
            Self::T04 => "T04",
            Self::F15 => "F15",
        }
    }

    fn parse(self) -> ManagementInventoryIdV1 {
        ManagementInventoryIdV1::parse(self.id()).expect("fixed workflow inventory")
    }
}

#[derive(Clone, Debug)]
pub struct WorkflowChangePreviewV1 {
    inventory: WorkflowEffectInventoryV1,
    before: ConversationSnapshot,
    candidate: ConversationStateV2,
    before_state_hash: String,
    after_state_hash: String,
    before_workflow_hash: ManagementEffectHashV1,
    after_workflow_hash: ManagementEffectHashV1,
}

impl WorkflowChangePreviewV1 {
    pub fn inventory(&self) -> WorkflowEffectInventoryV1 {
        self.inventory
    }
    pub fn session_id(&self) -> &str {
        &self.before.state.session_id
    }
    pub fn project_id(&self) -> &str {
        &self.before.state.project_id
    }
    pub fn before_generation(&self) -> u64 {
        self.before.state.generation
    }
    pub fn before_workflow(&self) -> &crate::workflow::WorkflowStateV1 {
        &self.before.state.workflow
    }
    pub fn candidate_workflow(&self) -> &crate::workflow::WorkflowStateV1 {
        &self.candidate.workflow
    }
    pub fn before_config(&self) -> Option<&crate::conversation_state::SavedWorkflowConfigV1> {
        self.before.state.workflow_config.as_ref()
    }
    pub fn candidate_config(&self) -> Option<&crate::conversation_state::SavedWorkflowConfigV1> {
        self.candidate.workflow_config.as_ref()
    }
    pub fn before_state_hash(&self) -> &str {
        &self.before_state_hash
    }
    pub fn after_state_hash(&self) -> &str {
        &self.after_state_hash
    }
}

#[derive(Clone, Debug)]
pub struct WorkflowPreparedChangeV1 {
    prepared: PreparedManagementEffectV1,
    preview: WorkflowChangePreviewV1,
}

#[derive(Debug, thiserror::Error)]
pub enum WorkflowChangeError {
    #[error("management effect: {0}")]
    Effect(#[from] ManagementEffectStoreError),
    #[error("session persistence: {0}")]
    Session(#[from] io::Error),
    #[error("workflow candidate is unchanged")]
    NoChange,
    #[error("workflow preview is stale")]
    StalePreview,
    #[error("workflow descriptor conflicts with an existing operation")]
    OperationCollision,
    #[error("workflow descriptor is missing, corrupt, or belongs to another session")]
    DescriptorMismatch,
    #[error("workflow serialization: {0}")]
    Serialization(#[from] serde_json::Error),
}

pub struct WorkflowChangeOwner<'a> {
    pub effects: &'a ManagementEffectStoreV1,
    pub lease: &'a ProjectMutationLeaseV1,
    pub state_dir: &'a Path,
}

impl WorkflowChangeOwner<'_> {
    fn validate_binding(&self) -> Result<(), WorkflowChangeError> {
        let supplied = fs::canonicalize(self.state_dir)?;
        if supplied != self.effects.state_dir()
            || fs::canonicalize(supplied.join("rewind-workspace"))? != self.lease.store_root()
        {
            return Err(WorkflowChangeError::DescriptorMismatch);
        }
        Ok(())
    }

    /// This is deliberately lease-free so a UI can show its exact target and
    /// wait for confirmation without blocking turns or another mutation.
    pub fn preview(
        session: &PersistedSession,
        inventory: WorkflowEffectInventoryV1,
        candidate: &SessionWorkflow,
    ) -> Result<WorkflowChangePreviewV1, WorkflowChangeError> {
        let before = session.snapshot()?;
        if before.state.workflow_config.is_none() {
            return Err(WorkflowChangeError::StalePreview);
        }
        let restored = SessionWorkflow::restore_with_artifacts(
            candidate.config.clone(),
            candidate.gates.clone(),
            candidate.state(),
            candidate.artifact_graph().clone(),
        )
        .map_err(io::Error::other)?;
        let mut next = before.state.clone();
        next.workflow_config = Some(SavedWorkflowConfigV1::capture(&restored.config));
        next.workflow = restored.state();
        next.gates = restored.gates.clone();
        next.workflow_artifacts = restored.artifact_graph().clone();
        if next.workflow_config == before.state.workflow_config
            && next.workflow == before.state.workflow
            && next.gates == before.state.gates
            && next.workflow_artifacts == before.state.workflow_artifacts
        {
            return Err(WorkflowChangeError::NoChange);
        }
        next.validate()?;
        let mut after = next.clone();
        after.generation = after
            .generation
            .checked_add(1)
            .ok_or_else(|| io::Error::other("Storage generation limit reached"))?;
        after.raw_commit_offset = before.raw_offset;
        after.raw_commit_hash = before.raw_hash.clone();
        after.recent_turn_ids = before.active_turns().into_iter().rev().take(10).collect();
        after.recent_turn_ids.reverse();
        after.validate()?;
        Ok(WorkflowChangePreviewV1 {
            inventory,
            before_state_hash: state_hash(&before.state)?,
            after_state_hash: state_hash(&after)?,
            before_workflow_hash: workflow_hash(&before.state)?,
            after_workflow_hash: workflow_hash(&after)?,
            before,
            candidate: next,
        })
    }

    /// Re-reads the full snapshot after confirmation.  A displayed preview
    /// cannot authorize a later generation or a substituted workflow state.
    pub fn prepare_confirmed(
        &self,
        session: &PersistedSession,
        preview: &WorkflowChangePreviewV1,
        operation_id: ManagementOperationIdV1,
    ) -> Result<WorkflowPreparedChangeV1, WorkflowChangeError> {
        self.validate_binding()?;
        let current = session.snapshot()?;
        if current.state.generation != preview.before.state.generation
            || state_hash(&current.state)? != preview.before_state_hash
            || current.raw_offset != preview.before.raw_offset
            || current.raw_hash != preview.before.raw_hash
        {
            return Err(WorkflowChangeError::StalePreview);
        }
        let descriptor = WorkflowDescriptorV1::from_preview(
            operation_id,
            preview,
            self.effects.project_scope_hash(),
        );
        let bytes = descriptor_bytes(&descriptor)?;
        let request = PrepareManagementEffectV1 {
            operation_id,
            inventory: preview.inventory.parse(),
            scope: ManagementEffectScopeV1 {
                project: self.effects.project_scope_hash(),
                session: Some(ManagementEffectHashV1::digest(
                    preview.before.state.session_id.as_bytes(),
                )),
                generation: preview.before.state.generation,
                revision: preview.before.state.workflow.revision,
                permission: ManagementPermissionScopeV1::LocalSession,
                account_binding: None,
            },
            request_hash: ManagementEffectHashV1::digest(&bytes),
            rewind: ManagementRewindDispositionV1::CapturedRestorable,
        };
        let prepared = match self
            .effects
            .prepare(self.lease, self.effects.cursor()?, request)?
        {
            PrepareManagementEffectOutcomeV1::Prepared(prepared) => prepared,
            PrepareManagementEffectOutcomeV1::Existing(_) => {
                return Err(WorkflowChangeError::OperationCollision);
            }
        };
        if let Err(error) = write_descriptor(self.state_dir, self.lease, &descriptor, self.effects)
        {
            let _ = self.effects.deny(
                self.lease,
                &prepared,
                ManagementDenialReasonV1::ResourceUnavailable,
            );
            return Err(error);
        }
        let read_back = match read_descriptor(
            self.state_dir,
            self.lease,
            &prepared,
            self.effects.project_scope_hash(),
        ) {
            Ok(found) => found,
            Err(error) => {
                let _ = self.effects.deny(
                    self.lease,
                    &prepared,
                    ManagementDenialReasonV1::ResourceUnavailable,
                );
                return Err(error);
            }
        };
        if read_back.as_bytes()? != bytes {
            let _ = self.effects.deny(
                self.lease,
                &prepared,
                ManagementDenialReasonV1::ResourceUnavailable,
            );
            return Err(WorkflowChangeError::DescriptorMismatch);
        }
        Ok(WorkflowPreparedChangeV1 {
            prepared,
            preview: preview.clone(),
        })
    }

    /// Starts one prepared effect, publishes through the dedicated session
    /// CAS, then records a receipt.  Any ambiguity after `start` remains a
    /// blocker and is never retried as another workflow publication.
    pub fn execute(
        &self,
        session: &PersistedSession,
        change: &WorkflowPreparedChangeV1,
    ) -> Result<ConversationSnapshot, WorkflowChangeError> {
        self.validate_binding()?;
        let descriptor = read_descriptor(
            self.state_dir,
            self.lease,
            &change.prepared,
            self.effects.project_scope_hash(),
        )?;
        if descriptor
            != WorkflowDescriptorV1::from_preview(
                change.prepared.operation_id(),
                &change.preview,
                self.effects.project_scope_hash(),
            )
        {
            return Err(WorkflowChangeError::DescriptorMismatch);
        }
        let started = self.effects.start(self.lease, &change.prepared)?;
        let published = match session.publish_workflow_change(
            change.preview.before.state.generation,
            &change.preview.before_state_hash,
            change.preview.candidate.clone(),
        ) {
            Ok(snapshot) => snapshot,
            Err(error) => {
                let _ = self.effects.mark_outcome_unknown(
                    self.lease,
                    &started,
                    ManagementUnknownReasonV1::InterruptedAfterStart,
                );
                return Err(error.into());
            }
        };
        if state_hash(&published.state)? != change.preview.after_state_hash
            || workflow_hash(&published.state)? != change.preview.after_workflow_hash
            || published.raw_offset != change.preview.before.raw_offset
            || published.raw_hash != change.preview.before.raw_hash
        {
            let _ = self.effects.mark_outcome_unknown(
                self.lease,
                &started,
                ManagementUnknownReasonV1::ReceiptPersistenceFailed,
            );
            return Err(WorkflowChangeError::DescriptorMismatch);
        }
        let evidence = receipt_hash("completed", &started, &published)?;
        let completion = complete_started_receipt(self.effects, self.lease, &started, evidence);
        match completed_receipt_state(
            self.effects.effect(change.prepared.operation_id())?,
            change.prepared.operation_id(),
            evidence,
        )? {
            CompletedReceiptStateV1::Completed => {}
            CompletedReceiptStateV1::EffectStarted => {
                let _ = self.effects.mark_outcome_unknown(
                    self.lease,
                    &started,
                    ManagementUnknownReasonV1::ReceiptPersistenceFailed,
                );
                return match completion {
                    Err(error) => Err(error.into()),
                    Ok(_) => Err(WorkflowChangeError::DescriptorMismatch),
                };
            }
            // A prior failed receipt transition may already have persisted an
            // unknown outcome.  Keep it as the restart blocker; never turn it
            // into a successful in-memory workflow update.
            CompletedReceiptStateV1::OutcomeUnknown => {
                return match completion {
                    Err(error) => Err(error.into()),
                    Ok(_) => Err(WorkflowChangeError::DescriptorMismatch),
                };
            }
        }
        // The completed receipt is already durable.  A failed cleanup leaves
        // evidence for terminal-only GC; it must not make the caller retain a
        // stale in-memory workflow after the marker was published.
        let _ = remove_descriptor(self.state_dir, self.lease, change.prepared.operation_id());
        Ok(published)
    }

    /// Reconciles only T03/T04/F15 records.  It never calls the session
    /// publish API: an unknown record is completed or abandoned from durable
    /// state, otherwise it remains a blocker.
    pub fn recover(
        &self,
        data_root: &Path,
        database: &Path,
        project_id: &str,
    ) -> Result<(), WorkflowChangeError> {
        self.validate_binding()?;
        let Some(effect) = self.effects.blocker()? else {
            return Ok(());
        };
        if !matches!(
            effect.request.inventory.encoded().as_str(),
            "T03" | "T04" | "F15"
        ) {
            return Ok(());
        }
        // `prepare` is durable before descriptor creation.  It has not
        // started the workflow mutation, so a restart safely denies it even
        // when descriptor creation was interrupted or never began.
        if matches!(effect.state, ManagementEffectStateV1::Prepared) {
            let prepared = self
                .effects
                .prepared_effect(effect.request.operation_id)?
                .ok_or(WorkflowChangeError::DescriptorMismatch)?;
            self.effects.deny(
                self.lease,
                &prepared,
                ManagementDenialReasonV1::ResourceUnavailable,
            )?;
            let _ = remove_descriptor(self.state_dir, self.lease, effect.request.operation_id);
            return Ok(());
        }
        let descriptor = read_descriptor_effect(
            self.state_dir,
            self.lease,
            &effect,
            self.effects.project_scope_hash(),
        )?;
        let session =
            PersistedSession::open(data_root, database, project_id, &descriptor.session_id)?;
        let snapshot = session.snapshot()?;
        if snapshot.state.project_id != project_id
            || snapshot.state.session_id != descriptor.session_id
        {
            return Err(WorkflowChangeError::DescriptorMismatch);
        }
        match effect.state {
            ManagementEffectStateV1::OutcomeUnknown { .. } => {
                let unknown = self
                    .effects
                    .unknown_effect(effect.request.operation_id)?
                    .ok_or(WorkflowChangeError::DescriptorMismatch)?;
                if descriptor.matches_after(&snapshot)? {
                    let evidence = receipt_hash_unknown("completed", &unknown, &snapshot)?;
                    self.effects
                        .complete_unknown(self.lease, &unknown, evidence)?;
                    remove_descriptor(self.state_dir, self.lease, effect.request.operation_id)?;
                } else if descriptor.matches_before(&snapshot)? {
                    let evidence = receipt_hash_unknown("abandoned", &unknown, &snapshot)?;
                    self.effects
                        .reconcile_abandon(self.lease, &unknown, evidence)?;
                    remove_descriptor(self.state_dir, self.lease, effect.request.operation_id)?;
                } else {
                    return Err(WorkflowChangeError::DescriptorMismatch);
                }
            }
            _ => return Err(WorkflowChangeError::DescriptorMismatch),
        }
        Ok(())
    }
}

fn complete_started_receipt(
    effects: &ManagementEffectStoreV1,
    lease: &ProjectMutationLeaseV1,
    started: &StartedManagementEffectV1,
    evidence: ManagementEffectHashV1,
) -> Result<super::CompletedManagementEffectV1, ManagementEffectStoreError> {
    #[cfg(test)]
    let fault = take_receipt_completion_fault();
    #[cfg(test)]
    if fault == ReceiptCompletionFaultV1::BeforeAppend {
        return Err(std::io::Error::other("injected receipt failure before append").into());
    }
    let completed = effects.complete_started(lease, started, evidence)?;
    #[cfg(test)]
    if fault == ReceiptCompletionFaultV1::AfterAppend {
        return Err(std::io::Error::other("injected receipt failure after append").into());
    }
    Ok(completed)
}

#[cfg(test)]
#[derive(Clone, Copy, Eq, PartialEq)]
enum ReceiptCompletionFaultV1 {
    None,
    BeforeAppend,
    AfterAppend,
}

#[cfg(test)]
thread_local! {
    static RECEIPT_COMPLETION_FAULT: std::cell::Cell<ReceiptCompletionFaultV1> = const {
        std::cell::Cell::new(ReceiptCompletionFaultV1::None)
    };
}

#[cfg(test)]
fn set_receipt_completion_fault(fault: ReceiptCompletionFaultV1) {
    RECEIPT_COMPLETION_FAULT.with(|current| current.set(fault));
}

#[cfg(test)]
fn take_receipt_completion_fault() -> ReceiptCompletionFaultV1 {
    RECEIPT_COMPLETION_FAULT.with(|current| current.replace(ReceiptCompletionFaultV1::None))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CompletedReceiptStateV1 {
    Completed,
    EffectStarted,
    OutcomeUnknown,
}

/// Checks the durable terminal record before callers update their in-memory
/// workflow.  The operation ID is repeated here so an invalid journal lookup
/// cannot make a different record authorize this publication.
fn completed_receipt_state(
    effect: Option<super::ManagementEffectSnapshotV1>,
    operation_id: ManagementOperationIdV1,
    evidence: ManagementEffectHashV1,
) -> Result<CompletedReceiptStateV1, WorkflowChangeError> {
    let effect = effect.ok_or(WorkflowChangeError::DescriptorMismatch)?;
    if effect.request.operation_id != operation_id {
        return Err(WorkflowChangeError::DescriptorMismatch);
    }
    match effect.state {
        ManagementEffectStateV1::Completed { evidence_hash } if evidence_hash == evidence => {
            Ok(CompletedReceiptStateV1::Completed)
        }
        ManagementEffectStateV1::EffectStarted => Ok(CompletedReceiptStateV1::EffectStarted),
        ManagementEffectStateV1::OutcomeUnknown { .. } => {
            Ok(CompletedReceiptStateV1::OutcomeUnknown)
        }
        _ => Err(WorkflowChangeError::DescriptorMismatch),
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkflowDescriptorV1 {
    version: u8,
    operation_id: ManagementOperationIdV1,
    inventory: String,
    project_hash: ManagementEffectHashV1,
    session_id: String,
    before_generation: u64,
    before_workflow_revision: u64,
    before_epoch: u64,
    before_state_hash: String,
    before_raw_offset: u64,
    before_raw_hash: String,
    before_workflow_hash: ManagementEffectHashV1,
    after_generation: u64,
    after_state_hash: String,
    after_workflow_hash: ManagementEffectHashV1,
}

impl WorkflowDescriptorV1 {
    fn from_preview(
        operation_id: ManagementOperationIdV1,
        preview: &WorkflowChangePreviewV1,
        project_hash: ManagementEffectHashV1,
    ) -> Self {
        Self {
            version: 1,
            operation_id,
            inventory: preview.inventory.id().into(),
            project_hash,
            session_id: preview.before.state.session_id.clone(),
            before_generation: preview.before.state.generation,
            before_workflow_revision: preview.before.state.workflow.revision,
            before_epoch: preview.before.state.epoch,
            before_state_hash: preview.before_state_hash.clone(),
            before_raw_offset: preview.before.raw_offset,
            before_raw_hash: preview.before.raw_hash.clone(),
            before_workflow_hash: preview.before_workflow_hash,
            after_generation: preview
                .before
                .state
                .generation
                .checked_add(1)
                .expect("preview rejects generation overflow"),
            after_state_hash: preview.after_state_hash.clone(),
            after_workflow_hash: preview.after_workflow_hash,
        }
    }

    fn as_bytes(&self) -> Result<Vec<u8>, WorkflowChangeError> {
        descriptor_bytes(self)
    }

    fn matches_before(&self, snapshot: &ConversationSnapshot) -> Result<bool, WorkflowChangeError> {
        Ok(snapshot.state.generation == self.before_generation
            && snapshot.state.epoch == self.before_epoch
            && state_hash(&snapshot.state)? == self.before_state_hash
            && workflow_hash(&snapshot.state)? == self.before_workflow_hash
            && snapshot.raw_offset == self.before_raw_offset
            && snapshot.raw_hash == self.before_raw_hash)
    }

    fn matches_after(&self, snapshot: &ConversationSnapshot) -> Result<bool, WorkflowChangeError> {
        Ok(snapshot.state.generation == self.after_generation
            && state_hash(&snapshot.state)? == self.after_state_hash
            && workflow_hash(&snapshot.state)? == self.after_workflow_hash
            && snapshot.raw_offset == self.before_raw_offset
            && snapshot.raw_hash == self.before_raw_hash)
    }
}

#[derive(Serialize)]
#[serde(deny_unknown_fields)]
struct WorkflowReceiptV1<'a> {
    version: u8,
    disposition: &'a str,
    operation_id: ManagementOperationIdV1,
    request_hash: ManagementEffectHashV1,
    session_id: &'a str,
    generation: u64,
    state_hash: String,
    raw_offset: u64,
    raw_hash: &'a str,
}

fn receipt_hash(
    disposition: &str,
    effect: &StartedManagementEffectV1,
    snapshot: &ConversationSnapshot,
) -> Result<ManagementEffectHashV1, WorkflowChangeError> {
    receipt_hash_parts(
        disposition,
        effect.operation_id(),
        effect.request().request_hash,
        snapshot,
    )
}

fn receipt_hash_unknown(
    disposition: &str,
    effect: &super::UnknownManagementEffectV1,
    snapshot: &ConversationSnapshot,
) -> Result<ManagementEffectHashV1, WorkflowChangeError> {
    receipt_hash_parts(
        disposition,
        effect.operation_id(),
        effect.request().request_hash,
        snapshot,
    )
}

fn receipt_hash_parts(
    disposition: &str,
    operation_id: ManagementOperationIdV1,
    request_hash: ManagementEffectHashV1,
    snapshot: &ConversationSnapshot,
) -> Result<ManagementEffectHashV1, WorkflowChangeError> {
    let receipt = WorkflowReceiptV1 {
        version: 1,
        disposition,
        operation_id,
        request_hash,
        session_id: &snapshot.state.session_id,
        generation: snapshot.state.generation,
        state_hash: state_hash(&snapshot.state)?,
        raw_offset: snapshot.raw_offset,
        raw_hash: &snapshot.raw_hash,
    };
    Ok(ManagementEffectHashV1::digest(&serde_json::to_vec(
        &receipt,
    )?))
}

fn state_hash(state: &ConversationStateV2) -> Result<String, WorkflowChangeError> {
    Ok(content_hash(&serde_json::to_vec(state)?))
}

fn workflow_hash(
    state: &ConversationStateV2,
) -> Result<ManagementEffectHashV1, WorkflowChangeError> {
    Ok(ManagementEffectHashV1::digest(&serde_json::to_vec(&(
        &state.workflow_config,
        &state.workflow,
        &state.gates,
        &state.workflow_artifacts,
    ))?))
}

fn descriptor_bytes(descriptor: &WorkflowDescriptorV1) -> Result<Vec<u8>, WorkflowChangeError> {
    let bytes = serde_json::to_vec(descriptor)?;
    if bytes.len() > MAX_DESCRIPTOR_BYTES {
        return Err(io::Error::other("workflow descriptor exceeds 16KiB").into());
    }
    Ok(bytes)
}

fn descriptor_root(
    state_dir: &Path,
    lease: &ProjectMutationLeaseV1,
) -> Result<File, WorkflowChangeError> {
    let parent = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(state_dir)?;
    let parent_meta = parent.metadata()?;
    if !parent_meta.is_dir() || parent_meta.uid() != unsafe { libc::geteuid() } {
        return Err(WorkflowChangeError::DescriptorMismatch);
    }
    let lease_name = CString::new("rewind-workspace").expect("fixed lease directory");
    let lease_fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            lease_name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if lease_fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let lease_directory = unsafe { File::from_raw_fd(lease_fd) };
    let lease_meta = lease_directory.metadata()?;
    if (lease_meta.dev(), lease_meta.ino()) != lease.store_identity() {
        return Err(WorkflowChangeError::DescriptorMismatch);
    }
    let name = CString::new(DIRECTORY).expect("fixed descriptor directory");
    if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o700) } == 0 {
        parent.sync_all()?;
    } else if io::Error::last_os_error().kind() != io::ErrorKind::AlreadyExists {
        return Err(io::Error::last_os_error().into());
    }
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let directory = unsafe { File::from_raw_fd(fd) };
    let meta = directory.metadata()?;
    if !meta.is_dir()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.mode() & 0o7077 != 0
        || meta.mode() & 0o700 != 0o700
    {
        return Err(WorkflowChangeError::DescriptorMismatch);
    }
    Ok(directory)
}

fn descriptor_file(
    directory: &File,
    operation_id: ManagementOperationIdV1,
    flags: libc::c_int,
) -> io::Result<File> {
    let name =
        CString::new(format!("{}.json", operation_id.as_hex())).expect("hex descriptor name");
    let fd = unsafe {
        libc::openat(
            directory.as_raw_fd(),
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { File::from_raw_fd(fd) })
    }
}

fn descriptor_entries(directory: &File) -> Result<Vec<String>, WorkflowChangeError> {
    let fd = unsafe { libc::dup(directory.as_raw_fd()) };
    if fd < 0 {
        return Err(io::Error::last_os_error().into());
    }
    let stream = unsafe { libc::fdopendir(fd) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(error.into());
    }
    unsafe { libc::rewinddir(stream) };
    let result = (|| {
        let mut names = Vec::new();
        loop {
            #[cfg(target_os = "linux")]
            unsafe {
                *libc::__errno_location() = 0
            };
            #[cfg(target_os = "macos")]
            unsafe {
                *libc::__error() = 0
            };
            let entry = unsafe { libc::readdir(stream) };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                if error.raw_os_error().unwrap_or(0) != 0 {
                    return Err(error.into());
                }
                break;
            }
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }
                .to_str()
                .map_err(|_| WorkflowChangeError::DescriptorMismatch)?;
            if name != "." && name != ".." {
                names.push(name.to_owned());
            }
        }
        Ok(names)
    })();
    if unsafe { libc::closedir(stream) } != 0 {
        return Err(io::Error::last_os_error().into());
    }
    result
}

fn descriptor_entry_stat(directory: &File, name: &str) -> Result<libc::stat, WorkflowChangeError> {
    let name = CString::new(name).map_err(|_| WorkflowChangeError::DescriptorMismatch)?;
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe {
        libc::fstatat(
            directory.as_raw_fd(),
            name.as_ptr(),
            stat.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    } != 0
    {
        return Err(io::Error::last_os_error().into());
    }
    let stat = unsafe { stat.assume_init() };
    if stat.st_mode & libc::S_IFMT != libc::S_IFREG
        || stat.st_uid != unsafe { libc::geteuid() }
        || stat.st_nlink != 1
        || stat.st_mode & 0o7177 != 0
        || stat.st_size < 0
    {
        return Err(WorkflowChangeError::DescriptorMismatch);
    }
    Ok(stat)
}

fn gc_terminal(
    directory: &File,
    effects: &ManagementEffectStoreV1,
) -> Result<(), WorkflowChangeError> {
    for name in descriptor_entries(directory)? {
        descriptor_entry_stat(directory, &name)?;
        let Some(stem) = name.strip_suffix(".json") else {
            continue;
        };
        let Ok(operation_id) = stem.parse::<ManagementOperationIdV1>() else {
            continue;
        };
        if effects
            .effect(operation_id)?
            .is_some_and(|effect| effect.state.is_terminal())
        {
            let name = CString::new(name).expect("validated descriptor filename");
            if unsafe { libc::unlinkat(directory.as_raw_fd(), name.as_ptr(), 0) } != 0 {
                return Err(io::Error::last_os_error().into());
            }
        }
    }
    directory.sync_all()?;
    Ok(())
}

fn ensure_capacity(
    directory: &File,
    effects: &ManagementEffectStoreV1,
    incoming_bytes: usize,
) -> Result<(), WorkflowChangeError> {
    gc_terminal(directory, effects)?;
    let mut count = 0usize;
    let mut total = 0u64;
    for name in descriptor_entries(directory)? {
        let meta = descriptor_entry_stat(directory, &name)?;
        count = count
            .checked_add(1)
            .ok_or_else(|| io::Error::other("descriptor count overflow"))?;
        total = total
            .checked_add(meta.st_size as u64)
            .ok_or_else(|| io::Error::other("descriptor size overflow"))?;
    }
    if count >= MAX_DESCRIPTORS
        || total
            .checked_add(incoming_bytes as u64)
            .is_none_or(|next| next > MAX_DESCRIPTOR_TOTAL_BYTES)
    {
        return Err(io::Error::other("workflow descriptor capacity exhausted").into());
    }
    Ok(())
}

fn write_descriptor(
    state_dir: &Path,
    lease: &ProjectMutationLeaseV1,
    descriptor: &WorkflowDescriptorV1,
    effects: &ManagementEffectStoreV1,
) -> Result<(), WorkflowChangeError> {
    let root = descriptor_root(state_dir, lease)?;
    let bytes = descriptor_bytes(descriptor)?;
    ensure_capacity(&root, effects, bytes.len())?;
    let mut file = descriptor_file(
        &root,
        descriptor.operation_id,
        libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
    )?;
    file.write_all(&bytes)?;
    file.sync_all()?;
    let meta = file.metadata()?;
    if !meta.is_file()
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.nlink() != 1
        || meta.mode() & 0o7177 != 0
    {
        return Err(WorkflowChangeError::DescriptorMismatch);
    }
    root.sync_all()?;
    Ok(())
}

fn read_descriptor(
    state_dir: &Path,
    lease: &ProjectMutationLeaseV1,
    prepared: &PreparedManagementEffectV1,
    project: ManagementEffectHashV1,
) -> Result<WorkflowDescriptorV1, WorkflowChangeError> {
    let effect = super::ManagementEffectSnapshotV1 {
        request: prepared.request().clone(),
        state: ManagementEffectStateV1::Prepared,
        last_sequence: prepared.last_sequence(),
    };
    read_descriptor_effect(state_dir, lease, &effect, project)
}

fn read_descriptor_effect(
    state_dir: &Path,
    lease: &ProjectMutationLeaseV1,
    effect: &super::ManagementEffectSnapshotV1,
    project: ManagementEffectHashV1,
) -> Result<WorkflowDescriptorV1, WorkflowChangeError> {
    let root = descriptor_root(state_dir, lease)?;
    let file = descriptor_file(
        &root,
        effect.request.operation_id,
        libc::O_RDONLY | libc::O_NOFOLLOW,
    )?;
    let meta = file.metadata()?;
    if !meta.file_type().is_file()
        || meta.file_type().is_symlink()
        || meta.len() > MAX_DESCRIPTOR_BYTES as u64
        || meta.uid() != unsafe { libc::geteuid() }
        || meta.nlink() != 1
        || meta.mode() & 0o7177 != 0
    {
        return Err(WorkflowChangeError::DescriptorMismatch);
    }
    let mut bytes = Vec::new();
    file.take((MAX_DESCRIPTOR_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let descriptor: WorkflowDescriptorV1 = serde_json::from_slice(&bytes)?;
    if bytes.len() > MAX_DESCRIPTOR_BYTES
        || descriptor_bytes(&descriptor)? != bytes
        || descriptor.version != 1
        || descriptor.operation_id != effect.request.operation_id
        || descriptor.inventory != effect.request.inventory.encoded()
        || descriptor.project_hash != project
        || descriptor.project_hash != effect.request.scope.project
        || ManagementEffectHashV1::digest(descriptor.session_id.as_bytes())
            != effect
                .request
                .scope
                .session
                .ok_or(WorkflowChangeError::DescriptorMismatch)?
        || descriptor.before_generation != effect.request.scope.generation
        || descriptor.before_workflow_revision != effect.request.scope.revision
        || effect.request.request_hash != ManagementEffectHashV1::digest(&bytes)
    {
        return Err(WorkflowChangeError::DescriptorMismatch);
    }
    Ok(descriptor)
}

fn remove_descriptor(
    state_dir: &Path,
    lease: &ProjectMutationLeaseV1,
    operation_id: ManagementOperationIdV1,
) -> Result<(), WorkflowChangeError> {
    let root = descriptor_root(state_dir, lease)?;
    let name =
        CString::new(format!("{}.json", operation_id.as_hex())).expect("hex descriptor name");
    if unsafe { libc::unlinkat(root.as_raw_fd(), name.as_ptr(), 0) } == 0 {
        root.sync_all()?;
    } else if io::Error::last_os_error().kind() != io::ErrorKind::NotFound {
        return Err(io::Error::last_os_error().into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        conversation_state::HistoryMode,
        management_effect::ManagementEffectSnapshotV1,
        project_mutation::ProjectMutationOwnerV1,
        workflow::{Phase, WorkflowConfig, WorkflowMode},
    };
    use polaris_provider::Message;

    struct WorkflowFixture {
        _root: tempfile::TempDir,
        _mutation_owner: ProjectMutationOwnerV1,
        project: std::path::PathBuf,
        state: std::path::PathBuf,
        data: std::path::PathBuf,
        lease: ProjectMutationLeaseV1,
        effects: ManagementEffectStoreV1,
    }

    impl WorkflowFixture {
        fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let project = root.path().join("project");
            let state = project.join(".polaris");
            let data = root.path().join("data");
            fs::create_dir(&project).unwrap();
            fs::create_dir(&state).unwrap();
            fs::create_dir(&data).unwrap();
            let mutation_owner =
                ProjectMutationOwnerV1::at_store_root(&state.join("rewind-workspace")).unwrap();
            let lease = mutation_owner.try_mutation().unwrap();
            let effects =
                ManagementEffectStoreV1::open_at_state_dir(&project, &state, &lease).unwrap();
            Self {
                _root: root,
                _mutation_owner: mutation_owner,
                project,
                state,
                data,
                lease,
                effects,
            }
        }

        fn owner(&self) -> WorkflowChangeOwner<'_> {
            WorkflowChangeOwner {
                effects: &self.effects,
                lease: &self.lease,
                state_dir: &self.state,
            }
        }
    }

    #[test]
    fn descriptor_directory_is_private_and_fd_stays_on_the_original_directory() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let parent = tempfile::tempdir().unwrap();
        let state = parent.path().join("state");
        fs::create_dir(&state).unwrap();
        let owner = crate::project_mutation::ProjectMutationOwnerV1::at_store_root(
            &state.join("rewind-workspace"),
        )
        .unwrap();
        let lease = owner.try_mutation().unwrap();
        let directory = descriptor_root(&state, &lease).unwrap();
        assert_eq!(
            directory.metadata().unwrap().permissions().mode() & 0o777,
            0o700
        );
        let operation_id = ManagementOperationIdV1::random().unwrap();
        let mut file = descriptor_file(
            &directory,
            operation_id,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
        )
        .unwrap();
        file.write_all(b"original").unwrap();
        file.sync_all().unwrap();
        let name = format!("{}.json", operation_id.as_hex());
        assert_eq!(descriptor_entries(&directory).unwrap(), vec![name.clone()]);
        assert_eq!(descriptor_entries(&directory).unwrap(), vec![name]);

        let original = state.join(DIRECTORY);
        let moved = state.join("moved-descriptors");
        fs::rename(&original, &moved).unwrap();
        let replacement = state.join("replacement");
        fs::create_dir(&replacement).unwrap();
        symlink(&replacement, &original).unwrap();
        assert!(descriptor_root(&state, &lease).is_err());
        let mut opened =
            descriptor_file(&directory, operation_id, libc::O_RDONLY | libc::O_NOFOLLOW).unwrap();
        let mut bytes = Vec::new();
        opened.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"original");
        assert!(fs::read_dir(&replacement).unwrap().next().is_none());

        let old_state = parent.path().join("old-state");
        fs::rename(&state, &old_state).unwrap();
        fs::create_dir(&state).unwrap();
        fs::create_dir(state.join("rewind-workspace")).unwrap();
        assert!(descriptor_root(&state, &lease).is_err());
    }

    #[test]
    fn preview_hash_matches_publish_normalization_after_a_real_turn() {
        let root = tempfile::tempdir().unwrap();
        let workflow = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            initial_phase: Phase::Specify,
            ..WorkflowConfig::default()
        });
        let session = PersistedSession::create(
            root.path(),
            &root.path().join("memory.sqlite3"),
            "project",
            HistoryMode::Legacy,
            Some(&workflow),
        )
        .unwrap();
        session
            .append(Message::user("real turn"), true, Some(&workflow))
            .unwrap();
        let mut candidate_config = workflow.config.clone();
        candidate_config.mode = WorkflowMode::Adaptive;
        let candidate = SessionWorkflow::restore_with_artifacts(
            candidate_config,
            workflow.gates.clone(),
            workflow.state(),
            workflow.artifact_graph().clone(),
        )
        .unwrap();
        let preview =
            WorkflowChangeOwner::preview(&session, WorkflowEffectInventoryV1::T03, &candidate)
                .unwrap();
        let descriptor = WorkflowDescriptorV1::from_preview(
            "00000000000000000000000000000001".parse().unwrap(),
            &preview,
            ManagementEffectHashV1::digest(b"project"),
        );
        assert_eq!(
            descriptor.after_generation,
            preview.before_generation().checked_add(1).unwrap()
        );
        let published = session
            .publish_workflow_change(
                preview.before_generation(),
                preview.before_state_hash(),
                preview.candidate.clone(),
            )
            .unwrap();
        assert_eq!(
            state_hash(&published.state).unwrap(),
            preview.after_state_hash()
        );
        assert_eq!(published.state.recent_turn_ids, vec![1]);
    }

    #[test]
    fn completed_receipt_readback_requires_matching_operation_and_evidence() {
        let operation_id = "00000000000000000000000000000001".parse().unwrap();
        let request = PrepareManagementEffectV1 {
            operation_id,
            inventory: ManagementInventoryIdV1::parse("T03").unwrap(),
            scope: ManagementEffectScopeV1 {
                project: ManagementEffectHashV1::digest(b"project"),
                session: Some(ManagementEffectHashV1::digest(b"session")),
                generation: 4,
                revision: 2,
                permission: ManagementPermissionScopeV1::LocalSession,
                account_binding: None,
            },
            request_hash: ManagementEffectHashV1::digest(b"request"),
            rewind: ManagementRewindDispositionV1::CapturedRestorable,
        };
        let evidence = ManagementEffectHashV1::digest(b"receipt");
        let completed = ManagementEffectSnapshotV1 {
            request: request.clone(),
            state: ManagementEffectStateV1::Completed {
                evidence_hash: evidence,
            },
            last_sequence: 3,
        };
        assert_eq!(
            completed_receipt_state(Some(completed.clone()), operation_id, evidence).unwrap(),
            CompletedReceiptStateV1::Completed
        );
        assert!(
            completed_receipt_state(
                Some(completed),
                operation_id,
                ManagementEffectHashV1::digest(b"different receipt"),
            )
            .is_err()
        );
        let different_operation = "00000000000000000000000000000002".parse().unwrap();
        let mismatched_operation = ManagementEffectSnapshotV1 {
            request: PrepareManagementEffectV1 {
                operation_id: different_operation,
                ..request.clone()
            },
            state: ManagementEffectStateV1::Completed {
                evidence_hash: evidence,
            },
            last_sequence: 3,
        };
        assert!(
            completed_receipt_state(Some(mismatched_operation), operation_id, evidence).is_err()
        );

        let started = ManagementEffectSnapshotV1 {
            request: request.clone(),
            state: ManagementEffectStateV1::EffectStarted,
            last_sequence: 2,
        };
        assert_eq!(
            completed_receipt_state(Some(started), operation_id, evidence).unwrap(),
            CompletedReceiptStateV1::EffectStarted
        );
        let unknown = ManagementEffectSnapshotV1 {
            request,
            state: ManagementEffectStateV1::OutcomeUnknown {
                reason: ManagementUnknownReasonV1::ReceiptPersistenceFailed,
            },
            last_sequence: 3,
        };
        assert_eq!(
            completed_receipt_state(Some(unknown), operation_id, evidence).unwrap(),
            CompletedReceiptStateV1::OutcomeUnknown
        );
    }

    #[test]
    fn execute_publishes_only_after_the_completed_receipt_is_read_back() {
        let fixture = WorkflowFixture::new();
        let workflow = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            initial_phase: Phase::Specify,
            ..WorkflowConfig::default()
        });
        let session = PersistedSession::create(
            &fixture.data,
            &fixture.state.join("memory.sqlite3"),
            fixture.project.to_str().unwrap(),
            HistoryMode::Legacy,
            Some(&workflow),
        )
        .unwrap();
        let mut candidate_config = workflow.config.clone();
        candidate_config.mode = WorkflowMode::Adaptive;
        let candidate = SessionWorkflow::restore_with_artifacts(
            candidate_config,
            workflow.gates.clone(),
            workflow.state(),
            workflow.artifact_graph().clone(),
        )
        .unwrap();
        let preview =
            WorkflowChangeOwner::preview(&session, WorkflowEffectInventoryV1::T03, &candidate)
                .unwrap();
        let operation_id = "00000000000000000000000000000003".parse().unwrap();
        let change = fixture
            .owner()
            .prepare_confirmed(&session, &preview, operation_id)
            .unwrap();
        let published = fixture.owner().execute(&session, &change).unwrap();
        let evidence = receipt_hash_parts(
            "completed",
            operation_id,
            change.prepared.request().request_hash,
            &published,
        )
        .unwrap();
        assert!(matches!(
            fixture.effects.effect(operation_id).unwrap().unwrap().state,
            ManagementEffectStateV1::Completed { evidence_hash } if evidence_hash == evidence
        ));
        assert!(
            !fixture
                .state
                .join(DIRECTORY)
                .join(format!("{}.json", operation_id.as_hex()))
                .exists()
        );
    }

    #[test]
    fn receipt_failure_before_append_keeps_unknown_blocker_and_descriptor() {
        let fixture = WorkflowFixture::new();
        let workflow = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            initial_phase: Phase::Specify,
            ..WorkflowConfig::default()
        });
        let session = PersistedSession::create(
            &fixture.data,
            &fixture.state.join("memory.sqlite3"),
            fixture.project.to_str().unwrap(),
            HistoryMode::Legacy,
            Some(&workflow),
        )
        .unwrap();
        let mut candidate_config = workflow.config.clone();
        candidate_config.mode = WorkflowMode::Adaptive;
        let candidate = SessionWorkflow::restore_with_artifacts(
            candidate_config,
            workflow.gates.clone(),
            workflow.state(),
            workflow.artifact_graph().clone(),
        )
        .unwrap();
        let preview =
            WorkflowChangeOwner::preview(&session, WorkflowEffectInventoryV1::T03, &candidate)
                .unwrap();
        let operation_id = "00000000000000000000000000000005".parse().unwrap();
        let change = fixture
            .owner()
            .prepare_confirmed(&session, &preview, operation_id)
            .unwrap();
        set_receipt_completion_fault(ReceiptCompletionFaultV1::BeforeAppend);
        assert!(fixture.owner().execute(&session, &change).is_err());
        assert!(matches!(
            fixture.effects.effect(operation_id).unwrap().unwrap().state,
            ManagementEffectStateV1::OutcomeUnknown {
                reason: ManagementUnknownReasonV1::ReceiptPersistenceFailed
            }
        ));
        assert!(fixture.effects.blocker().unwrap().is_some());
        assert!(
            fixture
                .state
                .join(DIRECTORY)
                .join(format!("{}.json", operation_id.as_hex()))
                .exists()
        );
    }

    #[test]
    fn receipt_error_after_durable_completion_returns_success_from_readback() {
        let fixture = WorkflowFixture::new();
        let workflow = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            initial_phase: Phase::Specify,
            ..WorkflowConfig::default()
        });
        let session = PersistedSession::create(
            &fixture.data,
            &fixture.state.join("memory.sqlite3"),
            fixture.project.to_str().unwrap(),
            HistoryMode::Legacy,
            Some(&workflow),
        )
        .unwrap();
        let mut candidate_config = workflow.config.clone();
        candidate_config.mode = WorkflowMode::Adaptive;
        let candidate = SessionWorkflow::restore_with_artifacts(
            candidate_config,
            workflow.gates.clone(),
            workflow.state(),
            workflow.artifact_graph().clone(),
        )
        .unwrap();
        let preview =
            WorkflowChangeOwner::preview(&session, WorkflowEffectInventoryV1::T03, &candidate)
                .unwrap();
        let operation_id = "00000000000000000000000000000006".parse().unwrap();
        let change = fixture
            .owner()
            .prepare_confirmed(&session, &preview, operation_id)
            .unwrap();
        set_receipt_completion_fault(ReceiptCompletionFaultV1::AfterAppend);
        assert!(fixture.owner().execute(&session, &change).is_ok());
        assert!(matches!(
            fixture.effects.effect(operation_id).unwrap().unwrap().state,
            ManagementEffectStateV1::Completed { .. }
        ));
        assert!(fixture.effects.blocker().unwrap().is_none());
    }

    #[test]
    fn restart_of_started_change_marks_unknown_then_reconciles_before_state() {
        let fixture = WorkflowFixture::new();
        let workflow = SessionWorkflow::new(WorkflowConfig {
            enabled: true,
            initial_phase: Phase::Specify,
            ..WorkflowConfig::default()
        });
        let session = PersistedSession::create(
            &fixture.data,
            &fixture.state.join("memory.sqlite3"),
            fixture.project.to_str().unwrap(),
            HistoryMode::Legacy,
            Some(&workflow),
        )
        .unwrap();
        let mut candidate_config = workflow.config.clone();
        candidate_config.mode = WorkflowMode::Adaptive;
        let candidate = SessionWorkflow::restore_with_artifacts(
            candidate_config,
            workflow.gates.clone(),
            workflow.state(),
            workflow.artifact_graph().clone(),
        )
        .unwrap();
        let preview =
            WorkflowChangeOwner::preview(&session, WorkflowEffectInventoryV1::T03, &candidate)
                .unwrap();
        let operation_id = "00000000000000000000000000000004".parse().unwrap();
        fixture
            .owner()
            .prepare_confirmed(&session, &preview, operation_id)
            .unwrap();
        let prepared = fixture
            .effects
            .prepared_effect(operation_id)
            .unwrap()
            .unwrap();
        fixture.effects.start(&fixture.lease, &prepared).unwrap();

        let restarted = ManagementEffectStoreV1::open_at_state_dir(
            &fixture.project,
            &fixture.state,
            &fixture.lease,
        )
        .unwrap();
        assert!(matches!(
            restarted.effect(operation_id).unwrap().unwrap().state,
            ManagementEffectStateV1::OutcomeUnknown { .. }
        ));
        WorkflowChangeOwner {
            effects: &restarted,
            lease: &fixture.lease,
            state_dir: &fixture.state,
        }
        .recover(
            &fixture.data,
            &fixture.state.join("memory.sqlite3"),
            fixture.project.to_str().unwrap(),
        )
        .unwrap();
        assert!(matches!(
            restarted.effect(operation_id).unwrap().unwrap().state,
            ManagementEffectStateV1::ReconciledAbandon { .. }
        ));
    }
}
